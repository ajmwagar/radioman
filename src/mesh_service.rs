//! Receive-only MQTT ingestion and a local HTTP projection.
use crate::mesh::Store;
use serde::Deserialize;
use std::{
    fs,
    net::SocketAddr,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Response, Server};
use unibus_mqtt::subscriber::{ReportEvent, ReportSubscriber, parse_pin};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    mqtt_endpoint: SocketAddr,
    client_id: String,
    username: String,
    password_file: String,
    certificate_sha256: String,
    subscriptions: Vec<Subscription>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    topic: String,
    format: String,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[derive(Default)]
struct State {
    store: Store,
    sources: Vec<serde_json::Value>,
}
pub fn serve(path: &str) -> Result<(), String> {
    let config: Config = toml::from_str(&fs::read_to_string(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if !config.listen.ip().is_loopback() {
        return Err(
            "mesh HTTP must bind loopback; expose through Unibus or an authenticated forward"
                .into(),
        );
    }
    if config.subscriptions.is_empty() || config.subscriptions.len() > 8 {
        return Err("configure 1..8 exact subscriptions".into());
    }
    let pin = parse_pin(&config.certificate_sha256).map_err(|e| e.to_string())?;
    let password = fs::read_to_string(&config.password_file)
        .map_err(|e| format!("read MQTT password: {e}"))?
        .trim()
        .to_owned();
    let state = Arc::new(Mutex::new(State::default()));
    // Validate all subscriptions before opening listeners or starting workers.
    let mut clients = Vec::new();
    for (index, sub) in config.subscriptions.into_iter().enumerate() {
        if !matches!(sub.format.as_str(), "meshtastic-json" | "radioman-node-v1") {
            return Err("unsupported mesh subscription format".into());
        }
        let client = ReportSubscriber::connect(
            config.mqtt_endpoint,
            &format!("{}-{index}", config.client_id),
            &sub.topic,
            &config.username,
            &password,
            pin,
        )
        .map_err(|e| e.to_string())?;
        state
            .lock()
            .map_err(|e| e.to_string())?
            .sources
            .push(serde_json::json!({"topic":sub.topic,"connected":false,"error":null}));
        clients.push((index, sub, client));
    }
    let server = Server::http(config.listen).map_err(|e| e.to_string())?;
    for (index, sub, mut client) in clients {
        let state = state.clone();
        thread::spawn(move || {
            loop {
                match client.poll(Duration::from_secs(1)) {
                    Ok(Some(ReportEvent::Subscribed)) => {
                        let mut s = state.lock().expect("mesh state lock");
                        s.sources[index]["connected"] = true.into();
                        s.sources[index]["error"] = serde_json::Value::Null;
                    }
                    Ok(Some(ReportEvent::Report {
                        topic,
                        payload,
                        retained,
                        ..
                    })) => {
                        let result = if topic == sub.topic {
                            state.lock().expect("mesh state lock").store.ingest(
                                &sub.format,
                                &payload,
                                now(),
                                retained,
                            )
                        } else {
                            Err("unexpected MQTT topic".into())
                        };
                        if let Err(error) = result {
                            eprintln!("radioman mesh rejected observation topic={topic}: {error}");
                            state.lock().expect("mesh state lock").sources[index]["error"] =
                                error.into();
                        } else {
                            state.lock().expect("mesh state lock").sources[index]["error"] =
                                serde_json::Value::Null;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("radioman mesh MQTT source={} failed: {e}", sub.topic);
                        let mut s = state.lock().expect("mesh state lock");
                        s.sources[index]["connected"] = false.into();
                        s.sources[index]["error"] = e.to_string().into();
                        drop(s);
                        thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        });
    }
    eprintln!(
        "radioman mesh snapshot http://{}/api/mesh/snapshot",
        config.listen
    );
    for request in server.incoming_requests() {
        let (code, body) = if request.method() != &Method::Get {
            (405, serde_json::json!({"error":"read-only endpoint"}))
        } else {
            let s = state.lock().map_err(|e| e.to_string())?;
            match request.url() {
                "/api/mesh/snapshot" => (
                    200,
                    serde_json::to_value(s.store.snapshot(now())).map_err(|e| e.to_string())?,
                ),
                "/api/mesh/health" => (200, serde_json::json!({"version":1,"sources":s.sources})),
                _ => (404, serde_json::json!({"error":"not found"})),
            }
        };
        let response = Response::from_data(serde_json::to_vec(&body).map_err(|e| e.to_string())?)
            .with_status_code(code)
            .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
            .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
        request.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn refuses_non_loopback_before_opening_any_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mesh.toml");
        std::fs::write(
            &path,
            r#"
listen = "0.0.0.0:18683"
mqtt_endpoint = "127.0.0.1:8883"
client_id = "radioman-mesh"
username = "readonly"
password_file = "/must-not-be-read"
certificate_sha256 = "not-enrolled"
subscriptions = []
"#,
        )
        .unwrap();
        let error = super::serve(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("loopback"));
    }
}
