//! Feature-gated ADS-B receive path.
//!
//! This is the first native packet decoder beside the spectrum/audio paths:
//! RTL-SDR IQ in, Mode S preamble/bit demodulation, CRC validation, and a
//! bounded JSON event stream out. The RF and decoding work stays in Rust;
//! Soil/UI consumes only decoded aircraft events.

use std::collections::HashMap;
use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::{RadioDescriptor, SampleFormat};

const MODE_S_PREAMBLE_US: [f64; 8] = [0.0, 1.0, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5];
const MODE_S_BITS: usize = 112;
const MODE_S_PREAMBLE_US_LEN: f64 = 8.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdsbConfig {
    pub destination: SocketAddr,
    pub gain_db_tenths: u16,
}

impl AdsbConfig {
    pub fn parse(destination: &str, gain_db: f32) -> Result<Self, String> {
        let destination = destination
            .parse()
            .map_err(|error| format!("invalid ADS-B destination: {error}"))?;
        if !gain_db.is_finite() || !(0.0..=49.6).contains(&gain_db) {
            return Err("ADS-B gain must be between 0 and 49.6 dB".into());
        }
        Ok(Self {
            destination,
            gain_db_tenths: (gain_db * 10.0).round() as u16,
        })
    }

    fn gain_db(&self) -> f32 {
        f32::from(self.gain_db_tenths) / 10.0
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AircraftEvent {
    pub schema_version: u32,
    pub icao: String,
    pub raw_hex: String,
    pub observed_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altitude_ft: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    pub even: bool,
}

#[derive(Debug, Clone, Copy)]
struct Cpr {
    lat: u32,
    lon: u32,
    at_ms: u64,
}

#[derive(Default)]
struct CprPair {
    even: Option<Cpr>,
    odd: Option<Cpr>,
}

pub struct AdsbDecoder {
    positions: HashMap<u32, CprPair>,
}

impl Default for AdsbDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AdsbDecoder {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
        }
    }

    /// Decode one validated 14-byte Mode S frame.
    pub fn decode(&mut self, message: &[u8; 14], observed_at_ms: u64) -> Option<AircraftEvent> {
        if message.len() != 14 || (message[0] >> 3) != 17 {
            return None;
        }
        let icao = u32::from_be_bytes([0, message[1], message[2], message[3]]);
        let me = message[4..11]
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
        let type_code = (me >> 51) & 0x1f;
        let mut event = AircraftEvent {
            schema_version: 1,
            icao: format!("{icao:06X}"),
            raw_hex: message.iter().map(|byte| format!("{byte:02X}")).collect(),
            observed_at_ms,
            callsign: None,
            altitude_ft: None,
            latitude: None,
            longitude: None,
            even: false,
        };

        if (1..=4).contains(&type_code) {
            event.callsign = decode_callsign(me);
            return Some(event);
        }

        if (9..=18).contains(&type_code) {
            let altitude = decode_altitude((me >> 36) & 0xfff);
            let odd = ((me >> 34) & 1) != 0;
            let lat = ((me >> 17) & 0x1ffff) as u32;
            let lon = (me & 0x1ffff) as u32;
            event.altitude_ft = altitude;
            event.even = !odd;
            let pair = self.positions.entry(icao).or_default();
            let frame = Cpr {
                lat,
                lon,
                at_ms: observed_at_ms,
            };
            if odd {
                pair.odd = Some(frame);
            } else {
                pair.even = Some(frame);
            }
            if let (Some(even), Some(odd)) = (pair.even, pair.odd)
                && even.at_ms.abs_diff(odd.at_ms) <= 10_000
                && let Some((latitude, longitude)) = decode_cpr(even, odd, odd.at_ms >= even.at_ms)
            {
                event.latitude = Some(latitude);
                event.longitude = Some(longitude);
            }
            return Some(event);
        }

        Some(event)
    }
}

fn bits(me: u64, start: u32, count: u32) -> u64 {
    (me >> (56 - start - count)) & ((1_u64 << count) - 1)
}

