//! Receive-only process adapters. Device drivers and FM demodulation stay in
//! hackrf_transfer/rtl_sdr/rx_fm/rtl_fm; every caller shares these validations.
use crate::{RadioDescriptor, RadioKind, SampleFormat};
use std::{
    fs::{File, OpenOptions},
    process::{Child, Command, Stdio},
};

pub fn claim(radio: &RadioDescriptor) -> Result<File, String> {
    // All Radioman receiver commands share one advisory lock per physical radio.
    // The daemon creates /run/radioman; run manual commands as its service user.
    let id = radio.serial.as_deref().unwrap_or(&radio.id);
    let name: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(format!("/run/radioman/receiver-{name}.lock"))
        .map_err(|e| format!("open receiver lock (run as radioman): {e}"))?;
    file.try_lock()
        .map_err(|e| format!("radio {} is already claimed or lock failed: {e}", radio.id))?;
    Ok(file)
}

pub fn spectrum_format(radio: &RadioDescriptor) -> Result<(u32, SampleFormat), String> {
    let profile = match radio.kind {
        RadioKind::RtlSdr => (2_048_000, SampleFormat::Cu8),
        RadioKind::HackRf => (2_000_000, SampleFormat::Cs8),
        RadioKind::Other => return Err("unsupported receive backend".into()),
    };
    if !radio.receive
        || !radio.sample_rates_hz.contains(&profile.0)
        || !radio.sample_formats.contains(&profile.1)
    {
        return Err("radio does not declare the backend receive rate and format".into());
    }
    Ok(profile)
}

pub fn validate(radio: &RadioDescriptor, frequency: u64, gain: f32) -> Result<(), String> {
    spectrum_format(radio)?;
    if !radio.frequency.contains(frequency) {
        return Err("receive frequency is outside the configured radio range".into());
    }
    if !gain.is_finite() || !(0.0..=49.6).contains(&gain) {
        return Err("receive gain must be between 0 and 49.6 dB".into());
    }
    if matches!(radio.kind, RadioKind::HackRf)
        && radio.serial.as_deref().is_none_or(|serial| {
            serial.is_empty() || serial.len() > 32 || !serial.bytes().all(|b| b.is_ascii_hexdigit())
        })
    {
        return Err("HackRF requires an explicit hexadecimal serial number".into());
    }
    Ok(())
}

pub fn iq_command(radio: &RadioDescriptor, frequency: u64, gain: f32) -> Result<Command, String> {
    validate(radio, frequency, gain)?;
    let (rate, _) = spectrum_format(radio)?;
    let serial = radio.serial.as_deref().unwrap_or("0");
    let mut command = match radio.kind {
        RadioKind::RtlSdr => {
            let mut command = Command::new("rtl_sdr");
            command.args([
                "-d",
                serial,
                "-f",
                &frequency.to_string(),
                "-s",
                &rate.to_string(),
                "-g",
                &gain.to_string(),
                "-",
            ]);
            command
        }
        RadioKind::HackRf => {
            let mut command = Command::new("hackrf_transfer");
            // RX only. Never enable antenna power or the RF amplifier implicitly.
            // Gain denotes VGA gain, quantized to the nearest hardware 2 dB step.
            command.args([
                "-d",
                serial,
                "-r",
                "-",
                "-f",
                &frequency.to_string(),
                "-s",
                &rate.to_string(),
                "-l",
                "16",
                "-g",
                &((gain / 2.0).round() * 2.0).to_string(),
                "-a",
                "0",
                "-p",
                "0",
            ]);
            command
        }
        RadioKind::Other => unreachable!(),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    Ok(command)
}

pub fn nfm_command(radio: &RadioDescriptor, frequency: u64, gain: f32) -> Result<Command, String> {
    validate(radio, frequency, gain)?;
    let mut command = match radio.kind {
        RadioKind::RtlSdr => {
            let mut command = Command::new("rtl_fm");
            command.args([
                "-d",
                radio.serial.as_deref().unwrap_or("0"),
                "-g",
                &gain.to_string(),
            ]);
            command
        }
        RadioKind::HackRf => {
            let mut command = Command::new("rx_fm");
            command.args([
                "-d",
                &format!("driver=hackrf,serial={}", radio.serial.as_deref().unwrap()),
                "-g",
                &format!("LNA=16,VGA={},AMP=0", (gain / 2.0).round() * 2.0),
            ]);
            command
        }
        RadioKind::Other => unreachable!(),
    };
    command.args([
        "-f",
        &frequency.to_string(),
        "-M",
        "fm",
        "-s",
        "24000",
        "-r",
        "24000",
        "-",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    Ok(command)
}

/// Ensure a failed consumer releases its USB receiver instead of orphaning it.
pub struct Receiver(pub Child);
impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hackrf() -> RadioDescriptor {
        serde_json::from_value(serde_json::json!({
            "id":"hackrf/test", "kind":"hack_rf", "label":"Test", "serial":"abc123",
            "frequency":{"minimum_hz":1000000,"maximum_hz":6000000000_u64},
            "sample_rates_hz":[2000000],"sample_formats":["cs8"],"receive":true,"transmit":false
        }))
        .unwrap()
    }
    #[test]
    fn hackrf_commands_only_receive_and_select_the_physical_device() {
        let radio = hackrf();
        let iq = iq_command(&radio, 1_694_100_000, 19.7).unwrap();
        let args: Vec<_> = iq.get_args().map(|s| s.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|v| v == ["-r", "-"]));
        assert!(args.windows(2).any(|v| v == ["-d", "abc123"]));
        assert!(args.windows(2).any(|v| v == ["-g", "20"]));
        assert!(args.windows(2).any(|v| v == ["-a", "0"]));
        assert!(args.windows(2).any(|v| v == ["-p", "0"]));
        assert!(!args.contains(&"-t"));
        assert_eq!(
            spectrum_format(&radio).unwrap(),
            (2_000_000, SampleFormat::Cs8)
        );
        let fm = nfm_command(&radio, 162_550_000, 20.0).unwrap();
        let args: Vec<_> = fm.get_args().map(|s| s.to_str().unwrap()).collect();
        assert!(args.windows(2).any(|v| v == ["-M", "fm"]));
        assert!(
            args.windows(2)
                .any(|v| v == ["-d", "driver=hackrf,serial=abc123"])
        );
        assert!(args.windows(2).any(|v| v == ["-r", "24000"]));
    }
    #[test]
    fn invalid_receive_requests_never_launch_a_driver() {
        let mut radio = hackrf();
        for gain in [f32::NAN, f32::INFINITY, -1.0, 50.0] {
            assert!(iq_command(&radio, 162_550_000, gain).is_err());
        }
        assert!(nfm_command(&radio, 7_000_000_000, 20.0).is_err());
        radio.receive = false;
        assert!(iq_command(&radio, 162_550_000, 20.0).is_err());
        radio.receive = true;
        radio.serial = None;
        assert!(nfm_command(&radio, 162_550_000, 20.0).is_err());
        radio.serial = Some("abc123".into());
        radio.sample_formats.clear();
        assert!(spectrum_format(&radio).is_err());
    }
}
