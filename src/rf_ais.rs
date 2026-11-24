//! Feature-gated native AIS receive path.
//!
//! AIS is GMSK at 9600 baud on two 25 kHz channels around 162 MHz. This
//! module downconverts each channel, FM-discriminates it, recovers the bit
//! clock, reverses NRZI, removes HDLC bit stuffing, validates the FCS, and
//! emits bounded decoded vessel events. It intentionally keeps the RF/DSP
//! path in Rust; Soil consumes only the events.

use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

use rustfft::num_complex::Complex32;
use serde::Serialize;

use crate::{RadioDescriptor, SampleFormat};

const SAMPLE_RATE_HZ: f64 = 2_048_000.0;
const AUDIO_RATE_HZ: f64 = 64_000.0;
const DECIMATE: u64 = 32;
const BAUD_HZ: f64 = 9_600.0;
const CHANNEL_OFFSETS_HZ: [f64; 2] = [-25_000.0, 25_000.0];
const CHANNEL_NAMES: [&str; 2] = ["A", "B"];
const MAX_FRAME_BITS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AisConfig {
    pub destination: SocketAddr,
    pub gain_db_tenths: u16,
}

impl AisConfig {
    pub fn parse(destination: &str, gain_db: f32) -> Result<Self, String> {
        let destination = destination
            .parse()
            .map_err(|error| format!("invalid AIS destination: {error}"))?;
        if !gain_db.is_finite() || !(0.0..=49.6).contains(&gain_db) {
            return Err("AIS gain must be between 0 and 49.6 dB".into());
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
pub struct AisEvent {
    pub schema_version: u32,
    pub channel: String,
    pub observed_at_ms: u64,
    pub message_type: u8,
    pub mmsi: u32,
    pub payload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
}

struct Channel {
    name: &'static str,
    phase: f64,
    phase_step: f64,
    lp_i: f32,
    lp_q: f32,
    previous: Option<Complex32>,
    dc: f32,
    pll: u32,
    pll_increment: u32,
    previous_slice: bool,
    last_bit: bool,
    hdlc: Hdlc,
}

impl Channel {
    fn new(name: &'static str, offset_hz: f64) -> Self {
        Self {
            name,
            phase: 0.0,
            phase_step: std::f64::consts::TAU * offset_hz / SAMPLE_RATE_HZ,
            lp_i: 0.0,
            lp_q: 0.0,
            previous: None,
            dc: 0.0,
            pll: 0,
            pll_increment: (65_536.0 * BAUD_HZ / AUDIO_RATE_HZ).round() as u32,
            previous_slice: false,
            last_bit: false,
            hdlc: Hdlc::new(),
        }
    }

    fn push(&mut self, i: f32, q: f32, sample: u64, output: &mut Vec<AisEvent>, at_ms: u64) {
        self.phase = (self.phase + self.phase_step).rem_euclid(std::f64::consts::TAU);
        let mixer = Complex32::from_polar(1.0, -self.phase as f32);
        let mixed_i = i * mixer.re - q * mixer.im;
        let mixed_q = i * mixer.im + q * mixer.re;
        // One-pole low-pass ahead of the 32x decimator. AIS occupies only a
        // few kHz around the channel centre, so this is sufficient for the
        // discriminator while keeping the implementation bounded.
        const ALPHA: f32 = 0.018;
        self.lp_i += ALPHA * (mixed_i - self.lp_i);
        self.lp_q += ALPHA * (mixed_q - self.lp_q);
        if sample % DECIMATE != 0 {
            return;
        }
        let current = Complex32::new(self.lp_i, self.lp_q);
        let Some(previous) = self.previous.replace(current) else {
            return;
        };
        let mut audio = (current * previous.conj()).arg();
        self.dc += 0.001 * (audio - self.dc);
        audio -= self.dc;

        let slice = audio > 0.0;
        if slice != self.previous_slice {
            if self.pll < 0x8000 {
                self.pll += self.pll_increment / 16;
            } else {
                self.pll -= self.pll_increment / 16;
            }
        }
        self.previous_slice = slice;
        self.pll = self.pll.wrapping_add(self.pll_increment);
        if self.pll > 0xffff {
            let raw = slice;
            let bit = u8::from(!(raw ^ self.last_bit));
            self.last_bit = raw;
            self.pll &= 0xffff;
            if let Some(frame) = self.hdlc.push(bit) {
                if let Some(event) = decode_frame(self.name, &frame, at_ms) {
                    output.push(event);
                }
            }
        }
    }
}

#[derive(Default)]
struct Hdlc {
    collecting: bool,
    bits: Vec<u8>,
    raw_history: u8,
}

impl Hdlc {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, bit: u8) -> Option<Vec<u8>> {
        self.raw_history = ((self.raw_history << 1) | bit) & 0xff;
        if self.raw_history == 0x7e {
            if self.collecting {
                // The detection happens on the eighth flag bit, after the
                // previous seven flag bits were appended to the frame.
                self.bits.truncate(self.bits.len().saturating_sub(7));
                let raw = std::mem::take(&mut self.bits);
                self.collecting = false;
                self.raw_history = 0;
                return Some(destuff(&raw));
            }
            self.collecting = true;
            self.bits.clear();
            self.raw_history = 0;
            return None;
        }
        if !self.collecting {
            return None;
        }
        self.bits.push(bit);
        if self.bits.len() > MAX_FRAME_BITS {
            self.collecting = false;
            self.bits.clear();
        }
        None
    }
}

fn destuff(raw: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(raw.len());
    let mut ones = 0_u8;
    for bit in raw {
        if ones == 5 {
            ones = 0;
            if *bit == 0 {
                continue;
            }
        }
        result.push(*bit);
        ones = if *bit == 1 { ones + 1 } else { 0 };
    }
    result
}

fn decode_frame(channel: &str, bits: &[u8], observed_at_ms: u64) -> Option<AisEvent> {
    if bits.len() < 16 || bits.len() > MAX_FRAME_BITS || bits.len() % 8 != 0 {
        return None;
    }
    let mut bytes = vec![0_u8; bits.len() / 8];
    for (index, bit) in bits.iter().enumerate() {
        bytes[index / 8] |= bit << (7 - index % 8);
    }
    if crc16(&bytes) != 0x0f47 {
        return None;
    }
    let payload_bits = &bits[..bits.len() - 16];
    let message_type = bits_value(payload_bits, 0, 6)? as u8;
    let mmsi = bits_value(payload_bits, 6, 30)? as u32;
    let payload = sixbit_string(payload_bits);
    let (latitude, longitude) = if (1..=3).contains(&message_type) {
        let longitude = signed(bits_value(payload_bits, 59, 28)?, 28) as f64 / 600_000.0;
        let latitude = signed(bits_value(payload_bits, 87, 27)?, 27) as f64 / 600_000.0;
        (Some(latitude), Some(longitude))
    } else {
        (None, None)
    };
    Some(AisEvent {
        schema_version: 1,
        channel: channel.into(),
        observed_at_ms,
        message_type,
        mmsi,
        payload,
        latitude,
        longitude,
    })
}

fn bits_value(bits: &[u8], start: usize, count: usize) -> Option<u64> {
    if start + count > bits.len() {
        return None;
    }
    let mut value = 0_u64;
    for bit in &bits[start..start + count] {
        value = (value << 1) | u64::from(*bit);
    }
    Some(value)
}

fn signed(value: u64, width: usize) -> i64 {
    let sign = 1_u64 << (width - 1);
    if value & sign != 0 {
        value as i64 - (1_i64 << width)
    } else {
        value as i64
    }
}

fn sixbit_string(bits: &[u8]) -> String {
    let mut result = String::new();
    for chunk in bits.chunks(6) {
        if chunk.len() < 6 {
            break;
        }
        let mut value = 0_u8;
        for bit in chunk {
            value = (value << 1) | *bit;
        }
        let byte = value + if value < 40 { 48 } else { 56 };
        result.push(char::from(byte));
    }
    result
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffff_u16;
    for byte in bytes {
        let mut value = 0x100_u16 | u16::from(*byte);
        while value > 1 {
            if (crc ^ value) & 1 != 0 {
                crc = (crc >> 1) ^ 0x8408;
            } else {
                crc >>= 1;
            }
            value >>= 1;
        }
    }
    !crc
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn decode_iq(bytes: &[u8], format: SampleFormat) -> Result<Vec<(f32, f32)>, String> {
    let sample: fn(u8) -> f32 = match format {
        SampleFormat::Cs8 => |byte| byte as i8 as f32 / 128.0,
        SampleFormat::Cu8 => |byte| (f32::from(byte) - 127.5) / 128.0,
        _ => return Err("AIS supports only 8-bit IQ".into()),
    };
    let mut result = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        result.push((sample(pair[0]), sample(pair[1])));
    }
    Ok(result)
}

/// Run the native dual-channel AIS receiver and emit JSON vessel events.
pub fn run(radio: &RadioDescriptor, config: AisConfig) -> Result<(), String> {
    let (rate, format) = crate::receiver::spectrum_format(radio)?;
    if rate != SAMPLE_RATE_HZ as u32 {
        return Err("AIS decoder expects the 2.048 MS/s receive profile".into());
    }
    let _claim = crate::receiver::claim(radio)?;
    let mut child = crate::receiver::Receiver(
        crate::receiver::iq_command(radio, 162_000_000, config.gain_db())?
            .spawn()
            .map_err(|error| format!("start AIS receiver: {error}"))?,
    );
    let mut stdout = child
        .0
        .stdout
        .take()
        .ok_or("AIS receiver stdout is unavailable")?;
    let socket =
        UdpSocket::bind("0.0.0.0:0").map_err(|error| format!("bind AIS socket: {error}"))?;
    socket
        .connect(config.destination)
        .map_err(|error| format!("connect AIS socket: {error}"))?;
    let mut channels = [
        Channel::new(CHANNEL_NAMES[0], CHANNEL_OFFSETS_HZ[0]),
        Channel::new(CHANNEL_NAMES[1], CHANNEL_OFFSETS_HZ[1]),
    ];
    let mut bytes = [0_u8; 4096];
    let mut sample = 0_u64;
    let mut sequence = 0_u64;
    eprintln!(
        "radioman: AIS radio={} center=162000000Hz rate={rate} destination={}",
        radio.id, config.destination
    );
    loop {
        stdout
            .read_exact(&mut bytes)
            .map_err(|error| format!("read AIS IQ: {error}"))?;
        let iq = decode_iq(&bytes, format.clone())?;
        let mut events = Vec::new();
        let at_ms = now_ms();
        for (i, q) in iq {
            for channel in &mut channels {
                channel.push(i, q, sample, &mut events, at_ms);
            }
            sample = sample.wrapping_add(1);
        }
        for event in events {
            sequence = sequence.saturating_add(1);
            let mut value = serde_json::to_value(&event)
                .map_err(|error| format!("encode AIS event: {error}"))?;
            if let Some(object) = value.as_object_mut() {
                object.insert("sequence".into(), sequence.into());
            }
            socket
                .send(
                    &serde_json::to_vec(&value)
                        .map_err(|error| format!("encode AIS event: {error}"))?,
                )
                .map_err(|error| format!("send AIS event: {error}"))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sixbit(bits: &[u8], start: usize, count: usize) -> u64 {
        bits_value(bits, start, count).unwrap()
    }

    fn payload_bits(payload: &str) -> Vec<u8> {
        let mut bits = Vec::new();
        for byte in payload.bytes() {
            let value = if byte >= 96 { byte - 56 } else { byte - 48 };
            for bit in (0..6).rev() {
                bits.push((value >> bit) & 1);
            }
        }
        bits
    }
    fn frame_bits(payload: &[u8]) -> Vec<u8> {
        let mut bits = payload.to_vec();
        let mut bytes = vec![0_u8; payload.len() / 8];
        for (index, bit) in payload.iter().enumerate() {
            bytes[index / 8] |= bit << (7 - index % 8);
        }
        let crc = crc16(&bytes).swap_bytes();
        for bit in (0..16).rev() {
            bits.push(((crc >> bit) & 1) as u8);
        }
        bits
    }

    fn stuff(bits: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        let mut ones = 0;
        for bit in bits {
            result.push(*bit);
            if *bit == 1 {
                ones += 1;
                if ones == 5 {
                    result.push(0);
                    ones = 0;
                }
            } else {
                ones = 0;
            }
        }
        result
    }

    #[test]
    fn known_ais_payload_parses_to_type_and_mmsi() {
        let bits = payload_bits("15Muq?0P00PD;88MD5MTDww@2D0l");
        assert_eq!(sixbit(&bits, 0, 6), 1);
        assert_eq!(sixbit(&bits, 6, 30), 91_741_775);
        let event = decode_frame("A", &frame_bits(&bits), 1).unwrap();
        assert_eq!(event.message_type, 1);
        assert_eq!(event.mmsi, 91_741_775);
        assert!(event.payload.starts_with("15Muq"));
    }

    #[test]
    fn hdlc_stuffing_and_fcs_round_trip() {
        let bits = payload_bits("15Muq?0P00PD;88MD5MTDww@2D0l");
        let frame = frame_bits(&bits);
        let stream = stuff(&frame);
        let mut hdlc = Hdlc::new();
        let mut decoded = None;
        // Start flag, stuffed frame, stop flag.
        for bit in [0, 1, 1, 1, 1, 1, 1, 0]
            .into_iter()
            .chain(stream)
            .chain([0, 1, 1, 1, 1, 1, 1, 0])
        {
            if let Some(frame) = hdlc.push(bit) {
                decoded = Some(frame);
            }
        }
        assert_eq!(decoded.unwrap(), frame);
    }

    #[test]
    fn corrupt_frame_fails_fcs() {
        let mut frame = frame_bits(&payload_bits("15Muq?0P00PD;88MD5MTDww@2D0l"));
        frame[40] ^= 1;
        assert!(decode_frame("A", &frame, 1).is_none());
    }
}
