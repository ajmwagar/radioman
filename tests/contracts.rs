use radioman::{
    Agent, DeviceIndex, ExperimentOutput, ExperimentRequest, FrequencyRange, IqStreamAnnouncement,
    PacketRadioCapability, PacketRadioDescriptor, RadioDescriptor, RadioKind, RxRequest,
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

#[test]
fn experiments_are_timed_and_outputs_are_narrow() {
    let mut rx = request("adsb-evening");
    rx.center_frequency_hz = 1_090_000_000;
    rx.sample_rate_hz = 2_048_000;
    rx.duration_ms = Some(60_000);
    let experiment = ExperimentRequest {
        owner: "canvas-neo".into(),
        start_at_ms: 1_000,
        rx,
        output: ExperimentOutput::Spectrum {
            destination: "192.168.10.82:50070".into(),
        },
    };
    experiment.validate_for(&rtl()).unwrap();
    let tuning = experiment.active_tuning();
    assert_eq!(tuning.owner, "canvas-neo");
    assert_eq!(tuning.session_id, "adsb-evening");
    assert_eq!(tuning.center_frequency_hz, 1_090_000_000);
    assert_eq!(tuning.ends_at_ms, 61_000);
    let mut invalid = experiment;
    invalid.rx.duration_ms = None;
    assert!(invalid.validate_for(&rtl()).is_err());
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
fn heterogeneous_devices_share_one_unique_index() {
    let hackrf = RadioDescriptor {
        id: "hackrf/78d063dc29876f67".into(),
        kind: RadioKind::HackRf,
        label: "Radioman HackRF One".into(),
        serial: Some("000000000000000078d063dc29876f67".into()),
        frequency: FrequencyRange {
            minimum_hz: 1_000_000,
            maximum_hz: 6_000_000_000,
        },
        sample_rates_hz: BTreeSet::from([2_000_000, 8_000_000, 10_000_000, 20_000_000]),
        sample_formats: BTreeSet::from([SampleFormat::Cs8]),
        receive: true,
        transmit: false,
    };
    let modem = PacketRadioDescriptor {
        id: "lora/48ca435bacc8".into(),
        label: "Radioman T-Beam Supreme".into(),
        serial: "48:CA:43:5B:AC:C8".into(),
        transport:
            "/dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit_48:CA:43:5B:AC:C8-if00"
                .into(),
        protocol: "radioman-node.v1".into(),
        capabilities: BTreeSet::from([PacketRadioCapability::Health]),
    };
    let mut index = DeviceIndex {
        schema_version: 1,
        node_id: "radioman-pi".into(),
        radios: vec![rtl(), hackrf],
        packet_radios: vec![modem],
    };
    index.validate().unwrap();

    index.packet_radios[0].id = index.radios[0].id.clone();
    assert_eq!(index.validate().unwrap_err(), "device ids must be unique");
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
        tuning: None,
    };
    frame.validate().unwrap();
    assert_eq!(frame.bin_frequency_hz(0), Some(1_089_001_000.0));
    assert_eq!(frame.bin_frequency_hz(999), Some(1_090_999_000.0));

    let mut unbounded = frame.clone();
    unbounded.bins_dbfs = vec![0.0; 4_097];
    assert!(unbounded.validate().is_err());
}
