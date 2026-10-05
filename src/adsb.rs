//! Native 1090 MHz Mode-S/ADS-B decoding into Radioman observations.

use crate::decoder::{DecodedPacket, Decoder, DecoderConfig};
use adsb_deku::{DF, Frame, ICAO};
use libdump1090_rs::{demod_2400::demodulate2400, utils::to_mag};
use num_complex::Complex;
use rsadsb_common::Airplanes;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};

pub const OBSERVATION_CONTRACT: &str = "radioman.adsb.observation.v1";
const ADSB_FREQUENCY_HZ: u64 = 1_090_000_000;
const ADSB_SAMPLE_RATE_HZ: u32 = 2_400_000;
const MAX_COMPLEX_PER_DEMODULATION: usize = 120_000;
const CARRY_COMPLEX_SAMPLES: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdsbObservation {
    pub installation_id: String,
    pub receiver_id: String,
    pub icao24: String,
    pub observed_at_unix_ms: u64,
    pub sequence: u64,
    pub position: Option<AdsbPosition>,
    pub callsign: Option<String>,
    pub altitude_m: Option<f64>,
    pub heading_deg: Option<f32>,
    pub speed_mps: Option<f32>,
    pub climb_mps: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdsbPosition {
    pub latitude: f64,
    pub longitude: f64,
}

pub struct ModeS1090Decoder {
    installation_id: String,
    receiver_position: (f64, f64),
    maximum_range_km: f64,
    config: Option<DecoderConfig>,
    airplanes: Airplanes,
    carry: Vec<Complex<i16>>,
    sequence: u64,
}

impl ModeS1090Decoder {
    pub fn new(
        installation_id: impl Into<String>,
        receiver_position: (f64, f64),
        maximum_range_km: f64,
    ) -> Result<Self, String> {
        let installation_id = installation_id.into();
        if installation_id.is_empty()
            || !receiver_position.0.is_finite()
            || !receiver_position.1.is_finite()
            || !(-90.0..=90.0).contains(&receiver_position.0)
            || !(-180.0..=180.0).contains(&receiver_position.1)
            || !maximum_range_km.is_finite()
            || maximum_range_km <= 0.0
        {
            return Err("invalid ADS-B installation, receiver position, or range".into());
        }
        Ok(Self {
            installation_id,
            receiver_position,
            maximum_range_km,
            config: None,
            airplanes: Airplanes::new(),
            carry: Vec::new(),
            sequence: 0,
        })
    }

    fn ingest_frame(
        &mut self,
        bytes: &[u8],
        observed_at_unix_ms: u64,
    ) -> Result<Option<DecodedPacket>, String> {
        let frame =
            Frame::from_bytes(bytes).map_err(|error| format!("decode ADS-B frame: {error:?}"))?;
        let icao = frame_icao(&frame);
        self.airplanes
            .action(frame, self.receiver_position, self.maximum_range_km);
        let Some(icao) = icao else { return Ok(None) };
        let Some(state) = self.airplanes.get(icao) else {
            return Ok(None);
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("ADS-B sequence overflow")?;
        let details = self.airplanes.aircraft_details(icao);
        let observation = AdsbObservation {
            installation_id: self.installation_id.clone(),
            receiver_id: self
                .config
                .as_ref()
                .ok_or("decoder is not running")?
                .source
                .clone(),
            icao24: icao.to_string().to_ascii_lowercase(),
            observed_at_unix_ms,
            sequence: self.sequence,
            position: details.as_ref().map(|details| AdsbPosition {
                latitude: details.position.latitude,
                longitude: details.position.longitude,
            }),
            callsign: state
                .callsign
                .as_ref()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            altitude_m: details
                .as_ref()
                .map(|details| f64::from(details.altitude) * 0.3048),
            heading_deg: state.heading,
            speed_mps: state.speed.map(|knots| knots * 0.514_444),
            climb_mps: state
                .vert_speed
                .map(|feet_per_minute| f32::from(feet_per_minute) * 0.005_08),
        };
        Ok(Some(DecodedPacket {
            source: observation.receiver_id.clone(),
            session_id: format!("{}-adsb", observation.receiver_id),
            sequence: observation.sequence,
            observed_at_unix_ms,
            contract: OBSERVATION_CONTRACT.into(),
            value: serde_json::to_value(observation).map_err(|error| error.to_string())?,
        }))
    }
}

impl Decoder for ModeS1090Decoder {
    fn start(&mut self, config: &DecoderConfig) -> Result<(), String> {
        if config.decoder != "mode_s_1090"
            || config.frequency_hz != ADSB_FREQUENCY_HZ
            || config.sample_rate_hz != ADSB_SAMPLE_RATE_HZ
        {
            return Err("mode_s_1090 requires 1090 MHz CU8 at 2.4 MS/s".into());
        }
        self.config = Some(config.clone());
        Ok(())
    }

    fn process(&mut self, iq: &[u8]) -> Result<Vec<DecodedPacket>, String> {
        if self.config.is_none() {
            return Err("decoder is not running".into());
        }
        if !iq.len().is_multiple_of(2) {
            return Err("ADS-B CU8 IQ must contain complete complex samples".into());
        }
        let mut complex = std::mem::take(&mut self.carry);
        complex.reserve(iq.len() / 2);
        complex.extend(iq.as_chunks::<2>().0.iter().map(|sample| {
            Complex::new(
                (i16::from(sample[0]) - 128) << 8,
                (i16::from(sample[1]) - 128) << 8,
            )
        }));
        let observed_at_unix_ms = now_ms();
        let mut packets = Vec::new();
        let mut touched = BTreeSet::new();
        let step = MAX_COMPLEX_PER_DEMODULATION - CARRY_COMPLEX_SAMPLES;
        for start in (0..complex.len()).step_by(step) {
            let end = (start + MAX_COMPLEX_PER_DEMODULATION).min(complex.len());
            let chunk = &complex[start..end];
            // @derives-from github:rsadsb/dump1090_rs@94a0e4d -- CU8 conversion and chunking wrap the upstream 2.4 MS/s demodulator.
            for message in demodulate2400(&to_mag(chunk)).map_err(str::to_owned)? {
                if let Some(packet) = self.ingest_frame(message.buffer(), observed_at_unix_ms)? {
                    let icao = packet.value["icao24"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    if touched.insert(icao) {
                        packets.push(packet)
                    }
                }
            }
        }
        self.carry = complex
            .into_iter()
            .rev()
            .take(CARRY_COMPLEX_SAMPLES)
            .collect();
        self.carry.reverse();
        Ok(packets)
    }

    fn stop(&mut self) -> Result<(), String> {
        self.config = None;
        self.carry.clear();
        Ok(())
    }
}

fn frame_icao(frame: &Frame) -> Option<ICAO> {
    match &frame.df {
        DF::ADSB(adsb) => Some(adsb.icao),
        DF::TisB { pi, .. } => Some(*pi),
        _ => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DecoderConfig {
        DecoderConfig {
            decoder: "mode_s_1090".into(),
            source: "roof-rtl".into(),
            frequency_hz: ADSB_FREQUENCY_HZ,
            sample_rate_hz: ADSB_SAMPLE_RATE_HZ,
        }
    }

    #[test]
    fn mode_s_frame_becomes_a_canonical_observation() {
        let mut decoder = ModeS1090Decoder::new("studio", (47.6062, -122.3321), 1_000.0).unwrap();
        decoder.start(&config()).unwrap();
        let packet = decoder
            .ingest_frame(&hex::decode("8da3f9cb9910100da8148571db11").unwrap(), 1_000)
            .unwrap()
            .unwrap();
        let observation: AdsbObservation = serde_json::from_value(packet.value).unwrap();
        assert_eq!(observation.icao24, "a3f9cb");
        assert_eq!(observation.receiver_id, "roof-rtl");
        assert!(observation.heading_deg.is_some());
        assert!(observation.speed_mps.is_some());
    }

    #[test]
    fn decoder_rejects_the_wrong_tuning_and_partial_iq() {
        let mut decoder = ModeS1090Decoder::new("studio", (47.6062, -122.3321), 500.0).unwrap();
        let mut wrong = config();
        wrong.sample_rate_hz = 2_048_000;
        assert!(decoder.start(&wrong).is_err());
        decoder.start(&config()).unwrap();
        assert!(decoder.process(&[127]).is_err());
    }
}
