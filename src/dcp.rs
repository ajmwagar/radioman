//! Provider-owned Decision Catalog Protocol projection for Radioman.
//!
//! The catalog is derived from the validated device index and current tuner
//! status. DCP transports bounded intent; spectrum and IQ remain on their
//! announced media endpoints.

use std::collections::BTreeMap;

use dcp::{
    Action, Authorization, Availability, Catalog, Discovery, Endpoints, ErrorCode, ExecuteRequest,
    Outcome, Phase, Problem, Provider, Receipt, Safety, TreeNode,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    DeviceIndex, ExperimentOutput, ExperimentRequest, RadioKind, RxRequest, SampleFormat,
    ServiceCommand, ServiceStatus,
};

pub const PROVIDER_ID: &str = "radioman";

pub fn rejected(
    projection: Projection<'_>,
    request: &ExecuteRequest,
    observed_at_ms: u64,
    error: Problem,
) -> Receipt {
    let current = projection.catalog();
    let (catalog_revision, state_revision) = current
        .as_ref()
        .map(|catalog| {
            (
                catalog.catalog_revision.clone(),
                catalog.state_revision.clone(),
            )
        })
        .unwrap_or_else(|_| ("unavailable".into(), "unavailable".into()));
    Receipt {
        dcp_version: dcp::VERSION.into(),
        receipt_id: receipt_id(&request.request_id),
        request_id: request.request_id.clone(),
        provider_id: PROVIDER_ID.into(),
        action_id: request.action_id.clone(),
        phase: request.phase.clone(),
        outcome: Outcome::Rejected,
        expected_catalog_revision: request.expected_catalog_revision.clone(),
        expected_state_revision: request.expected_state_revision.clone(),
        observed_catalog_revision: catalog_revision,
        observed_state_revision: state_revision,
        resulting_catalog_revision: None,
        resulting_state_revision: None,
        observed_at: timestamp(observed_at_ms),
        error: Some(error),
    }
}

pub fn execute<F>(
    projection: Projection<'_>,
    request: &ExecuteRequest,
    observed_at_ms: u64,
    apply: F,
) -> Receipt
where
    F: FnOnce(ServiceCommand) -> Result<ServiceStatus, String>,
{
    let current = projection.catalog();
    let (catalog_revision, state_revision) = current
        .as_ref()
        .map(|catalog| {
            (
                catalog.catalog_revision.clone(),
                catalog.state_revision.clone(),
            )
        })
        .unwrap_or_else(|_| ("unavailable".into(), "unavailable".into()));
    let base = |outcome, error, resulting: Option<&Catalog>| Receipt {
        dcp_version: dcp::VERSION.into(),
        receipt_id: receipt_id(&request.request_id),
        request_id: request.request_id.clone(),
        provider_id: PROVIDER_ID.into(),
        action_id: request.action_id.clone(),
        phase: request.phase.clone(),
        outcome,
        expected_catalog_revision: request.expected_catalog_revision.clone(),
        expected_state_revision: request.expected_state_revision.clone(),
        observed_catalog_revision: catalog_revision.clone(),
        observed_state_revision: state_revision.clone(),
        resulting_catalog_revision: resulting.map(|catalog| catalog.catalog_revision.clone()),
        resulting_state_revision: resulting.map(|catalog| catalog.state_revision.clone()),
        observed_at: timestamp(observed_at_ms),
        error,
    };
    let command = match projection.command(request) {
        Ok(command) => command,
        Err(error) => return base(Outcome::Rejected, Some(error), None),
    };
    let resulting_status = match apply(command) {
        Ok(status) if status.error.is_none() => status,
        Ok(status) => {
            return base(
                Outcome::Rejected,
                Some(problem(
                    ErrorCode::InternalError,
                    status.error.as_deref().unwrap_or("Radioman command failed"),
                    true,
                )),
                None,
            );
        }
        Err(error) => {
            return base(
                Outcome::Rejected,
                Some(problem(ErrorCode::InternalError, &error, true)),
                None,
            );
        }
    };
    let resulting_projection = Projection {
        devices: projection.devices,
        status: &resulting_status,
        spectrum_destination: projection.spectrum_destination,
    };
    match resulting_projection.catalog() {
        Ok(resulting) => base(Outcome::Applied, None, Some(&resulting)),
        Err(error) => base(
            Outcome::Rejected,
            Some(problem(ErrorCode::InternalError, &error, true)),
            None,
        ),
    }
}

