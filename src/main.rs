use radioman::{
    DeviceIndex, ExperimentOutput, ExperimentRequest, PacketRadioDescriptor, RadioDescriptor,
    RadioKind, ServiceCommand, ServiceStatus, SpectrumFrame,
};
use rustfft::{FftPlanner, num_complex::Complex32};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    net::{SocketAddr, UdpSocket},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    process::{Child, Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use stream_descriptors::{
    EndpointAnnouncementV1, Freshness, MediaCharacteristics, SCHEMA_VERSION, StreamDescriptorV1,
    TransportEndpoint,
};
use subtle::ConstantTimeEq;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

struct ActiveExperiment {
    request: ExperimentRequest,
    child: Child,
    ends_at_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn service_status(
    active: &Option<ActiveExperiment>,
    queued: &BTreeMap<String, ExperimentRequest>,
    error: Option<String>,
) -> ServiceStatus {
    ServiceStatus {
        schema_version: 1,
        active: active.as_ref().map(|v| v.request.active_tuning()),
        queued_session_ids: queued.keys().cloned().collect(),
        observed_at_ms: now_ms(),
        error,
    }
}

fn overlaps(a: &ExperimentRequest, b: &ExperimentRequest) -> bool {
    let a_end = a.start_at_ms.saturating_add(a.rx.duration_ms.unwrap_or(0));
    let b_end = b.start_at_ms.saturating_add(b.rx.duration_ms.unwrap_or(0));
    a.start_at_ms < b_end && b.start_at_ms < a_end
}

fn spawn_experiment(
    config_path: &str,
    request: &ExperimentRequest,
) -> Result<ActiveExperiment, String> {
    let ExperimentOutput::Spectrum { destination } = &request.output else {
        return Err("this backend currently executes spectrum experiments only".into());
    };
    if request.rx.sample_rate_hz != 2_048_000 {
        return Err("spectrum experiments require 2048000 samples/s".into());
    }
    let executable = env::current_exe().map_err(|e| format!("locate radioman: {e}"))?;
    let child = Command::new("setsid")
        .arg(executable)
        .args(["spectrum", config_path])
        .arg(request.rx.center_frequency_hz.to_string())
        .arg(destination)
        .arg(request.rx.gain_db.unwrap_or(19.7).to_string())
        .arg(
            serde_json::to_string(&request.active_tuning())
                .map_err(|e| format!("encode active tuning: {e}"))?,
        )
        .arg(&request.rx.radio_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start spectrum experiment: {e}"))?;
    Ok(ActiveExperiment {
        request: request.clone(),
        child,
        ends_at_ms: request
            .start_at_ms
            .saturating_add(request.rx.duration_ms.unwrap_or(0)),
    })
}

fn terminate(active: &mut ActiveExperiment) {
    let _ = Command::new("kill")
        .args(["-TERM", &format!("-{}", active.child.id())])
        .status();
    let _ = active.child.wait();
}

fn serve(config_path: &str, socket_path: &str) -> Result<(), String> {
    let config = read_config(config_path)?;
    if let Ok(metadata) = fs::symlink_metadata(socket_path) {
        if !metadata.file_type().is_socket() {
            return Err("refusing to replace non-socket control path".into());
        }
        fs::remove_file(socket_path).map_err(|e| format!("remove stale control socket: {e}"))?;
    }
    let listener =
        UnixListener::bind(socket_path).map_err(|e| format!("bind control socket: {e}"))?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o660))
        .map_err(|e| format!("secure control socket: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("configure control socket: {e}"))?;
    let mut queued = BTreeMap::<String, ExperimentRequest>::new();
    let mut active: Option<ActiveExperiment> = None;
    eprintln!("radioman: tuner service control={socket_path}");
    loop {
        if let Some(running) = active.as_mut() {
            let finished = running
                .child
                .try_wait()
                .map_err(|e| format!("poll experiment: {e}"))?
                .is_some();
            if finished || now_ms() >= running.ends_at_ms {
                if !finished {
                    terminate(running);
                }
                active = None;
            }
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut input = Vec::new();
                stream
                    .read_to_end(&mut input)
                    .map_err(|e| format!("read command: {e}"))?;
                let status = match serde_json::from_slice::<ServiceCommand>(&input) {
                    Ok(ServiceCommand::Status) => service_status(&active, &queued, None),
                    Ok(ServiceCommand::Cancel { session_id }) => {
                        if active
                            .as_ref()
                            .is_some_and(|v| v.request.rx.session_id == session_id)
                        {
                            terminate(active.as_mut().unwrap());
                            active = None;
                        } else {
                            queued.remove(&session_id);
                        }
                        service_status(&active, &queued, None)
                    }
                    Ok(ServiceCommand::Retune {
                        session_id,
                        center_frequency_hz,
                        gain_db,
                    }) => {
                        let result = (|| {
                            let running =
                                active.as_ref().ok_or("no active experiment to retune")?;
                            if running.request.rx.session_id != session_id {
                                return Err(format!(
                                    "session does not own the active tuner: {session_id}"
                                ));
                            }
                            let now = now_ms();
                            let remaining = running.ends_at_ms.saturating_sub(now);
                            if remaining == 0 {
                                return Err("active experiment has already expired".into());
                            }
                            let mut replacement = running.request.clone();
                            replacement.start_at_ms = now;
                            replacement.rx.center_frequency_hz = center_frequency_hz;
                            replacement.rx.duration_ms = Some(remaining);
                            if gain_db.is_some() {
                                replacement.rx.gain_db = gain_db;
                            }
                            let radio = config.radio(&replacement.rx.radio_id)?;
                            replacement.validate_for(radio)?;

                            // Validation happens before disrupting the current
                            // receiver. rtl_sdr cannot retune in place, so this
                            // backend performs one explicit process handoff.
                            terminate(active.as_mut().expect("active checked above"));
                            active = Some(spawn_experiment(config_path, &replacement)?);
                            Ok(())
                        })();
                        service_status(&active, &queued, result.err())
                    }
                    Ok(ServiceCommand::Submit { mut experiment }) => {
                        if experiment.start_at_ms == 0 {
                            experiment.start_at_ms = now_ms();
                        }
                        let result = config
                            .radio(&experiment.rx.radio_id)
                            .and_then(|radio| {
                                if !matches!(radio.kind, RadioKind::RtlSdr) {
                                    return Err(
                                        "this backend currently executes RTL-SDR experiments only"
                                            .into(),
                                    );
                                }
                                experiment.validate_for(radio)
                            })
                            .and_then(|_| {
                                if !matches!(experiment.output, ExperimentOutput::Spectrum { .. }) {
                                    return Err(
                                        "unsupported experiment output on this backend".into()
                                    );
                                }
                                if active
                                    .as_ref()
                                    .is_some_and(|v| overlaps(&v.request, &experiment))
                                    || queued.values().any(|v| overlaps(v, &experiment))
                                {
                                    return Err(
                                        "experiment overlaps an existing tuner claim".into()
                                    );
                                }
                                if queued.contains_key(&experiment.rx.session_id) {
                                    return Err("duplicate experiment session id".into());
                                }
                                queued.insert(experiment.rx.session_id.clone(), experiment);
                                Ok(())
                            });
                        service_status(&active, &queued, result.err())
                    }
                    Err(e) => service_status(
                        &active,
                        &queued,
                        Some(format!("invalid service command: {e}")),
                    ),
                };
                stream
                    .write_all(
                        &serde_json::to_vec(&status)
                            .map_err(|e| format!("encode response: {e}"))?,
                    )
                    .map_err(|e| format!("write response: {e}"))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("accept command: {e}")),
        }
        if active.is_none() {
            let next = queued
                .iter()
                .filter(|(_, v)| v.start_at_ms <= now_ms())
                .min_by_key(|(_, v)| v.start_at_ms)
                .map(|(id, _)| id.clone());
            if let Some(id) = next {
                let request = queued.remove(&id).unwrap();
                active = Some(spawn_experiment(config_path, &request)?);
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn control(socket_path: &str, json: &str) -> Result<(), String> {
    let status = control_status(socket_path, json.as_bytes())?;
    println!(
        "{}",
        serde_json::to_string(&status).map_err(|e| format!("encode status: {e}"))?
    );
    Ok(())
}

fn control_status(socket_path: &str, command: &[u8]) -> Result<ServiceStatus, String> {
    let mut stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect tuner service: {e}"))?;
    stream
        .write_all(command)
        .map_err(|e| format!("send command: {e}"))?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("finish command: {e}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read response: {e}"))?;
    serde_json::from_slice(&response).map_err(|e| format!("decode service status: {e}"))
}

fn dcp_serve(
    config: &Config,
    socket_path: &str,
    listen: &str,
    spectrum_destination: &str,
    token_file: &str,
) -> Result<(), String> {
    let address = listen
        .parse::<SocketAddr>()
        .map_err(|e| format!("invalid DCP listen address: {e}"))?;
    if !address.ip().is_loopback() {
        return Err("Radioman DCP HTTP must bind loopback; publish remotely through Unibus".into());
    }
    spectrum_destination
        .parse::<SocketAddr>()
        .map_err(|e| format!("invalid DCP spectrum destination: {e}"))?;
    let token =
        fs::read_to_string(token_file).map_err(|e| format!("read DCP bearer token: {e}"))?;
    let token = token.trim().as_bytes().to_vec();
    if token.len() < 32 {
        return Err("DCP bearer token must contain at least 32 bytes".into());
    }
    let server = Server::http(address).map_err(|e| format!("bind Radioman DCP HTTP: {e}"))?;
    let devices = config.device_index();
    let mut receipts = BTreeMap::<String, (Vec<u8>, dcp::Receipt)>::new();
    eprintln!("radioman: DCP provider listening on http://{address}");
    for mut request in server.incoming_requests() {
        let supplied = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Authorization"))
            .and_then(|header| header.value.as_str().strip_prefix("Bearer "))
            .map(str::as_bytes)
            .unwrap_or_default();
        if supplied.len() != token.len() || supplied.ct_eq(&token).unwrap_u8() != 1 {
            respond_json(
                request,
                401,
                &serde_json::json!({"code":"unauthorized","message":"valid bearer authorization is required","retryable":false}),
            )?;
            continue;
        }
        let version = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("DCP-Version"))
            .map(|header| header.value.as_str());
        if version != Some(dcp::VERSION) {
            respond_json(
                request,
                406,
                &serde_json::json!({"code":"unsupported_version","message":"DCP-Version must be exactly 0.1","retryable":false}),
            )?;
            continue;
        }
        let path = request.url().split('?').next().unwrap_or(request.url());
        match (request.method(), path) {
            (&Method::Get, "/.well-known/dcp") => {
                let status = current_status(socket_path)?;
                let projection = radioman::dcp::Projection {
                    devices: &devices,
                    status: &status,
                    spectrum_destination,
                };
                respond_json(request, 200, &projection.discovery())?;
            }
            (&Method::Get, "/v1/decisions") => {
                let status = current_status(socket_path)?;
                let projection = radioman::dcp::Projection {
                    devices: &devices,
                    status: &status,
                    spectrum_destination,
                };
                match projection.catalog() {
                    Ok(catalog) => respond_json(request, 200, &catalog)?,
                    Err(error) => respond_json(
                        request,
                        503,
                        &serde_json::json!({"code":"internal_error","message":error,"retryable":true}),
                    )?,
                }
            }
            (&Method::Post, "/v1/decisions/execute") => {
                let mut body = Vec::new();
                request
                    .as_reader()
                    .take(1024 * 1024)
                    .read_to_end(&mut body)
                    .map_err(|e| format!("read DCP execute request: {e}"))?;
                let execute: dcp::ExecuteRequest = match serde_json::from_slice(&body) {
                    Ok(value) => value,
                    Err(error) => {
                        respond_json(
                            request,
                            400,
                            &serde_json::json!({"code":"invalid_request","message":error.to_string(),"retryable":false}),
                        )?;
                        continue;
                    }
                };
                let status = current_status(socket_path)?;
                let projection = radioman::dcp::Projection {
                    devices: &devices,
                    status: &status,
                    spectrum_destination,
                };
                if let Some((fingerprint, receipt)) = receipts.get(&execute.request_id) {
                    if fingerprint == &body {
                        let code = if matches!(receipt.outcome, dcp::Outcome::Rejected) {
                            409
                        } else {
                            200
                        };
                        respond_json(request, code, receipt)?;
                    } else {
                        let receipt = radioman::dcp::rejected(
                            projection,
                            &execute,
                            now_ms(),
                            dcp::Problem {
                                code: dcp::ErrorCode::Conflict,
                                message: "request_id was reused with different content".into(),
                                retryable: false,
                                details: None,
                            },
                        );
                        respond_json(request, 409, &receipt)?;
                    }
                    continue;
                }
                if receipts.len() >= 1024 {
                    let receipt = radioman::dcp::rejected(
                        projection,
                        &execute,
                        now_ms(),
                        dcp::Problem {
                            code: dcp::ErrorCode::InternalError,
                            message: "DCP receipt ledger is full".into(),
                            retryable: true,
                            details: None,
                        },
                    );
                    respond_json(request, 503, &receipt)?;
                    continue;
                }
                let receipt = radioman::dcp::execute(projection, &execute, now_ms(), |command| {
                    let body = serde_json::to_vec(&command)
                        .map_err(|e| format!("encode service command: {e}"))?;
                    control_status(socket_path, &body)
                });
                let code = if matches!(receipt.outcome, dcp::Outcome::Rejected) {
                    409
                } else {
                    200
                };
                receipts.insert(execute.request_id.clone(), (body, receipt.clone()));
                respond_json(request, code, &receipt)?;
            }
            _ => respond_json(
                request,
                404,
                &serde_json::json!({"code":"action_not_found","message":"DCP endpoint not found","retryable":false}),
            )?,
        }
    }
    Ok(())
}

fn current_status(socket_path: &str) -> Result<ServiceStatus, String> {
    control_status(
        socket_path,
        &serde_json::to_vec(&ServiceCommand::Status)
            .map_err(|e| format!("encode status command: {e}"))?,
    )
}

fn respond_json<T: serde::Serialize>(
    request: Request,
    status: u16,
    value: &T,
) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|e| format!("encode DCP response: {e}"))?;
    let response = Response::from_data(body)
        .with_status_code(StatusCode(status))
        .with_header(
            Header::from_bytes("content-type", "application/json")
                .map_err(|_| "invalid content-type header")?,
        )
        .with_header(
            Header::from_bytes("dcp-version", dcp::VERSION)
                .map_err(|_| "invalid DCP version header")?,
        )
        .with_header(
            Header::from_bytes("cache-control", "no-store").map_err(|_| "invalid cache header")?,
        );
    request
        .respond(response)
        .map_err(|e| format!("send DCP response: {e}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema_version: u32,
    node_id: String,
    #[serde(default)]
    radios: Vec<RadioDescriptor>,
    #[serde(default)]
    packet_radios: Vec<PacketRadioDescriptor>,
    iq_bind: String,
    iq_advertise: String,
}

impl Config {
    fn radio(&self, id: &str) -> Result<&RadioDescriptor, String> {
        self.radios
            .iter()
            .find(|radio| radio.id == id)
            .ok_or_else(|| format!("unknown radio id {id}"))
    }

    fn rtl_radio(&self) -> Result<&RadioDescriptor, String> {
        self.radios
            .iter()
            .find(|radio| matches!(radio.kind, RadioKind::RtlSdr))
            .ok_or("no RTL-SDR is configured".into())
    }

    fn device_index(&self) -> DeviceIndex {
        DeviceIndex {
            schema_version: radioman::CONTRACT_VERSION,
            node_id: self.node_id.clone(),
            radios: self.radios.clone(),
            packet_radios: self.packet_radios.clone(),
        }
    }
}

fn spectrum_bins(iq: &[u8], planner: &mut FftPlanner<f32>) -> Result<Vec<f32>, String> {
    const FFT: usize = 1024;
    const BINS: usize = 128;
    if iq.len() < FFT * 2 {
        return Err("RTL-SDR produced a partial FFT window".into());
    }
    let mut samples = (0..FFT)
        .map(|index| {
            let phase = 2.0 * std::f32::consts::PI * index as f32 / (FFT - 1) as f32;
            let window = 0.5 - 0.5 * phase.cos();
            Complex32::new(
                (f32::from(iq[index * 2]) - 127.5) / 127.5 * window,
                (f32::from(iq[index * 2 + 1]) - 127.5) / 127.5 * window,
            )
        })
        .collect::<Vec<_>>();
    planner.plan_fft_forward(FFT).process(&mut samples);
    let scale = (FFT as f32 * 0.5).powi(2);
    Ok((0..BINS)
        .map(|output| {
            let power = (0..FFT / BINS)
                .map(|offset| {
                    let shifted = output * (FFT / BINS) + offset;
                    let index = (shifted + FFT / 2) % FFT;
                    samples[index].norm_sqr() / scale
                })
                .sum::<f32>()
                / (FFT / BINS) as f32;
            10.0 * power.max(1.0e-12).log10()
        })
        .collect())
}

fn receive_spectrum(
    config: &Config,
    radio_id: Option<&str>,
    center_frequency_hz: u64,
    destination: SocketAddr,
    gain_db: f32,
    tuning: Option<radioman::ActiveTuning>,
) -> Result<(), String> {
    const SAMPLE_RATE: u32 = 2_048_000;
    let radio = match radio_id {
        Some(id) => config.radio(id)?,
        None => config.rtl_radio()?,
    };
    if !matches!(radio.kind, RadioKind::RtlSdr) {
        return Err("spectrum backend currently supports RTL-SDR only".into());
    }
    if !radio.frequency.contains(center_frequency_hz) {
        return Err("spectrum center frequency is outside the configured radio range".into());
    }
    if !gain_db.is_finite() || !(0.0..=49.6).contains(&gain_db) {
        return Err("RTL-SDR gain must be between 0 and 49.6 dB".into());
    }
    let serial = radio.serial.as_deref().unwrap_or("0");
    let mut child = Command::new("rtl_sdr")
        .args([
            "-d",
            serial,
            "-f",
            &center_frequency_hz.to_string(),
            "-s",
            &SAMPLE_RATE.to_string(),
            "-g",
            &gain_db.to_string(),
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("start rtl_sdr: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("rtl_sdr stdout is unavailable")?;
    let socket =
        UdpSocket::bind("0.0.0.0:0").map_err(|error| format!("bind spectrum socket: {error}"))?;
    socket
        .connect(destination)
        .map_err(|error| format!("connect spectrum socket: {error}"))?;
    let mut planner = FftPlanner::new();
    let mut iq = [0_u8; 2048];
    let mut sequence = 0_u64;
    let mut next_frame = Instant::now();
    let mut next_descriptor = Instant::now();
    eprintln!(
        "radioman: spectrum center={center_frequency_hz}Hz span={SAMPLE_RATE}Hz gain={gain_db:.1}dB destination={destination}"
    );
    loop {
        stdout
            .read_exact(&mut iq)
            .map_err(|error| format!("read rtl_sdr IQ: {error}"))?;
        if Instant::now() < next_frame {
            continue;
        }
        if Instant::now() >= next_descriptor {
            let observed_at_unix_ms = now_ms();
            let descriptor = StreamDescriptorV1 {
                schema_version: SCHEMA_VERSION,
                stream_id: "radio/spectrum/live".into(),
                semantic_type: "rf.spectrum".into(),
                source_resource: "radio/rtl-sdr".into(),
                content: BTreeMap::from([
                    ("role".into(), serde_json::json!("radio-analysis")),
                    (
                        "description".into(),
                        serde_json::json!(
                            "FFT power observations from the active software-defined-radio receiver, suitable for spectrum and waterfall views."
                        ),
                    ),
                ]),
                media: MediaCharacteristics {
                    sample_format: Some("power-db".into()),
                    sample_rate: Some(SAMPLE_RATE as u32),
                    ..Default::default()
                },
                freshness: Freshness::Live,
                provenance: "radioman".into(),
            };
            descriptor.validate()?;
            socket
                .send(
                    &serde_json::to_vec(&descriptor)
                        .map_err(|error| format!("encode stream descriptor: {error}"))?,
                )
                .map_err(|error| format!("send stream descriptor: {error}"))?;
            let endpoints = EndpointAnnouncementV1 {
                schema_version: SCHEMA_VERSION,
                stream_id: descriptor.stream_id.clone(),
                endpoints: vec![TransportEndpoint {
                    transport: "udp".into(),
                    uri_reference: "radioman/spectrum".into(),
                }],
                observed_at_unix_ms,
                valid_until_unix_ms: observed_at_unix_ms + 90_000,
                provenance: "radioman".into(),
            };
            endpoints.validate(observed_at_unix_ms)?;
            socket
                .send(
                    &serde_json::to_vec(&endpoints)
                        .map_err(|error| format!("encode endpoint announcement: {error}"))?,
                )
                .map_err(|error| format!("send endpoint announcement: {error}"))?;
            next_descriptor = Instant::now() + Duration::from_secs(30);
        }
        next_frame = Instant::now() + Duration::from_millis(50);
        sequence = sequence.saturating_add(1);
        let frame = SpectrumFrame {
            schema_version: 1,
            session_id: tuning.as_ref().map_or_else(
                || "manual-spectrum".into(),
                |value| value.session_id.clone(),
            ),
            generation: 1,
            sequence,
            observed_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            center_frequency_hz,
            span_hz: SAMPLE_RATE,
            floor_dbfs: -100.0,
            ceiling_dbfs: -10.0,
            bins_dbfs: spectrum_bins(&iq, &mut planner)?,
            tuning: tuning.clone(),
        };
        frame.validate()?;
        let encoded =
            serde_json::to_vec(&frame).map_err(|error| format!("encode spectrum: {error}"))?;
        socket
            .send(&encoded)
            .map_err(|error| format!("send spectrum: {error}"))?;
    }
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != radioman::CONFIG_VERSION {
            return Err("unsupported Radioman config schema".into());
        }
        if self.node_id.trim().is_empty() || self.node_id.len() > radioman::MAX_ID_BYTES {
            return Err("invalid node id".into());
        }
        self.device_index().validate()?;
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
    if input.len() % 2 != 0 {
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
    let radio = config.rtl_radio()?;
    if !radio.frequency.contains(center_frequency_hz) {
        return Err("FM center frequency is outside the configured radio range".into());
    }
    if !gain_db.is_finite() || !(0.0..=49.6).contains(&gain_db) {
        return Err("RTL-SDR gain must be between 0 and 49.6 dB".into());
    }
    let serial = radio.serial.as_deref().unwrap_or("0");
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
    let socket =
        UdpSocket::bind("0.0.0.0:0").map_err(|error| format!("bind PCM socket: {error}"))?;
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
            println!(
                "valid node={} radios={} packet_radios={}",
                config.node_id,
                config.radios.len(),
                config.packet_radios.len()
            );
            Ok(())
        }
        [_, command, path] if command == "device-index" => {
            let config = read_config(path)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&config.device_index())
                    .map_err(|error| format!("encode device index: {error}"))?
            );
            Ok(())
        }
        [_, command, path, socket] if command == "serve" => serve(path, socket),
        [_, command, socket, json] if command == "control" => control(socket, json),
        [
            _,
            command,
            path,
            socket,
            listen,
            spectrum_destination,
            token_file,
        ] if command == "dcp-serve" => {
            let config = read_config(path)?;
            dcp_serve(&config, socket, listen, spectrum_destination, token_file)
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
        [_, command, path, frequency, destination]
        | [_, command, path, frequency, destination, _]
        | [_, command, path, frequency, destination, _, _]
        | [_, command, path, frequency, destination, _, _, _]
            if command == "spectrum" =>
        {
            let config = read_config(path)?;
            let frequency = frequency
                .parse::<u64>()
                .map_err(|error| format!("invalid center frequency: {error}"))?;
            let destination = destination
                .parse::<SocketAddr>()
                .map_err(|error| format!("invalid spectrum destination: {error}"))?;
            let gain = args
                .get(5)
                .map(|value| value.parse::<f32>())
                .transpose()
                .map_err(|error| format!("invalid gain: {error}"))?
                .unwrap_or(19.7);
            let tuning = args
                .get(6)
                .map(|value| serde_json::from_str(value))
                .transpose()
                .map_err(|error| format!("invalid active tuning: {error}"))?;
            let radio_id = args.get(7).map(String::as_str);
            receive_spectrum(&config, radio_id, frequency, destination, gain, tuning)
        }
        _ => Err(format!(
            "usage:\n  {} validate-config CONFIG.toml\n  {} device-index CONFIG.toml\n  {} serve CONFIG.toml CONTROL_SOCKET\n  {} control CONTROL_SOCKET JSON\n  {} dcp-serve CONFIG.toml CONTROL_SOCKET LISTEN SPECTRUM_DEST TOKEN_FILE\n  {} fm CONFIG.toml CENTER_HZ PCM_DEST [GAIN_DB]\n  {} spectrum CONFIG.toml CENTER_HZ UDP_DEST [GAIN_DB] [TUNING_JSON] [RADIO_ID]",
            args[0], args[0], args[0], args[0], args[0], args[0], args[0]
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
    use super::{mono_s16le_to_stereo_f32le, spectrum_bins};
    use rustfft::FftPlanner;

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

    #[test]
    fn spectrum_is_bounded_and_finite() {
        let iq = (0..2048)
            .map(|index| if index % 2 == 0 { 220 } else { 127 })
            .collect::<Vec<_>>();
        let bins = spectrum_bins(&iq, &mut FftPlanner::new()).unwrap();
        assert_eq!(bins.len(), 128);
        assert!(bins.iter().all(|bin| bin.is_finite() && *bin <= 40.0));
    }
}
