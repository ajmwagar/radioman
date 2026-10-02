use radioman::{
    Agent, FrequencyRange, IqStreamAnnouncement, RadioDescriptor, RadioKind, RxRequest,
    SampleFormat, SessionPhase, SpectrumFrame, StopRequest,
};
use std::collections::BTreeSet;

fn rtl() -> RadioDescriptor {
    RadioDescriptor {
        id: "rtl-sdr/00000001".into(),
        kind: RadioKind::RtlSdr,
        label: "Radioman RTL-SDR".into(),
        serial: Some("00000001".into()),
        frequency: FrequencyRange {
            minimum_hz: 24_000_000,
            maximum_hz: 1_766_000_000,
        },
        sample_rates_hz: BTreeSet::from([1_024_000, 2_048_000, 2_400_000]),
        sample_formats: BTreeSet::from([SampleFormat::Cu8]),
        receive: true,
        transmit: false,
    }
}

fn request(session_id: &str) -> RxRequest {
    RxRequest {
        session_id: session_id.into(),
        radio_id: "rtl-sdr/00000001".into(),
        center_frequency_hz: 100_000_000,
        sample_rate_hz: 2_400_000,
        sample_format: SampleFormat::Cu8,
        gain_db: Some(20.0),
        ppm: 0,
        duration_ms: None,
    }
}

#[test]
fn one_session_exclusively_owns_the_radio() {
    let mut agent = Agent::new("radioman-pi".into(), rtl()).unwrap();
    let starting = agent.start_rx(request("fm"), 10).unwrap();
    assert_eq!(starting.phase, SessionPhase::Starting);
    assert!(agent.start_rx(request("other"), 11).is_err());
    assert!(
        agent
            .stop(
                StopRequest {
                    session_id: "other".into()
                },
                12
            )
            .is_err()
    );
    let stopped = agent
        .stop(
            StopRequest {
                session_id: "fm".into(),
            },
            13,
        )
        .unwrap();
    assert_eq!(stopped.phase, SessionPhase::Stopped);
    assert!(agent.start_rx(request("other"), 14).is_ok());
}

#[test]
fn invalid_tuning_fails_before_hardware_is_touched() {
    let radio = rtl();
    let mut invalid = request("bad");
    invalid.center_frequency_hz = 2_000_000_000;
    assert_eq!(
        invalid.validate_for(&radio).unwrap_err(),
        "center frequency is outside the radio range"
    );
}

#[test]
fn iq_announcement_is_expiring_and_reports_wire_rate() {
    let announcement = IqStreamAnnouncement {
        schema_version: 1,
        session_id: "fm".into(),
        generation: 1,
        endpoint: "udp://192.168.2.80:50060".into(),
        sample_format: SampleFormat::Cu8,
        sample_rate_hz: 2_400_000,
        center_frequency_hz: 100_000_000,
        expires_at_ms: 20_000,
    };
    announcement.validate(10_000).unwrap();
    assert_eq!(announcement.payload_bytes_per_second(), 4_800_000);
    assert!(announcement.validate(20_000).is_err());
}

#[test]
fn rtl_sdr_cannot_claim_transmit() {
    let mut radio = rtl();
    radio.transmit = true;
    assert_eq!(
        radio.validate().unwrap_err(),
        "RTL-SDR cannot advertise transmit capability"
    );
}

#[test]
fn spectrum_is_bounded_and_frequency_axis_is_derived() {
    let frame = SpectrumFrame {
        schema_version: 1,
        session_id: "adsb".into(),
        generation: 2,
        sequence: 10,
        observed_at_ms: 1_000,
        center_frequency_hz: 1_090_000_000,
        span_hz: 2_000_000,
        floor_dbfs: -100.0,
        ceiling_dbfs: -10.0,
        bins_dbfs: vec![-80.0; 1_000],
    };
    frame.validate().unwrap();
    assert_eq!(frame.bin_frequency_hz(0), Some(1_089_001_000.0));
    assert_eq!(frame.bin_frequency_hz(999), Some(1_090_999_000.0));

    let mut unbounded = frame.clone();
    unbounded.bins_dbfs = vec![0.0; 4_097];
    assert!(unbounded.validate().is_err());
}