#[derive(Debug, Clone)]
pub struct Projection<'a> {
    pub devices: &'a DeviceIndex,
    pub status: &'a ServiceStatus,
    pub spectrum_destination: &'a str,
}

impl Projection<'_> {
    pub fn discovery(&self) -> Discovery {
        Discovery {
            dcp: dcp::SPEC.into(),
            provider: self.provider(),
            versions: vec![dcp::VERSION.into()],
            endpoints: Endpoints {
                catalog: "/v1/decisions".into(),
                execute: Some("/v1/decisions/execute".into()),
                stream: None,
            },
            authorization: vec![Authorization {
                scheme: "bearer".into(),
                issuer: None,
                scopes: vec!["radio:receive".into()],
            }],
        }
    }

    pub fn catalog(&self) -> Result<Catalog, String> {
        self.devices.validate()?;
        if let Some(error) = self.status.error.as_deref() {
            return Err(format!("radioman service is unhealthy: {error}"));
        }
        let actions = if self.status.active.is_some() {
            vec![self.retune_action(), self.stop_action()]
        } else {
            vec![self.start_action()?]
        };
        let state = self.state();
        Ok(Catalog {
            dcp_version: dcp::VERSION.into(),
            provider: self.provider(),
            catalog_revision: digest(&serde_json::to_value(&actions).map_err(|e| e.to_string())?)?,
            state_revision: digest(&Value::Object(state.clone().into_iter().collect()))?,
            observed_at: timestamp(self.status.observed_at_ms),
            expires_at: None,
            state: Some(state),
            actions,
            tree: vec![TreeNode {
                id: "radio".into(),
                label: "Radio".into(),
                action_ids: if self.status.active.is_some() {
                    vec![
                        "radioman.tuner.retune".into(),
                        "radioman.session.stop".into(),
                    ]
                } else {
                    vec!["radioman.spectrum.start".into()]
                },
                children: vec![],
            }],
        })
    }

    pub fn command(&self, request: &ExecuteRequest) -> Result<ServiceCommand, Problem> {
        let catalog = self.catalog().map_err(internal)?;
        if request.dcp_version != dcp::VERSION {
            return Err(problem(
                ErrorCode::UnsupportedVersion,
                "unsupported DCP version",
                false,
            ));
        }
        if request.phase != Phase::Commit {
            return Err(problem(
                ErrorCode::UnsafePhase,
                "Radioman actions are commit-only",
                false,
            ));
        }
        if !request.evidence.final_ {
            return Err(problem(
                ErrorCode::FinalityRequired,
                "Radioman requires final transcript evidence",
                false,
            ));
        }
        if request.expected_catalog_revision != catalog.catalog_revision {
            return Err(problem(
                ErrorCode::StaleCatalog,
                "Radioman catalog changed",
                true,
            ));
        }
        if request.expected_state_revision != catalog.state_revision {
            return Err(problem(
                ErrorCode::StaleState,
                "Radioman tuner state changed",
                true,
            ));
        }
        if !catalog
            .actions
            .iter()
            .any(|action| action.id == request.action_id)
        {
            return Err(problem(
                ErrorCode::ActionNotFound,
                "action is not currently advertised",
                false,
            ));
        }
        match request.action_id.as_str() {
            "radioman.spectrum.start" => self.start_command(&request.arguments),
            "radioman.tuner.retune" => self.retune_command(&request.arguments),
            "radioman.session.stop" => self.stop_command(&request.arguments),
            _ => Err(problem(
                ErrorCode::ActionNotFound,
                "unknown Radioman action",
                false,
            )),
        }
    }

    fn provider(&self) -> Provider {
        Provider {
            id: PROVIDER_ID.into(),
            name: "Radioman".into(),
            instance: format!("urn:fpl:node:{}", self.devices.node_id),
        }
    }

    fn state(&self) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("node_id".into(), json!(self.devices.node_id)),
            (
                "radios".into(),
                serde_json::to_value(&self.devices.radios).unwrap_or(Value::Null),
            ),
            (
                "packet_radios".into(),
                serde_json::to_value(&self.devices.packet_radios).unwrap_or(Value::Null),
            ),
            (
                "active".into(),
                serde_json::to_value(&self.status.active).unwrap_or(Value::Null),
            ),
            (
                "queued_session_ids".into(),
                json!(self.status.queued_session_ids),
            ),
            (
                "spectrum_stream".into(),
                json!({
                    "stream_id": "radio/spectrum/live",
                    "semantic_type": "rf.spectrum",
                    "transport": "udp",
                    "destination": self.spectrum_destination,
                }),
            ),
        ])
    }

    fn start_action(&self) -> Result<Action, String> {
        let radios = self
            .devices
            .radios
            .iter()
            .filter(|radio| {
                radio.receive
                    && matches!(radio.kind, RadioKind::RtlSdr)
                    && radio.sample_rates_hz.contains(&2_048_000)
            })
            .collect::<Vec<_>>();
        let available = !radios.is_empty();
        let radio_ids = radios
            .iter()
            .map(|radio| radio.id.as_str())
            .collect::<Vec<_>>();
        let minimum = radios
            .iter()
            .map(|radio| radio.frequency.minimum_hz)
            .min()
            .unwrap_or(1);
        let maximum = radios
            .iter()
            .map(|radio| radio.frequency.maximum_hz)
            .max()
            .unwrap_or(1);
        Ok(action(
            "radioman.spectrum.start",
            "Start spectrum receiver",
            "Claim one available receive-only tuner and publish its bounded live spectrum stream.",
            json!({
                "$schema": dcp::SCHEMA_2020_12,
                "type": "object",
                "additionalProperties": false,
                "required": ["session_id", "radio_id", "center_frequency_hz", "duration_ms"],
                "properties": {
                    "session_id": {"type":"string", "pattern":"^[A-Za-z0-9._/-]{1,96}$"},
                    "radio_id": {"type":"string", "enum":radio_ids},
                    "center_frequency_hz": {"type":"integer", "minimum":minimum, "maximum":maximum},
                    "duration_ms": {"type":"integer", "minimum":1000, "maximum":86400000},
                    "gain_db": {"type":"number", "minimum":0, "maximum":49.6}
                }
            }),
            available,
            Some("no_receive_tuner"),
        ))
    }

    fn retune_action(&self) -> Action {
        let active = self.status.active.as_ref().expect("active checked");
        let radio = self
            .devices
            .radios
            .iter()
            .find(|radio| radio.id == active.radio_id)
            .or_else(|| self.devices.radios.first());
        let (minimum, maximum) = radio
            .map(|radio| (radio.frequency.minimum_hz, radio.frequency.maximum_hz))
            .unwrap_or((1, u64::MAX));
        action(
            "radioman.tuner.retune",
            "Retune active receiver",
            "Retune the currently owned receive session without changing its route or end time.",
            json!({
                "$schema": dcp::SCHEMA_2020_12, "type":"object", "additionalProperties":false,
                "required":["center_frequency_hz"],
                "properties":{
                    "center_frequency_hz":{"type":"integer","minimum":minimum,"maximum":maximum},
                    "gain_db":{"type":"number","minimum":0,"maximum":49.6}
                }
            }),
            true,
            None,
        )
    }

    fn stop_action(&self) -> Action {
        action(
            "radioman.session.stop",
            "Stop active receiver",
            "Stop the active receive session and release its tuner.",
            json!({"$schema":dcp::SCHEMA_2020_12,"type":"object","additionalProperties":false,"properties":{}}),
            true,
            None,
        )
    }

    fn start_command(
        &self,
        arguments: &BTreeMap<String, Value>,
    ) -> Result<ServiceCommand, Problem> {
        let session_id = string_arg(arguments, "session_id")?;
        let radio_id = string_arg(arguments, "radio_id")?;
        let center_frequency_hz = u64_arg(arguments, "center_frequency_hz")?;
        let duration_ms = u64_arg(arguments, "duration_ms")?;
        let gain_db = optional_f32_arg(arguments, "gain_db")?;
        let radio = self
            .devices
            .radios
            .iter()
            .find(|radio| radio.id == radio_id)
            .ok_or_else(|| {
                problem(
                    ErrorCode::InvalidRequest,
                    "radio_id is not available",
                    false,
                )
            })?;
        let experiment = ExperimentRequest {
            owner: "dcp".into(),
            start_at_ms: 0,
            rx: RxRequest {
                session_id,
                radio_id,
                center_frequency_hz,
                sample_rate_hz: 2_048_000,
                sample_format: SampleFormat::Cu8,
                gain_db,
                ppm: 0,
                duration_ms: Some(duration_ms),
            },
            output: ExperimentOutput::Spectrum {
                destination: self.spectrum_destination.into(),
            },
        };
        experiment.validate_for(radio).map_err(invalid)?;
        Ok(ServiceCommand::Submit { experiment })
    }

    fn retune_command(
        &self,
        arguments: &BTreeMap<String, Value>,
    ) -> Result<ServiceCommand, Problem> {
        let active = self
            .status
            .active
            .as_ref()
            .ok_or_else(|| problem(ErrorCode::ActionUnavailable, "no active receiver", true))?;
        Ok(ServiceCommand::Retune {
            session_id: active.session_id.clone(),
            center_frequency_hz: u64_arg(arguments, "center_frequency_hz")?,
            gain_db: optional_f32_arg(arguments, "gain_db")?,
        })
    }

    fn stop_command(&self, arguments: &BTreeMap<String, Value>) -> Result<ServiceCommand, Problem> {
        if !arguments.is_empty() {
            return Err(problem(
                ErrorCode::InvalidRequest,
                "stop accepts no arguments",
                false,
            ));
        }
        let active = self
            .status
            .active
            .as_ref()
            .ok_or_else(|| problem(ErrorCode::ActionUnavailable, "no active receiver", true))?;
        Ok(ServiceCommand::Cancel {
            session_id: active.session_id.clone(),
        })
    }
}

