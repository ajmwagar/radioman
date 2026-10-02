//! Provider-neutral radio ownership and session contracts.
//!
//! Radioman owns SDR semantics and the high-rate sample plane. Message buses
//! may route the bounded command/status envelopes defined here, but do not own
//! tuning, device arbitration, or IQ framing.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const CONTRACT_VERSION: u32 = 1;
pub const MAX_ID_BYTES: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioKind {
    RtlSdr,
    HackRf,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    Cu8,
    Cs8,
    Cs16Le,
    Cf32Le,
}

impl SampleFormat {
    pub const fn bytes_per_complex_sample(&self) -> u64 {
        match self {
            Self::Cu8 | Self::Cs8 => 2,
            Self::Cs16Le => 4,
            Self::Cf32Le => 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrequencyRange {
    pub minimum_hz: u64,
    pub maximum_hz: u64,
}

impl FrequencyRange {
    fn contains(&self, frequency_hz: u64) -> bool {
        self.minimum_hz <= frequency_hz && frequency_hz <= self.maximum_hz
    }

    fn validate(&self) -> Result<(), String> {
        if self.minimum_hz == 0 || self.minimum_hz > self.maximum_hz {
            return Err("invalid radio frequency range".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadioDescriptor {
    pub id: String,
    pub kind: RadioKind,
    pub label: String,
    pub serial: Option<String>,
    pub frequency: FrequencyRange,
    pub sample_rates_hz: BTreeSet<u32>,
    pub sample_formats: BTreeSet<SampleFormat>,
    pub receive: bool,
    pub transmit: bool,
}

impl RadioDescriptor {
    pub fn validate(&self) -> Result<(), String> {
        validate_id("radio id", &self.id)?;
        if self.label.trim().is_empty() || self.label.len() > 128 {
            return Err("radio label must contain 1..=128 bytes".into());
        }
        self.frequency.validate()?;
        if self.sample_rates_hz.is_empty() || self.sample_rates_hz.contains(&0) {
            return Err("radio must declare non-zero sample rates".into());
        }
        if self.sample_formats.is_empty() {
            return Err("radio must declare a sample format".into());
        }
        if !self.receive && !self.transmit {
            return Err("radio must support receive or transmit".into());
        }
        if matches!(self.kind, RadioKind::RtlSdr) && self.transmit {
            return Err("RTL-SDR cannot advertise transmit capability".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RxRequest {
    pub session_id: String,
    pub radio_id: String,
    pub center_frequency_hz: u64,
    pub sample_rate_hz: u32,
    pub sample_format: SampleFormat,
    pub gain_db: Option<f32>,
    pub ppm: i32,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    StartRx { request: RxRequest },
    Stop { request: StopRequest },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiverStatus {
    pub schema_version: u32,
    pub node_id: String,
    pub radio_id: String,
    pub session_id: Option<String>,
    pub generation: u64,
    pub phase: SessionPhase,
    pub center_frequency_hz: Option<u64>,
    pub sample_rate_hz: Option<u32>,
    pub gain_db: Option<f32>,
    pub observed_at_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IqStreamAnnouncement {
    pub schema_version: u32,
    pub session_id: String,
    pub generation: u64,
    pub endpoint: String,
    pub sample_format: SampleFormat,
    pub sample_rate_hz: u32,
    pub center_frequency_hz: u64,
    pub expires_at_ms: u64,
}

impl IqStreamAnnouncement {
    pub fn validate(&self, now_ms: u64) -> Result<(), String> {
        if self.schema_version != CONTRACT_VERSION {
            return Err("unsupported IQ announcement schema".into());
        }
        validate_id("session id", &self.session_id)?;
        if self.generation == 0 || self.expires_at_ms <= now_ms {
            return Err("stale IQ stream announcement".into());
        }
        if !(self.endpoint.starts_with("udp://") || self.endpoint.starts_with("quic://")) {
            return Err("IQ endpoint must use udp:// or quic://".into());
        }
        if self.sample_rate_hz == 0 || self.center_frequency_hz == 0 {
            return Err("IQ announcement has invalid tuning".into());
        }
        Ok(())
    }

    pub fn payload_bytes_per_second(&self) -> u64 {
        u64::from(self.sample_rate_hz) * self.sample_format.bytes_per_complex_sample()
    }
}

/// Deterministic exclusive-session coordinator. Hardware backends execute the
/// accepted transition; a bus adapter merely transports the request/result.
#[derive(Debug)]
pub struct Agent {
    node_id: String,
    radio: RadioDescriptor,
    generation: u64,
    active: Option<RxRequest>,
}

impl Agent {
    pub fn new(node_id: String, radio: RadioDescriptor) -> Result<Self, String> {
        validate_id("node id", &node_id)?;
        radio.validate()?;
        Ok(Self {
            node_id,
            radio,
            generation: 0,
            active: None,
        })
    }

    pub fn start_rx(&mut self, request: RxRequest, now_ms: u64) -> Result<ReceiverStatus, String> {
        request.validate_for(&self.radio)?;
        if let Some(active) = &self.active {
            return Err(format!("radio is owned by session {}", active.session_id));
        }
        self.generation = self.generation.saturating_add(1).max(1);
        self.active = Some(request.clone());
        Ok(self.status(SessionPhase::Starting, now_ms, None))
    }

    pub fn mark_running(&self, now_ms: u64) -> Result<ReceiverStatus, String> {
        if self.active.is_none() {
            return Err("no active receive session".into());
        }
        Ok(self.status(SessionPhase::Running, now_ms, None))
    }

    pub fn stop(&mut self, request: StopRequest, now_ms: u64) -> Result<ReceiverStatus, String> {
        let active = self.active.as_ref().ok_or("no active receive session")?;
        if active.session_id != request.session_id {
            return Err("session does not own this radio".into());
        }
        let status = self.status(SessionPhase::Stopped, now_ms, None);
        self.active = None;
        Ok(status)
    }

    fn status(&self, phase: SessionPhase, now_ms: u64, error: Option<String>) -> ReceiverStatus {
        ReceiverStatus {
            schema_version: CONTRACT_VERSION,
            node_id: self.node_id.clone(),
            radio_id: self.radio.id.clone(),
            session_id: self
                .active
                .as_ref()
                .map(|request| request.session_id.clone()),
            generation: self.generation,
            phase,
            center_frequency_hz: self
                .active
                .as_ref()
                .map(|request| request.center_frequency_hz),
            sample_rate_hz: self.active.as_ref().map(|request| request.sample_rate_hz),
            gain_db: self.active.as_ref().and_then(|request| request.gain_db),
            observed_at_ms: now_ms,
            error,
        }
    }
}

impl RxRequest {
    pub fn validate_for(&self, radio: &RadioDescriptor) -> Result<(), String> {
        validate_id("session id", &self.session_id)?;
        if self.radio_id != radio.id {
            return Err("request targets a different radio".into());
        }
        if !radio.receive {
            return Err("radio does not support receive".into());
        }
        if !radio.frequency.contains(self.center_frequency_hz) {
            return Err("center frequency is outside the radio range".into());
        }
        if !radio.sample_rates_hz.contains(&self.sample_rate_hz) {
            return Err("unsupported sample rate".into());
        }
        if !radio.sample_formats.contains(&self.sample_format) {
            return Err("unsupported sample format".into());
        }
        if self
            .gain_db
            .is_some_and(|gain| !gain.is_finite() || !(-20.0..=70.0).contains(&gain))
        {
            return Err("gain must be finite and between -20 and 70 dB".into());
        }
        if !(-1_000..=1_000).contains(&self.ppm) {
            return Err("frequency correction must be within +/-1000 ppm".into());
        }
        if self.duration_ms.is_some_and(|duration| duration == 0) {
            return Err("session duration must be non-zero".into());
        }
        Ok(())
    }
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err(format!("invalid {label}"));
    }
    Ok(())
}