fn decode_callsign(me: u64) -> Option<String> {
    const CHARS: &[u8] = b"#ABCDEFGHIJKLMNOPQRSTUVWXYZ#####_###############0123456789######";
    let mut result = String::new();
    for index in 0..8 {
        let code = bits(me, 8 + index * 6, 6) as usize;
        let character = *CHARS.get(code)?;
        if character == b'#' {
            return None;
        }
        result.push(char::from(character));
    }
    let result = result.trim_end_matches('_').trim().to_string();
    (!result.is_empty()).then_some(result)
}

fn decode_altitude(value: u64) -> Option<i32> {
    let q = (value >> 4) & 1;
    if q == 0 {
        return None;
    }
    let n = ((value >> 5) << 4) | (value & 0xf);
    Some(n as i32 * 25 - 1000)
}

fn cpr_nl(latitude: f64) -> i32 {
    let latitude = latitude.to_radians();
    let nz = 15.0;
    let a = 1.0 - (std::f64::consts::PI / (2.0 * nz)).cos();
    let b = latitude.cos().powi(2);
    let value = 1.0 - a / b;
    if !(-1.0..=1.0).contains(&value) {
        return 1;
    }
    (2.0 * std::f64::consts::PI / value.acos()).floor() as i32
}

fn decode_cpr(even: Cpr, odd: Cpr, odd_is_latest: bool) -> Option<(f64, f64)> {
    let lat_even = f64::from(even.lat) / 131_072.0;
    let lat_odd = f64::from(odd.lat) / 131_072.0;
    let j = (59.0 * lat_even - 60.0 * lat_odd + 0.5).floor();
    let mut latitude_even = 6.0 * (j.rem_euclid(60.0) + lat_even);
    let mut latitude_odd = (360.0 / 59.0) * (j.rem_euclid(59.0) + lat_odd);
    if latitude_even >= 270.0 {
        latitude_even -= 360.0;
    }
    if latitude_odd >= 270.0 {
        latitude_odd -= 360.0;
    }
    if cpr_nl(latitude_even) != cpr_nl(latitude_odd) {
        return None;
    }
    let nl = cpr_nl(latitude_even);
    let (latitude, ni, lon_even, lon_odd, lon_use) = if odd_is_latest {
        (
            latitude_odd,
            (nl - 1).max(1),
            f64::from(even.lon) / 131_072.0,
            f64::from(odd.lon) / 131_072.0,
            f64::from(odd.lon) / 131_072.0,
        )
    } else {
        (
            latitude_even,
            nl.max(1),
            f64::from(even.lon) / 131_072.0,
            f64::from(odd.lon) / 131_072.0,
            f64::from(even.lon) / 131_072.0,
        )
    };
    let m = (lon_even * f64::from(nl - 1) - lon_odd * f64::from(nl) + 0.5).floor();
    let mut longitude = (360.0 / f64::from(ni)) * (m.rem_euclid(f64::from(ni)) + lon_use);
    if longitude >= 180.0 {
        longitude -= 360.0;
    }
    Some((latitude, longitude))
}

pub struct Demodulator {
    sample_rate_hz: f64,
    buffer: Vec<f32>,
}

