use radioman::RadioDescriptor;
use serde::Deserialize;
use std::{
    env, fs,
    io::Read,
    net::{SocketAddr, UdpSocket},
    process::{Command, ExitCode, Stdio},
};

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

fn read_config(path: &str) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("read config: {error}"))?;
    let config: Config = toml::from_str(&text).map_err(|error| format!("parse config: {error}"))?;
    config.validate()?;
    Ok(config)
}

fn mono_s16le_to_stereo_f32le(input: &[u8], output: &mut Vec<u8>) -> Result<(), String> {
    if !input.len().is_multiple_of(2) {
        return Err("rtl_fm produced a partial s16le sample".into());
    }
    output.clear();
    output.reserve(input.len() * 4);
    for sample in input.chunks_exact(2) {
        let value = f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32_768.0;
        output.extend_from_slice(&value.to_le_bytes());
        output.extend_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

fn receive_fm(
    config: &Config,
    center_frequency_hz: u64,
    pcm_destination: SocketAddr,
    gain_db: f32,
) -> Result<(), String> {
    if !config.radio.frequency.contains(center_frequency_hz) {
        return Err("FM center frequency is outside the configured radio range".into());
    }
    if !gain_db.is_finite() || !(0.0..=49.6).contains(&gain_db) {
        return Err("RTL-SDR gain must be between 0 and 49.6 dB".into());
    }
    let serial = config.radio.serial.as_deref().unwrap_or("0");
    let mut child = Command::new("rtl_fm")
        .args([
            "-d",
            serial,
            "-f",
            &center_frequency_hz.to_string(),
            "-M",
            "wbfm",
            "-s",
            "200000",
            "-r",
            "48000",
            "-E",
            "deemp",
            "-g",
            &gain_db.to_string(),
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start rtl_fm: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("rtl_fm stdout is unavailable")?;
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|error| format!("bind PCM socket: {error}"))?;
    socket
        .connect(pcm_destination)
        .map_err(|error| format!("connect PCM socket: {error}"))?;
    let mut input = [0_u8; 960];
    let mut output = Vec::with_capacity(input.len() * 4);
    eprintln!(
        "radioman: receiving WBFM center={}Hz gain={gain_db:.1}dB pcm={pcm_destination}",
        center_frequency_hz
    );
    loop {
        let size = stdout
            .read(&mut input)
            .map_err(|error| format!("read rtl_fm PCM: {error}"))?;
        if size == 0 {
            let status = child
                .wait()
                .map_err(|error| format!("wait for rtl_fm: {error}"))?;
            return Err(format!("rtl_fm exited with {status}"));
        }
        let whole = size - size % 2;
        mono_s16le_to_stereo_f32le(&input[..whole], &mut output)?;
        socket
            .send(&output)
            .map_err(|error| format!("send PCM datagram: {error}"))?;
    }
}

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, command, path] if command == "validate-config" => {
            let config = read_config(path)?;
            println!("valid node={} radio={}", config.node_id, config.radio.id);
            Ok(())
        }
        [_, command, path, frequency, destination]
        | [_, command, path, frequency, destination, _]
            if command == "fm" =>
        {
            let config = read_config(path)?;
            let frequency = frequency
                .parse::<u64>()
                .map_err(|error| format!("invalid center frequency: {error}"))?;
            let destination = destination
                .parse::<SocketAddr>()
                .map_err(|error| format!("invalid PCM destination: {error}"))?;
            let gain = args
                .get(5)
                .map(|value| value.parse::<f32>())
                .transpose()
                .map_err(|error| format!("invalid gain: {error}"))?
                .unwrap_or(19.7);
            receive_fm(&config, frequency, destination, gain)
        }
        _ => Err(format!(
            "usage:\n  {} validate-config CONFIG.toml\n  {} fm CONFIG.toml CENTER_HZ PCM_DEST [GAIN_DB]",
            args[0], args[0]
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::mono_s16le_to_stereo_f32le;

    #[test]
    fn mono_s16_becomes_bounded_stereo_f32() {
        let mut output = Vec::new();
        mono_s16le_to_stereo_f32le(&[0, 0, 0xff, 0x7f, 0, 0x80], &mut output).unwrap();
        let samples = output
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(samples[0], 0.0);
        assert_eq!(samples[0], samples[1]);
        assert_eq!(samples[2], samples[3]);
        assert_eq!(samples[4], samples[5]);
        assert!(samples[2] < 1.0 && samples[2] > 0.99);
        assert_eq!(samples[4], -1.0);
    }

    #[test]
    fn partial_sample_fails_loud() {
        assert!(mono_s16le_to_stereo_f32le(&[1], &mut Vec::new()).is_err());
    }
}
