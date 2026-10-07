//! Receive-only radio mesh observations. No inferred positions or topology.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_NODES: usize = 128;
pub const MAX_LINKS: usize = 256;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Meshtastic,
    Meshcore,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Position {
    pub lat: f64,
    pub lon: f64,
    pub updated_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precision_m: Option<f64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub protocol: Protocol,
    pub id: String,
    pub label: String,
    pub last_heard: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi_dbm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub protocol: Protocol,
    pub from: String,
    pub to: String,
    pub last_heard: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub observed_at: u64,
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
}
#[derive(Default, Clone)]
pub struct Store {
    nodes: BTreeMap<(Protocol, String), Node>,
    links: BTreeMap<(Protocol, String, String), Link>,
}
fn text(v: &str, limit: usize) -> bool {
    !v.is_empty() && v.len() <= limit && !v.chars().any(char::is_control)
}
fn timestamp(t: u64, now: u64) -> Result<u64, String> {
    if t == 0 || t > now.saturating_add(30) {
        Err("invalid observation timestamp".into())
    } else {
        Ok(t)
    }
}
fn number(v: &Value, key: &str, min: f64, max: f64) -> Option<f64> {
    v[key]
        .as_f64()
        .filter(|n| n.is_finite() && (min..=max).contains(n))
}
fn id(v: &Value) -> Result<String, String> {
    let n = v
        .as_u64()
        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
        .ok_or("invalid Meshtastic node number")?;
    Ok(format!("!{n:08x}"))
}
impl Node {
    pub fn validate(&self, now: u64) -> Result<(), String> {
        if !text(&self.id, 64)
            || !text(&self.label, 32)
            || self.role.as_ref().is_some_and(|r| !text(r, 32))
        {
            return Err("invalid mesh identity".into());
        }
        timestamp(self.last_heard, now)?;
        if let Some(p) = &self.position {
            timestamp(p.updated_at, now)?;
            if p.updated_at > self.last_heard
                || !p.lat.is_finite()
                || !(-90.0..=90.0).contains(&p.lat)
                || !p.lon.is_finite()
                || !(-180.0..=180.0).contains(&p.lon)
                || p.precision_m
                    .is_some_and(|v| !v.is_finite() || v < 0.0 || v > 1_000_000.0)
            {
                return Err("invalid mesh position".into());
            }
        }
        for (v, min, max) in [
            (self.battery_percent, 0.0, 100.0),
            (self.rssi_dbm, -200.0, 0.0),
            (self.snr_db, -100.0, 100.0),
        ] {
            if v.is_some_and(|v| !v.is_finite() || !(min..=max).contains(&v)) {
                return Err("invalid mesh telemetry".into());
            }
        }
        Ok(())
    }
}
impl Store {
    pub fn snapshot(&self, now: u64) -> Snapshot {
        let mut snapshot = Snapshot {
            version: 1,
            observed_at: now,
            nodes: self
                .nodes
                .values()
                .filter(|n| now.saturating_sub(n.last_heard) <= 86400)
                .cloned()
                .collect(),
            links: self
                .links
                .values()
                .filter(|l| now.saturating_sub(l.last_heard) <= 86400)
                .cloned()
                .collect(),
        };
        // Escaped UTF8 strings can make a record substantially larger than its byte fields.
        while serde_json::to_vec(&snapshot)
            .expect("finite validated mesh snapshot")
            .len()
            > 128 * 1024
        {
            if let Some((index, _)) = snapshot
                .links
                .iter()
                .enumerate()
                .min_by_key(|(_, l)| l.last_heard)
            {
                snapshot.links.remove(index);
            } else if let Some((index, _)) = snapshot
                .nodes
                .iter()
                .enumerate()
                .min_by_key(|(_, n)| n.last_heard)
            {
                snapshot.nodes.remove(index);
            } else {
                break;
            }
        }
        snapshot
    }
    pub fn ingest(
        &mut self,
        format: &str,
        bytes: &[u8],
        now: u64,
        retained: bool,
    ) -> Result<(), String> {
        if bytes.len() > 65536 {
            return Err("mesh observation exceeds 64 KiB".into());
        }
        // Validate and apply transactionally, preventing malformed neighbor records from partly updating state.
        let mut next = self.clone();
        let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("mesh JSON: {e}"))?;
        match format {
            "meshtastic-json" => next.meshtastic(&v, now, retained)?,
            "radioman-node-v1" => {
                if v["version"].as_u64() != Some(1) {
                    return Err("unsupported observation version".into());
                }
                let node: Node =
                    serde_json::from_value(v["node"].clone()).map_err(|e| e.to_string())?;
                node.validate(now)?;
                next.merge(node);
            }
            _ => return Err("unsupported mesh input format".into()),
        }
        while next.nodes.len() > MAX_NODES {
            let key = next
                .nodes
                .iter()
                .min_by_key(|(_, n)| n.last_heard)
                .unwrap()
                .0
                .clone();
            next.nodes.remove(&key);
        }
        while next.links.len() > MAX_LINKS {
            let key = next
                .links
                .iter()
                .min_by_key(|(_, n)| n.last_heard)
                .unwrap()
                .0
                .clone();
            next.links.remove(&key);
        }
        *self = next;
        Ok(())
    }
    fn merge(&mut self, mut n: Node) {
        let key = (n.protocol.clone(), n.id.clone());
        if let Some(old) = self.nodes.get(&key) {
            if n.last_heard < old.last_heard {
                return;
            }
            if n.position.as_ref().map(|p| p.updated_at).unwrap_or(0)
                < old.position.as_ref().map(|p| p.updated_at).unwrap_or(0)
            {
                n.position = old.position.clone()
            }
            if n.label == n.id {
                n.label = old.label.clone()
            }
            n.battery_percent = n.battery_percent.or(old.battery_percent);
            n.role = n.role.or(old.role.clone());
        }
        self.nodes.insert(key, n);
    }
    fn meshtastic(&mut self, v: &Value, now: u64, retained: bool) -> Result<(), String> {
        let node_id = id(&v["from"])?;
        let heard = match v["timestamp"].as_u64().filter(|t| *t != 0) {
            Some(t) => timestamp(t, now)?,
            None if !retained => now,
            None => return Err("retained observation requires original timestamp".into()),
        };
        let p = &v["payload"];
        let mut n = Node {
            protocol: Protocol::Meshtastic,
            id: node_id.clone(),
            label: node_id.clone(),
            last_heard: heard,
            position: None,
            battery_percent: None,
            rssi_dbm: number(v, "rssi", -200.0, 0.0),
            snr_db: number(v, "snr", -100.0, 100.0),
            role: None,
        };
        match v["type"].as_str().ok_or("missing Meshtastic packet type")? {
            "position" => {
                let lat = p["latitude_i"].as_i64().ok_or("missing latitude_i")? as f64 / 1e7;
                let lon = p["longitude_i"].as_i64().ok_or("missing longitude_i")? as f64 / 1e7;
                if lat == 0.0 && lon == 0.0 {
                    return Err("unset Meshtastic position".into());
                }
                let t = p["timestamp"]
                    .as_u64()
                    .filter(|t| *t != 0)
                    .or_else(|| p["time"].as_u64().filter(|t| *t != 0))
                    .unwrap_or(heard);
                n.position = Some(Position {
                    lat,
                    lon,
                    updated_at: timestamp(t, now)?,
                    precision_m: None,
                });
            }
            "nodeinfo" => {
                let label = p["longname"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&node_id);
                n.label.clear();
                for c in label.trim().chars().filter(|c| !c.is_control()) {
                    if n.label.len() + c.len_utf8() > 32 {
                        break;
                    }
                    n.label.push(c);
                }
                if n.label.is_empty() {
                    n.label = node_id.clone();
                }
                n.role = p["role"]
                    .as_u64()
                    .filter(|r| *r <= u32::MAX as u64)
                    .map(|r| format!("meshtastic-role-{r}"));
            }
            "telemetry" => n.battery_percent = number(p, "battery_level", 0.0, 100.0),
            "neighborinfo" => {
                let from = id(&p["node_id"])?;
                if from != node_id {
                    return Err("neighbor report source mismatch".into());
                }
                let neighbors = p["neighbors"].as_array().ok_or("missing neighbors")?;
                if neighbors.len() > 64 {
                    return Err("too many neighbors".into());
                }
                for neighbor in neighbors {
                    let to = id(&neighbor["node_id"])?;
                    if to != from {
                        let key = (Protocol::Meshtastic, from.clone(), to.clone());
                        if self.links.get(&key).is_none_or(|l| heard >= l.last_heard) {
                            self.links.insert(
                                key,
                                Link {
                                    protocol: Protocol::Meshtastic,
                                    from: from.clone(),
                                    to,
                                    last_heard: heard,
                                },
                            );
                        }
                    }
                }
            }
            "text" => {}
            _ => return Err("unsupported Meshtastic packet type".into()),
        }
        n.validate(now)?;
        self.merge(n);
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_snapshots_and_sparse_nodes() {
        let mut s = Store::default();
        for i in 1..=140 {
            let bytes=serde_json::to_vec(&serde_json::json!({"from":i,"type":"nodeinfo","timestamp":100,"payload":{"longname":" 😀😀😀😀😀😀😀😀😀😀😀\n ","role":2}})).unwrap();
            s.ingest("meshtastic-json", &bytes, 110, false).unwrap();
        }
        let snapshot = s.snapshot(110);
        assert_eq!(snapshot.nodes.len(), 128);
        assert!(
            snapshot
                .nodes
                .iter()
                .all(|n| n.label.len() <= 32 && n.position.is_none())
        );
        for i in 0..256 {
            let from = format!("{:0>64}", i);
            let to = "\"".repeat(64);
            s.links.insert(
                (Protocol::Meshcore, from.clone(), to.clone()),
                Link {
                    protocol: Protocol::Meshcore,
                    from,
                    to,
                    last_heard: 100,
                },
            );
        }
        assert!(serde_json::to_vec(&s.snapshot(110)).unwrap().len() <= 128 * 1024);
    }
    #[test]
    fn telemetry_does_not_refresh_position() {
        let mut s = Store::default();
        s.ingest("meshtastic-json",br#"{"from":42,"type":"position","timestamp":100,"payload":{"latitude_i":476000000,"longitude_i":-1223000000,"time":90}}"#,110,false).unwrap();
        s.ingest(
            "meshtastic-json",
            br#"{"from":42,"type":"telemetry","timestamp":110,"payload":{"battery_level":101}}"#,
            110,
            false,
        )
        .unwrap();
        let n = &s.snapshot(110).nodes[0];
        assert_eq!(n.position.as_ref().unwrap().updated_at, 90);
        assert_eq!(n.last_heard, 110);
        assert_eq!(n.battery_percent, None);
    }
    #[test]
    fn retained_without_time_is_rejected() {
        assert!(
            Store::default()
                .ingest(
                    "meshtastic-json",
                    br#"{"from":42,"type":"text","payload":{}}"#,
                    110,
                    true
                )
                .is_err()
        );
    }
    #[test]
    fn observed_neighbors_only() {
        let mut s = Store::default();
        s.ingest("meshtastic-json",br#"{"from":42,"type":"neighborinfo","timestamp":100,"payload":{"node_id":42,"neighbors":[{"node_id":43}]}}"#,110,false).unwrap();
        assert_eq!(s.snapshot(110).links.len(), 1);
        assert!(s.snapshot(86511).nodes.is_empty());
    }
    #[test]
    fn invalid_coordinates_do_not_mutate() {
        let mut s = Store::default();
        assert!(s.ingest("meshtastic-json",br#"{"from":42,"type":"position","timestamp":100,"payload":{"latitude_i":990000000,"longitude_i":3}}"#,110,false).is_err());
        assert!(s.snapshot(110).nodes.is_empty());
    }
    #[test]
    fn meshcore_without_fix_is_preserved() {
        let mut s = Store::default();
        s.ingest("radioman-node-v1",br#"{"version":1,"node":{"protocol":"meshcore","id":"abc","label":"Repeater","last_heard":100,"position":null,"battery_percent":null,"rssi_dbm":-90,"snr_db":8,"role":"repeater"}}"#,110,true).unwrap();
        assert!(s.snapshot(110).nodes[0].position.is_none());
    }
}