impl Demodulator {
    pub fn new(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz: f64::from(sample_rate_hz),
            buffer: Vec::new(),
        }
    }

    fn window_samples(&self) -> usize {
        ((MODE_S_PREAMBLE_US_LEN + MODE_S_BITS as f64) * self.sample_rate_hz / 1_000_000.0).ceil()
            as usize
    }

    pub fn push(&mut self, magnitudes: &[f32]) -> Vec<[u8; 14]> {
        self.buffer.extend_from_slice(magnitudes);
        let window = self.window_samples();
        let mut frames = Vec::new();
        let mut index = 0;
        while index + window <= self.buffer.len() {
            if let Some(message) = self.try_decode(index, window) {
                frames.push(message);
                index += window;
            } else {
                index += 1;
            }
        }
        if index > 0 {
            self.buffer.drain(..index);
        }
        frames
    }

    fn sample(&self, index: f64) -> Option<f32> {
        if !index.is_finite() || index < 0.0 {
            return None;
        }
        self.buffer.get(index.round() as usize).copied()
    }

    fn try_decode(&self, start: usize, window: usize) -> Option<[u8; 14]> {
        let rate = self.sample_rate_hz / 1_000_000.0;
        let local = &self.buffer[start..start + window];
        let mean = local.iter().copied().sum::<f32>() / local.len() as f32;
        if mean <= 0.0 {
            return None;
        }
        let mut preamble_sum = 0.0;
        for micros in MODE_S_PREAMBLE_US {
            let index = start + (micros * rate).round() as usize;
            preamble_sum += self.buffer.get(index).copied()?;
        }
        // The preamble is eight short pulses spanning 8 us. Its average is
        // well above the noise/background average of the whole message window.
        if preamble_sum < mean * 4.0 {
            return None;
        }
        let message_start = f64::from(start as u32) + MODE_S_PREAMBLE_US_LEN * rate;
        let mut bits = [0_u8; MODE_S_BITS];
        for bit in 0..MODE_S_BITS {
            let bit_start = message_start + bit as f64 * rate;
            let first = self.sample(bit_start + 0.25 * rate)?;
            let second = self.sample(bit_start + 0.75 * rate)?;
            bits[bit] = u8::from(first > second * 1.15);
        }
        let mut message = [0_u8; 14];
        for (index, bit) in bits.iter().enumerate() {
            message[index / 8] |= *bit << (7 - (index % 8));
        }
        (crc24(&message) == 0).then_some(message)
    }
}

fn crc24(message: &[u8; 14]) -> u32 {
    let mut crc = 0_u32;
    for byte in message {
        for bit in (0..8).rev() {
            let input = (byte >> bit) & 1;
            let top = (crc >> 23) & 1;
            crc = ((crc << 1) & 0xff_ffff) | u32::from(input);
            if top ^ u32::from(input) != 0 {
                crc ^= 0xFF_F409;
            }
        }
    }
    crc
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn magnitudes(bytes: &[u8], format: SampleFormat) -> Result<Vec<f32>, String> {
    let sample: fn(u8) -> f32 = match format {
        SampleFormat::Cs8 => |byte| byte as i8 as f32 / 128.0,
        SampleFormat::Cu8 => |byte| (f32::from(byte) - 127.5) / 128.0,
        _ => return Err("ADS-B supports only 8-bit IQ".into()),
    };
    let mut result = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let i = sample(pair[0]);
        let q = sample(pair[1]);
        result.push((i * i + q * q).sqrt());
    }
    Ok(result)
}

