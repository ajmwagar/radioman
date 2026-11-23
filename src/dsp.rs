//! Bounded receive DSP for the native radio owner.
//!
//! This module deliberately does not own a USB device, a clock, or a sink. It
//! consumes complex baseband blocks and produces bounded mono PCM blocks. The
//! `soil:radio` WIT contract is the boundary a Spore/plugin sees; this is the
//! native implementation behind it.
//!
//! No framework is required for the first receive path: `num-complex` supplies
//! the sample type and the detector/resampler below are small, deterministic
//! state machines. `rustfft` remains in the crate for the spectrum path.

use num_complex::Complex32;
use serde::{Deserialize, Serialize};

pub const DEFAULT_AUDIO_RATE_HZ: u32 = 48_000;
pub const MAX_INPUT_SAMPLES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemodMode {
    Am,
    Nfm,
    Wfm,
    Usb,
    Lsb,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemodConfig {
    pub mode: DemodMode,
    pub input_rate_hz: f64,
    pub output_rate_hz: f64,
    pub bandwidth_hz: f64,
    /// Peak deviation used to scale the FM discriminator. NFM voice is 2.5k;
    /// broadcast FM is 75k.
    pub deviation_hz: f64,
}

impl DemodConfig {
    pub fn new(mode: DemodMode, input_rate_hz: f64) -> Self {
        let (bandwidth_hz, deviation_hz) = match mode {
            DemodMode::Am => (6_000.0, 0.0),
            DemodMode::Nfm => (12_500.0, 2_500.0),
            DemodMode::Wfm => (200_000.0, 75_000.0),
            DemodMode::Usb | DemodMode::Lsb => (3_000.0, 0.0),
        };
        Self {
            mode,
            input_rate_hz,
            output_rate_hz: f64::from(DEFAULT_AUDIO_RATE_HZ),
            bandwidth_hz,
            deviation_hz,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.input_rate_hz.is_finite() || self.input_rate_hz <= 0.0 {
            return Err("input rate must be positive".into());
        }
        if !self.output_rate_hz.is_finite() || self.output_rate_hz <= 0.0 {
            return Err("output rate must be positive".into());
        }
        if self.output_rate_hz > self.input_rate_hz {
            return Err("output rate cannot exceed input rate".into());
        }
        if !self.bandwidth_hz.is_finite() || self.bandwidth_hz <= 0.0 {
            return Err("bandwidth must be positive".into());
        }
        if matches!(self.mode, DemodMode::Nfm | DemodMode::Wfm)
            && (!self.deviation_hz.is_finite() || self.deviation_hz <= 0.0)
        {
            return Err("FM deviation must be positive".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Demodulator {
    config: DemodConfig,
    last_iq: Option<Complex32>,
    deemphasis: f32,
    dc_block: f32,
    lowpass_1: f32,
    lowpass_2: f32,
    resample_phase: f64,
    sequence: u64,
}

impl Demodulator {
    pub fn new(config: DemodConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            last_iq: None,
            deemphasis: 0.0,
            dc_block: 0.0,
            lowpass_1: 0.0,
            lowpass_2: 0.0,
            resample_phase: 0.0,
            sequence: 0,
        })
    }

    pub fn config(&self) -> &DemodConfig {
        &self.config
    }

    /// Process one bounded IQ block and return mono f32 samples at
    /// `config.output_rate_hz`. The caller owns the sink and its clock.
    pub fn process(&mut self, iq: &[Complex32]) -> Result<Vec<f32>, String> {
        if iq.len() > MAX_INPUT_SAMPLES {
            return Err(format!("IQ block exceeds {MAX_INPUT_SAMPLES} samples"));
        }
        if iq.is_empty() {
            return Ok(Vec::new());
        }

        let mut detected = Vec::with_capacity(iq.len());
        match self.config.mode {
            DemodMode::Nfm | DemodMode::Wfm => {
                let previous = self.last_iq.unwrap_or(iq[0]);
                let mut last = previous;
                for sample in iq {
                    let product = *sample * last.conj();
                    let phase = product.arg();
                    let scaled = phase
                        * (self.config.input_rate_hz
                            / (std::f64::consts::TAU * self.config.deviation_hz))
                            as f32;
                    detected.push(scaled);
                    last = *sample;
                }
                self.last_iq = Some(last);
            }
            DemodMode::Am => {
                let rate = self.config.input_rate_hz as f32;
                for sample in iq {
                    let envelope = sample.norm();
                    // A slow one-pole high-pass removes the carrier DC term.
                    self.dc_block += (envelope - self.dc_block) * (20.0 / rate).min(0.25);
                    detected.push(envelope - self.dc_block);
                }
            }
            DemodMode::Usb => detected.extend(iq.iter().map(|sample| sample.re)),
            DemodMode::Lsb => detected.extend(iq.iter().map(|sample| sample.im)),
        }

        if matches!(self.config.mode, DemodMode::Nfm | DemodMode::Wfm) {
            // 75 us de-emphasis, applied at the detector rate.
            let dt = 1.0 / self.config.input_rate_hz;
            let tau = 75.0e-6;
            let alpha = (dt / (tau + dt)) as f32;
            for value in &mut detected {
                self.deemphasis += alpha * (*value - self.deemphasis);
                *value = self.deemphasis;
            }
        }

        // Two cascaded one-poles reject energy above the audio band before the
        // fractional resampler. Fast enough for the Pi and stable across blocks.
        let cutoff = (self.config.bandwidth_hz / 2.0)
            .min(self.config.output_rate_hz * 0.45)
            .max(1.0);
        let alpha =
            (1.0 - (-std::f64::consts::TAU * cutoff / self.config.input_rate_hz).exp()) as f32;
        for value in &mut detected {
            self.lowpass_1 += alpha * (*value - self.lowpass_1);
            self.lowpass_2 += alpha * (self.lowpass_1 - self.lowpass_2);
            *value = self.lowpass_2;
        }

        let mut output = self.resample(&detected);
        // Keep the wire contract bounded and avoid clipping downstream sinks.
        for value in &mut output {
            *value = value.clamp(-1.0, 1.0);
        }
        self.sequence = self.sequence.saturating_add(1);
        Ok(output)
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn resample(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        let step = self.config.input_rate_hz / self.config.output_rate_hz;
        if step <= 1.0 {
            return input.to_vec();
        }
        let mut output = Vec::with_capacity(((input.len() as f64) / step).ceil() as usize + 2);
        let mut position = self.resample_phase;
        while position < (input.len() - 1) as f64 {
            let index = position.floor() as usize;
            let fraction = (position - index as f64) as f32;
            let first = input[index];
            let second = input[index + 1];
            output.push(first + (second - first) * fraction);
            position += step;
        }
        self.resample_phase = (position - input.len() as f64).max(0.0);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: f64, frequency: f64, samples: usize) -> Vec<Complex32> {
        (0..samples)
            .map(|index| {
                let phase = std::f64::consts::TAU * frequency * index as f64 / rate;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn fm_demodulates_a_tone_without_exploding() {
        let mut demod = Demodulator::new(DemodConfig::new(DemodMode::Nfm, 240_000.0)).unwrap();
        let input = tone(240_000.0, 1_000.0, 24_000);
        let output = demod.process(&input).unwrap();
        assert!(output.len() > 4_000, "output was {}", output.len());
        assert!(output.iter().all(|value| value.is_finite()));
        assert!(output.iter().any(|value| value.abs() > 0.01));
    }

    #[test]
    fn am_envelope_tracks_amplitude() {
        let mut demod = Demodulator::new(DemodConfig::new(DemodMode::Am, 240_000.0)).unwrap();
        let input: Vec<_> = tone(240_000.0, 1_000.0, 24_000)
            .into_iter()
            .enumerate()
            .map(|(index, sample)| {
                let envelope = 0.2
                    + 0.15
                        * (std::f64::consts::TAU * 200.0 * index as f64 / 240_000.0).sin() as f32;
                sample * envelope
            })
            .collect();
        let output = demod.process(&input).unwrap();
        assert!(output.iter().all(|value| value.is_finite()));
        let peak = output
            .iter()
            .fold(0.0_f32, |peak, value| peak.max(value.abs()));
        assert!(peak > 0.01, "peak was {peak}");
    }

    #[test]
    fn output_is_bounded_and_configured_rate_is_respected() {
        let mut demod = Demodulator::new(DemodConfig {
            mode: DemodMode::Wfm,
            input_rate_hz: 2_400_000.0,
            output_rate_hz: 48_000.0,
            bandwidth_hz: 200_000.0,
            deviation_hz: 75_000.0,
        })
        .unwrap();
        let input = tone(2_400_000.0, 10_000.0, 24_000);
        let output = demod.process(&input).unwrap();
        let expected = 24_000.0 / 50.0;
        assert!((output.len() as f64 - expected).abs() < 20.0);
        assert!(output.iter().all(|value| (-1.0..=1.0).contains(value)));
    }

    #[test]
    fn invalid_config_is_rejected() {
        let config = DemodConfig {
            mode: DemodMode::Nfm,
            input_rate_hz: 48_000.0,
            output_rate_hz: 96_000.0,
            bandwidth_hz: 12_500.0,
            deviation_hz: 2_500.0,
        };
        assert!(config.validate().is_err());
    }
}
