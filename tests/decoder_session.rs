use radioman::decoder::{DecodedPacket, Decoder, DecoderConfig, DecoderSession, MAX_BATCH_PACKETS};

struct Fixture {
    count: usize,
}
impl Decoder for Fixture {
    fn start(&mut self, _: &DecoderConfig) -> Result<(), String> {
        Ok(())
    }
    fn process(&mut self, _: &[u8]) -> Result<Vec<DecodedPacket>, String> {
        Ok(vec![
            DecodedPacket {
                source: "fixture-rx".into(),
                session_id: "rx-1".into(),
                sequence: 1,
                observed_at_unix_ms: 1,
                contract: "fixture.v1".into(),
                value: serde_json::json!({})
            };
            self.count
        ])
    }
    fn stop(&mut self) -> Result<(), String> {
        Ok(())
    }
}
fn config() -> DecoderConfig {
    DecoderConfig {
        decoder: "fixture".into(),
        source: "fixture-rx".into(),
        frequency_hz: 1090000000,
        sample_rate_hz: 2400000,
    }
}

#[test]
fn lifecycle_and_batch_bounds() {
    let mut session = DecoderSession::new(Fixture { count: 1 });
    assert!(session.process(&[]).is_err());
    assert!(session.stop().is_err());
    session.start(config()).unwrap();
    assert!(session.start(config()).is_err());
    assert_eq!(session.process(&[]).unwrap().len(), 1);
    assert!(session.process(&vec![0; 1024 * 1024 + 1]).is_err());
    session.stop().unwrap();
    assert!(session.process(&[]).is_err());
    let mut oversized = DecoderSession::new(Fixture {
        count: MAX_BATCH_PACKETS + 1,
    });
    oversized.start(config()).unwrap();
    assert!(oversized.process(&[]).is_err());
}