fn action(
    id: &str,
    title: &str,
    description: &str,
    input_schema: Value,
    available: bool,
    reason: Option<&str>,
) -> Action {
    Action {
        id: id.into(),
        domain: "radio".into(),
        title: title.into(),
        description: description.into(),
        input_schema,
        phases: vec![Phase::Commit],
        safety: Safety {
            idempotent: true,
            reversible: false,
            requires_final: true,
            confirmation_required: false,
        },
        availability: Availability {
            available,
            reason_code: reason.map(str::to_owned),
            reason: None,
        },
    }
}

fn string_arg(arguments: &BTreeMap<String, Value>, name: &str) -> Result<String, Problem> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            problem(
                ErrorCode::InvalidRequest,
                &format!("missing or invalid {name}"),
                false,
            )
        })
}

fn u64_arg(arguments: &BTreeMap<String, Value>, name: &str) -> Result<u64, Problem> {
    arguments.get(name).and_then(Value::as_u64).ok_or_else(|| {
        problem(
            ErrorCode::InvalidRequest,
            &format!("missing or invalid {name}"),
            false,
        )
    })
}

fn optional_f32_arg(
    arguments: &BTreeMap<String, Value>,
    name: &str,
) -> Result<Option<f32>, Problem> {
    arguments
        .get(name)
        .map(|value| {
            value.as_f64().map(|value| value as f32).ok_or_else(|| {
                problem(ErrorCode::InvalidRequest, &format!("invalid {name}"), false)
            })
        })
        .transpose()
}

