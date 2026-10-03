//! Provider-neutral radio ownership and session contracts.
//!
//! Radioman owns SDR semantics and the high-rate sample plane. Message buses
//! may route the bounded command/status envelopes defined here, but do not own
//! tuning, device arbitration, or IQ framing.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const CONTRACT_VERSION: u32 = 1;
pub const CONFIG_VERSION: u32 = 2;
pub const MAX_ID_BYTES: usize = 96;
pub const SPECTRUM_CONTRACT: &str = "radioman.spectrum.v1";
pub const SPECTRUM_CENSUS_CONTRACT: &str = "radioman.spectrum-census.v1";
pub const MIN_SPECTRUM_BINS: usize = 64;
pub const MAX_SPECTRUM_BINS: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioKind {
    RtlSdr,
    HackRf,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketRadioCapability {
    Health,
    Gnss,
    Display,
    LoraRx,
    LoraTx,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketRadioDescriptor {
    pub id: String,
    pub label: String,
    pub serial: String,
    pub transport: String,
    pub protocol: String,
    pub capabilities: BTreeSet<PacketRadioCapability>,
}

impl PacketRadioDescriptor {
    pub fn validate(&self) -> Result<(), String> {
        validate_id("packet radio id", &self.id)?;
        if self.label.trim().is_empty() || self.label.len() > 128 {
            return Err("packet radio label must contain 1..=128 bytes".into());
        }
        if self.serial.trim().is_empty() || self.serial.len() > 128 {
            return Err("packet radio serial must contain 1..=128 bytes".into());
        }
        if !self.transport.starts_with("/dev/serial/by-id/") {
            return Err("packet radio transport must use a stable /dev/serial/by-id path".into());
        }
        validate_id("packet radio protocol", &self.protocol)?;
        if self.capabilities.is_empty() {
            return Err("packet radio must declare at least one capability".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceIndex {
    pub schema_version: u32,
    pub node_id: String,
    pub radios: Vec<RadioDescriptor>,
    pub packet_radios: Vec<PacketRadioDescriptor>,
}

impl DeviceIndex {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CONTRACT_VERSION {
            return Err("unsupported device index schema".into());
        }
        validate_id("node id", &self.node_id)?;
        if self.radios.is_empty() && self.packet_radios.is_empty() {
            return Err("device index must contain at least one device".into());
        }
        let mut ids = BTreeSet::new();
        for radio in &self.radios {
            radio.validate()?;
            if !ids.insert(&radio.id) {
                return Err("device ids must be unique".into());
            }
        }
        for modem in &self.packet_radios {
            modem.validate()?;
            if !ids.insert(&modem.id) {
                return Err("device ids must be unique".into());
            }
        }
        Ok(())
    }
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
    pub fn contains(&self, frequency_hz: u64) -> bool {
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

/// One bounded FFT power frame. Frequencies are derived from center/span and
/// bin index; peaks and waterfall history are consumer-owned projections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpectrumFrame {
    pub schema_version: u32,
    pub session_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub observed_at_ms: u64,
    pub center_frequency_hz: u64,
    pub span_hz: u32,
    pub floor_dbfs: f32,
    pub ceiling_dbfs: f32,
    pub bins_dbfs: Vec<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning: Option<ActiveTuning>,
}

/// Low-rate, bounded projection of many FFT frames. This is the durable
/// observatory contract; raw IQ and renderer-specific history remain outside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpectrumCensusFrame {
    pub schema_version: u32,
    pub session_id: String,
    pub generation: u64,
    pub sequence: u64,
    pub started_at_ms: u64,
    pub observed_at_ms: u64,
    pub center_frequency_hz: u64,
    pub span_hz: u32,
    pub frame_count: u64,
    pub threshold_dbfs: f32,
    pub mean_dbfs: Vec<f32>,
    pub peak_dbfs: Vec<f32>,
    pub occupancy: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct SpectrumCensus {
    started_at_ms: u64,
    threshold_dbfs: f32,
    frames: u64,
    sums: Vec<f64>,
    peaks: Vec<f32>,
    occupied: Vec<u64>,
}

impl SpectrumCensus {
    pub fn new(started_at_ms: u64, buckets: usize, threshold_dbfs: f32) -> Result<Self, String> {
        if started_at_ms == 0 || !(4..=256).contains(&buckets) || !threshold_dbfs.is_finite() {
            return Err("invalid spectrum census configuration".into());
        }
        Ok(Self {
            started_at_ms,
            threshold_dbfs,
            frames: 0,
            sums: vec![0.0; buckets],
            peaks: vec![f32::NEG_INFINITY; buckets],
            occupied: vec![0; buckets],
        })
    }

    pub fn observe(&mut self, frame: &SpectrumFrame) -> Result<(), String> {
        frame.validate()?;
        self.frames += 1;
        for bucket in 0..self.sums.len() {
            let start = bucket * frame.bins_dbfs.len() / self.sums.len();
            let end = (bucket + 1) * frame.bins_dbfs.len() / self.sums.len();
            let value = frame.bins_dbfs[start..end]
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max);
            self.sums[bucket] += f64::from(value);
            self.peaks[bucket] = self.peaks[bucket].max(value);
            self.occupied[bucket] += u64::from(value >= self.threshold_dbfs);
        }
        Ok(())
    }

    pub fn snapshot(&self, frame: &SpectrumFrame) -> Result<SpectrumCensusFrame, String> {
        if self.frames == 0 {
            return Err("spectrum census has no observations".into());
        }
        Ok(SpectrumCensusFrame {
            schema_version: CONTRACT_VERSION,
            session_id: frame.session_id.clone(),
            generation: frame.generation,
            sequence: frame.sequence,
            started_at_ms: self.started_at_ms,
            observed_at_ms: frame.observed_at_ms,
            center_frequency_hz: frame.center_frequency_hz,
            span_hz: frame.span_hz,
            frame_count: self.frames,
            threshold_dbfs: self.threshold_dbfs,
            mean_dbfs: self
                .sums
                .iter()
                .map(|sum| (*sum / self.frames as f64) as f32)
                .collect(),
            peak_dbfs: self.peaks.clone(),
            occupancy: self
                .occupied
                .iter()
                .map(|count| *count as f32 / self.frames as f32)
                .collect(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExperimentOutput {
    Spectrum { destination: String },
    IqCapture { artifact_name: String },
    Adsb { destination: String },
}

/// A schedulable claim on one physical tuner. The service, not the caller,
/// arbitrates overlap and turns this intent into backend processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentRequest {
    pub owner: String,
    pub start_at_ms: u64,
    pub rx: RxRequest,
    pub output: ExperimentOutput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveTuning {
    pub owner: String,
    pub session_id: String,
    pub center_frequency_hz: u64,
    pub sample_rate_hz: u32,
    pub gain_db: Option<f32>,
    pub started_at_ms: u64,
    pub ends_at_ms: u64,
    pub output: ExperimentOutput,
}

impl ExperimentRequest {
    pub fn active_tuning(&self) -> ActiveTuning {
        ActiveTuning {
            owner: self.owner.clone(),
            session_id: self.rx.session_id.clone(),
            center_frequency_hz: self.rx.center_frequency_hz,
            sample_rate_hz: self.rx.sample_rate_hz,
            gain_db: self.rx.gain_db,
            started_at_ms: self.start_at_ms,
            ends_at_ms: self
                .start_at_ms
                .saturating_add(self.rx.duration_ms.unwrap_or(0)),
            output: self.output.clone(),
        }
    }
}

impl ExperimentRequest {
    pub fn validate_for(&self, radio: &RadioDescriptor) -> Result<(), String> {
        validate_id("experiment owner", &self.owner)?;
        self.rx.validate_for(radio)?;
        let duration = self
            .rx
            .duration_ms
            .ok_or("scheduled experiments require duration_ms")?;
        if duration == 0 || duration > 86_400_000 {
            return Err("experiment duration must be between 1 ms and 24 hours".into());
        }
        match &self.output {
            ExperimentOutput::Spectrum { destination } | ExperimentOutput::Adsb { destination } => {
                destination
                    .parse::<std::net::SocketAddr>()
                    .map_err(|_| "experiment destination must be an IP:port socket")?;
            }
            ExperimentOutput::IqCapture { artifact_name } => {
                validate_id("artifact name", artifact_name)?
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceCommand {
    Submit {
        experiment: ExperimentRequest,
    },
    Cancel {
        session_id: String,
    },
    /// Replace the tuning of the active session while preserving its owner,
    /// output route, radio claim, and original end time.
    Retune {
        session_id: String,
        center_frequency_hz: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gain_db: Option<f32>,
    },
    Status,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStatus {
    pub schema_version: u32,
    pub active: Option<ActiveTuning>,
    pub queued_session_ids: Vec<String>,
    pub observed_at_ms: u64,
    pub error: Option<String>,
}

impl SpectrumFrame {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CONTRACT_VERSION {
            return Err("unsupported spectrum schema".into());
        }
        validate_id("session id", &self.session_id)?;
        if self.generation == 0 || self.observed_at_ms == 0 || self.center_frequency_hz == 0 {
            return Err("spectrum frame has invalid identity or timing".into());
        }
        if self.span_hz == 0 || u64::from(self.span_hz) > self.center_frequency_hz * 2 {
            return Err("spectrum span is invalid".into());
        }
        if !self.floor_dbfs.is_finite()
            || !self.ceiling_dbfs.is_finite()
            || self.floor_dbfs < -240.0
            || self.ceiling_dbfs > 40.0
            || self.floor_dbfs >= self.ceiling_dbfs
        {
            return Err("spectrum power range is invalid".into());
        }
        if !(MIN_SPECTRUM_BINS..=MAX_SPECTRUM_BINS).contains(&self.bins_dbfs.len())
            || self.bins_dbfs.iter().any(|power| !power.is_finite())
        {
            return Err("spectrum bins are invalid or unbounded".into());
        }
        Ok(())
    }

    pub fn bin_frequency_hz(&self, index: usize) -> Option<f64> {
        if index >= self.bins_dbfs.len() {
            return None;
        }
        let start = self.center_frequency_hz as f64 - f64::from(self.span_hz) / 2.0;
        let width = f64::from(self.span_hz) / self.bins_dbfs.len() as f64;
        Some(start + (index as f64 + 0.5) * width)
    }
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
