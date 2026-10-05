#![cfg(feature = "lua")]
use radioman::decoder::{DecodedPacket, lua::PacketPolicy};

const SCRIPT: &str = r#"
return {
  config = { decoder = 'fixture', source = 'roof-rtl', frequency_hz = 1090000000, sample_rate_hz = 2400000 },
  on_packet = function(packet)
    if packet.value.keep then return 'radio/observations' end
    return nil
  end
}
"#;

fn packet() -> DecodedPacket {
    DecodedPacket {
        source: "roof-rtl".into(),
        session_id: "rx-1".into(),
        sequence: 1,
        observed_at_unix_ms: 1000,
        contract: "radioman.fixture.v1".into(),
        value: serde_json::json!({"keep":true}),
    }
}

#[test]
fn routes_without_mutating_reception_metadata() {
    let policy = PacketPolicy::load(SCRIPT).unwrap();
    let mut observation = packet();
    assert_eq!(
        policy.route(&observation).unwrap().as_deref(),
        Some("radio/observations")
    );
    observation.value["keep"] = false.into();
    assert_eq!(policy.route(&observation).unwrap(), None);
    observation.source = "other-receiver".into();
    assert!(policy.route(&observation).is_err());
}

#[test]
fn reload_is_transactional_and_retuning_is_explicit() {
    let mut policy = PacketPolicy::load(SCRIPT).unwrap();
    assert!(policy.reload("broken lua").is_err());
    assert!(
        policy
            .reload(&SCRIPT.replace("1090000000", "1080000000"))
            .is_err()
    );
    assert_eq!(
        policy.route(&packet()).unwrap().as_deref(),
        Some("radio/observations")
    );
    policy
        .reload(&SCRIPT.replace("radio/observations", "radio/filtered"))
        .unwrap();
    assert_eq!(
        policy.route(&packet()).unwrap().as_deref(),
        Some("radio/filtered")
    );
}

#[test]
fn bounded_scripts_fail_visibly() {
    assert!(PacketPolicy::load("while true do end").is_err());
    let looping = SCRIPT.replace(
        "if packet.value.keep then return 'radio/observations' end",
        "while true do end",
    );
    assert!(
        PacketPolicy::load(&looping)
            .unwrap()
            .route(&packet())
            .is_err()
    );
    let oversized = SCRIPT.replace(
        "return 'radio/observations'",
        "return string.rep('x', 5000000)",
    );
    assert!(
        PacketPolicy::load(&oversized)
            .unwrap()
            .route(&packet())
            .is_err()
    );
    let unavailable = SCRIPT.replace(
        "return 'radio/observations'",
        "return io.open('/etc/passwd')",
    );
    assert!(
        PacketPolicy::load(&unavailable)
            .unwrap()
            .route(&packet())
            .is_err()
    );
}

#[test]
fn packet_and_routing_contracts_are_checked() {
    let policy = PacketPolicy::load(SCRIPT).unwrap();
    let mut invalid = packet();
    invalid.sequence = 0;
    assert!(policy.route(&invalid).is_err());
    invalid = packet();
    invalid.value = serde_json::json!({"large":"x".repeat(65536)});
    assert!(policy.route(&invalid).is_err());
    let invalid_route = SCRIPT.replace("radio/observations", "../ bad stream");
    assert!(
        PacketPolicy::load(&invalid_route)
            .unwrap()
            .route(&packet())
            .is_err()
    );
}