/// Run the native ADS-B receiver and emit JSON aircraft events over UDP.
pub fn run(radio: &RadioDescriptor, config: AdsbConfig) -> Result<(), String> {
    let (rate, format) = crate::receiver::spectrum_format(radio)?;
    let _claim = crate::receiver::claim(radio)?;
    let mut child = crate::receiver::Receiver(
        crate::receiver::iq_command(radio, 1_090_000_000, config.gain_db())?
            .spawn()
            .map_err(|error| format!("start ADS-B receiver: {error}"))?,
    );
    let mut stdout = child
        .0
        .stdout
        .take()
        .ok_or("ADS-B receiver stdout is unavailable")?;
    let socket =
        UdpSocket::bind("0.0.0.0:0").map_err(|error| format!("bind ADS-B socket: {error}"))?;
    socket
        .connect(config.destination)
        .map_err(|error| format!("connect ADS-B socket: {error}"))?;
    let mut demod = Demodulator::new(rate);
    let mut decoder = AdsbDecoder::new();
    let mut bytes = [0_u8; 4096];
    let mut sequence = 0_u64;
    eprintln!(
        "radioman: ADS-B radio={} center=1090000000Hz rate={rate} destination={}",
        radio.id, config.destination
    );
    loop {
        stdout
            .read_exact(&mut bytes)
            .map_err(|error| format!("read ADS-B IQ: {error}"))?;
        let samples = magnitudes(&bytes, format.clone())?;
        for message in demod.push(&samples) {
            let Some(event) = decoder.decode(&message, now_ms()) else {
                continue;
            };
            sequence = sequence.saturating_add(1);
            let mut value = serde_json::to_value(&event)
                .map_err(|error| format!("encode ADS-B event: {error}"))?;
            if let Some(object) = value.as_object_mut() {
                object.insert("sequence".into(), sequence.into());
            }
            let encoded = serde_json::to_vec(&value)
                .map_err(|error| format!("encode ADS-B event: {error}"))?;
            socket
                .send(&encoded)
                .map_err(|error| format!("send ADS-B event: {error}"))?;
            println!("{}", String::from_utf8_lossy(&encoded));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(value: &str) -> [u8; 14] {
        let bytes = (0..14)
            .map(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).unwrap())
            .collect::<Vec<_>>();
        bytes.try_into().unwrap()
    }

    #[test]
    fn known_adsb_frames_pass_crc() {
        for value in [
            "8D4840D6202CC371C32CE0576098",
            "8D40621D58C382D690C8AC2863A7",
            "8D40621D58C386435CC412692AD6",
            "8D485020994409940838175B284F",
        ] {
            assert_eq!(crc24(&hex(value)), 0, "{value}");
        }
    }

    #[test]
    fn decodes_identification_and_global_cpr() {
        let mut decoder = AdsbDecoder::new();
        let identification = decoder
            .decode(&hex("8D4840D6202CC371C32CE0576098"), 1)
            .unwrap();
        assert_eq!(identification.icao, "4840D6");
        assert_eq!(identification.callsign.as_deref(), Some("KLM1023"));

        let even = decoder
            .decode(&hex("8D40621D58C382D690C8AC2863A7"), 1)
            .unwrap();
        let odd = decoder
            .decode(&hex("8D40621D58C386435CC412692AD6"), 2)
            .unwrap();
        assert_eq!(even.icao, "40621D");
        assert_eq!(even.altitude_ft, Some(38_000));
        assert!(odd.latitude.is_some());
        assert!(odd.longitude.is_some());
        let latitude = odd.latitude.unwrap();
        let longitude = odd.longitude.unwrap();
        assert!((latitude - 52.26578).abs() < 0.01, "{latitude}");
        assert!((longitude - 3.93891).abs() < 0.01, "{longitude}");
    }

    fn synth_frame(value: &str, rate: f64) -> Vec<f32> {
        let message = hex(value);
        let mut bits = [0_u8; MODE_S_BITS];
        for (index, bit) in bits.iter_mut().enumerate() {
            *bit = (message[index / 8] >> (7 - index % 8)) & 1;
        }
        let len =
            ((MODE_S_PREAMBLE_US_LEN + MODE_S_BITS as f64) * rate / 1_000_000.0).ceil() as usize;
        let mut samples = vec![0.01_f32; len];
        for micros in MODE_S_PREAMBLE_US {
            let index = (micros * rate / 1_000_000.0).round() as usize;
            samples[index] = 1.0;
        }
        let start = MODE_S_PREAMBLE_US_LEN * rate / 1_000_000.0;
        for (bit, value) in bits.iter().enumerate() {
            let pulse = if *value == 1 { 0.25 } else { 0.75 };
            let index = (start + bit as f64 * rate / 1_000_000.0 + pulse * rate / 1_000_000.0)
                .round() as usize;
            samples[index] = 1.0;
        }
        samples
    }

    #[test]
    fn synthesised_mode_s_frame_demodulates_and_passes_crc() {
        let rate = 2_048_000_u32;
        let mut demod = Demodulator::new(rate);
        let frames = demod.push(&synth_frame(
            "8D4840D6202CC371C32CE0576098",
            f64::from(rate),
        ));
        assert_eq!(frames.len(), 1);
        assert_eq!(crc24(&frames[0]), 0);
        assert_eq!(&frames[0][..], &hex("8D4840D6202CC371C32CE0576098"));
    }
}
