use radioman::{
    Agent, DeviceIndex, ExperimentOutput, ExperimentRequest, FrequencyRange, IqStreamAnnouncement,
    PacketRadioCapability, PacketRadioDescriptor, RadioDescriptor, RadioKind, RxRequest,
    SampleFormat, ServiceCommand, ServiceStatus, SessionPhase, SpectrumCensus, SpectrumFrame,
    StopRequest,
};
use std::collections::{BTreeMap, BTreeSet};

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
    assert_eq!(tuning.radio_id, "rtl-sdr/00000001");
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

#[test]
fn spectrum_census_retains_mean_peak_and_occupancy() {
    let mut frame = SpectrumFrame {
        schema_version: 1,
        session_id: "survey".into(),
        generation: 1,
        sequence: 1,
        observed_at_ms: 1_000,
        center_frequency_hz: 100_000_000,
        span_hz: 2_000_000,
        floor_dbfs: -100.0,
        ceiling_dbfs: -10.0,
        bins_dbfs: vec![-90.0; 64],
        tuning: None,
    };
    let mut census = SpectrumCensus::new(1_000, 4, -70.0).unwrap();
    census.observe(&frame).unwrap();
    frame.sequence = 2;
    frame.observed_at_ms = 2_000;
    frame.bins_dbfs[0..16].fill(-50.0);
    census.observe(&frame).unwrap();
    let snapshot = census.snapshot(&frame).unwrap();
    assert_eq!(snapshot.frame_count, 2);
    assert_eq!(snapshot.mean_dbfs, vec![-70.0, -90.0, -90.0, -90.0]);
    assert_eq!(snapshot.peak_dbfs, vec![-50.0, -90.0, -90.0, -90.0]);
    assert_eq!(snapshot.occupancy, vec![0.5, 0.0, 0.0, 0.0]);
}

#[test]
fn retune_command_is_narrow_and_typed() {
    let command: ServiceCommand = serde_json::from_value(serde_json::json!({
        "command": "retune",
        "session_id": "ism-915-live",
        "center_frequency_hz": 433_920_000,
        "gain_db": 17.4
    }))
    .unwrap();
    assert!(matches!(
        command,
        ServiceCommand::Retune {
            session_id,
            center_frequency_hz: 433_920_000,
            gain_db: Some(gain)
        } if session_id == "ism-915-live" && (gain - 17.4).abs() < f32::EPSILON
    ));
}

#[test]
fn dcp_catalog_changes_actions_with_tuner_ownership() {
    let devices = DeviceIndex {
        schema_version: 1,
        node_id: "radioman-pi".into(),
        radios: vec![rtl()],
        packet_radios: vec![],
    };
    let idle = ServiceStatus {
        schema_version: 1,
        active: None,
        queued_session_ids: vec![],
        observed_at_ms: 1_000,
        error: None,
    };
    let idle_projection = radioman::dcp::Projection {
        devices: &devices,
        status: &idle,
        spectrum_destination: "192.168.2.3:50070",
    };
    let idle_catalog = idle_projection.catalog().unwrap();
    assert_eq!(idle_catalog.actions.len(), 1);
    assert_eq!(idle_catalog.actions[0].id, "radioman.spectrum.start");

    let running = ServiceStatus {
        schema_version: 1,
        active: Some(
            ExperimentRequest {
                owner: "dcp".into(),
                start_at_ms: 1_000,
                rx: RxRequest {
                    duration_ms: Some(60_000),
                    ..request("fm-live")
                },
                output: ExperimentOutput::Spectrum {
                    destination: "192.168.2.3:50070".into(),
                },
            }
            .active_tuning(),
        ),
        queued_session_ids: vec![],
        observed_at_ms: 2_000,
        error: None,
    };
    let running_projection = radioman::dcp::Projection {
        devices: &devices,
        status: &running,
        spectrum_destination: "192.168.2.3:50070",
    };
    let running_catalog = running_projection.catalog().unwrap();
    assert_eq!(
        running_catalog
            .actions
            .iter()
            .map(|action| action.id.as_str())
            .collect::<Vec<_>>(),
        ["radioman.tuner.retune", "radioman.session.stop"]
    );
    assert_ne!(
        idle_catalog.catalog_revision,
        running_catalog.catalog_revision
    );
    assert_ne!(idle_catalog.state_revision, running_catalog.state_revision);
}

#[test]
fn dcp_execution_rejects_stale_state_before_emitting_a_command() {
    let devices = DeviceIndex {
        schema_version: 1,
        node_id: "radioman-pi".into(),
        radios: vec![rtl()],
        packet_radios: vec![],
    };
    let status = ServiceStatus {
        schema_version: 1,
        active: None,
        queued_session_ids: vec![],
        observed_at_ms: 1_000,
        error: None,
    };
    let projection = radioman::dcp::Projection {
        devices: &devices,
        status: &status,
        spectrum_destination: "192.168.2.3:50070",
    };
    let catalog = projection.catalog().unwrap();
    let request = dcp::ExecuteRequest {
        dcp_version: dcp::VERSION.into(),
        request_id: "request-1".into(),
        decision_id: "decision-1".into(),
        action_id: "radioman.spectrum.start".into(),
        arguments: BTreeMap::from([
            ("session_id".into(), serde_json::json!("fm-live")),
            ("radio_id".into(), serde_json::json!("rtl-sdr/00000001")),
            ("center_frequency_hz".into(), serde_json::json!(100_000_000)),
            ("duration_ms".into(), serde_json::json!(60_000)),
        ]),
        phase: dcp::Phase::Commit,
        expected_catalog_revision: catalog.catalog_revision.clone(),
        expected_state_revision: "state-old".into(),
        prepared_receipt_id: None,
        confirmed: false,
        evidence: dcp::Evidence {
            utterance_id: "utterance-1".into(),
            transcript_revision: 3,
            final_: true,
        },
    };
    let stale = projection.command(&request).unwrap_err();
    assert_eq!(stale.code, dcp::ErrorCode::StaleState);

    let command = projection
        .command(&dcp::ExecuteRequest {
            expected_state_revision: catalog.state_revision,
            ..request
        })
        .unwrap();
    assert!(matches!(command, ServiceCommand::Submit { .. }));
}