fn problem(code: ErrorCode, message: &str, retryable: bool) -> Problem {
    Problem {
        code,
        message: message.into(),
        retryable,
        details: None,
    }
}

fn invalid(message: String) -> Problem {
    problem(ErrorCode::InvalidRequest, &message, false)
}
fn internal(message: String) -> Problem {
    problem(ErrorCode::InternalError, &message, true)
}

fn digest(value: &Value) -> Result<String, String> {
    fn write(value: &Value, out: &mut Vec<u8>) -> Result<(), String> {
        match value {
            Value::Object(map) => {
                out.push(b'{');
                let mut entries = map.iter().collect::<Vec<_>>();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    out.extend(serde_json::to_vec(key).map_err(|e| e.to_string())?);
                    out.push(b':');
                    write(value, out)?;
                }
                out.push(b'}');
            }
            Value::Array(values) => {
                out.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    write(value, out)?;
                }
                out.push(b']');
            }
            _ => out.extend(serde_json::to_vec(value).map_err(|e| e.to_string())?),
        }
        Ok(())
    }
    let mut bytes = Vec::new();
    write(value, &mut bytes)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn receipt_id(request_id: &str) -> String {
    let hash = format!("{:x}", Sha256::digest(request_id.as_bytes()));
    format!("radioman-{}", &hash[..24])
}

fn timestamp(unix_ms: u64) -> String {
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ms) * 1_000_000)
        .ok()
        .and_then(|value| value.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}
