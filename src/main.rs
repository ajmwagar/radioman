use radioman::RadioDescriptor;
use serde::Deserialize;
use std::{env, fs, process::ExitCode};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema_version: u32,
    node_id: String,
    radio: RadioDescriptor,
    iq_bind: String,
    iq_advertise: String,
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != radioman::CONTRACT_VERSION {
            return Err("unsupported Radioman config schema".into());
        }
        if self.node_id.trim().is_empty() || self.node_id.len() > radioman::MAX_ID_BYTES {
            return Err("invalid node id".into());
        }
        self.radio.validate()?;
        if !valid_socket(&self.iq_bind) {
            return Err("iq_bind must be an IP:port socket".into());
        }
        if !(self.iq_advertise.starts_with("udp://") || self.iq_advertise.starts_with("quic://")) {
            return Err("iq_advertise must use udp:// or quic://".into());
        }
        Ok(())
    }
}

fn valid_socket(value: &str) -> bool {
    value.parse::<std::net::SocketAddr>().is_ok()
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 3 || args[1] != "validate-config" {
        return Err(format!("usage: {} validate-config CONFIG.toml", args[0]));
    }
    let text = fs::read_to_string(&args[2]).map_err(|error| format!("read config: {error}"))?;
    let config: Config = toml::from_str(&text).map_err(|error| format!("parse config: {error}"))?;
    config.validate()?;
    println!("valid node={} radio={}", config.node_id, config.radio.id);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("radioman: {error}");
            ExitCode::FAILURE
        }
    }
}
