//! Receiver-location enrichment. Aircraft/subject position stays in the payload.
use crate::decoder::DecodedPacket;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationSource {
    Gps,
    Surveyed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: Option<f64>,
    pub accuracy_m: Option<f64>,
    pub source: LocationSource,
    pub measured_at_unix_ms: u64,
    /// None denotes a fixed-site location, not an indefinitely fresh mobile fix.
    pub valid_until_unix_ms: Option<u64>,
}

impl SiteLocation {
    pub fn validate(&self) -> Result<(), String> {
        if !self.latitude.is_finite()
            || !self.longitude.is_finite()
            || !(-90.0..=90.0).contains(&self.latitude)
            || !(-180.0..=180.0).contains(&self.longitude)
            || self.altitude_m.is_some_and(|a| !a.is_finite())
            || self.accuracy_m.is_some_and(|a| !a.is_finite() || a < 0.0)
            || self.measured_at_unix_ms == 0
            || self
                .valid_until_unix_ms
                .is_some_and(|t| t <= self.measured_at_unix_ms)
        {
            return Err("invalid site location or evidence timing".into());
        }
        Ok(())
    }
}

/// One source of truth per site; receiver assignments are explicit, not inferred from LAN.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteRegistry {
    pub sites: BTreeMap<String, SiteLocation>,
    pub receivers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocatedPacket {
    pub packet: DecodedPacket,
    pub reception: ReceptionLocation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceptionLocation {
    pub site_id: String,
    pub location: SiteLocation,
}

impl SiteRegistry {
    pub fn validate(&self) -> Result<(), String> {
        for (id, location) in &self.sites {
            crate::validate_id("site", id)?;
            location.validate()?;
        }
        for (receiver, site) in &self.receivers {
            crate::validate_id("receiver", receiver)?;
            if !self.sites.contains_key(site) {
                return Err(format!(
                    "receiver {receiver} references unknown site {site}"
                ));
            }
        }
        Ok(())
    }

    /// Rust host enriches the original packet AFTER the Lua route/filter hook.
    /// No mutation or fallback to another site's GPS is permitted.
    pub fn locate(&self, packet: DecodedPacket) -> Result<LocatedPacket, String> {
        self.validate()?;
        packet.validate()?;
        let site_id = self
            .receivers
            .get(&packet.source)
            .ok_or("receiver has no site assignment")?;
        let location = &self.sites[site_id];
        if location.measured_at_unix_ms > packet.observed_at_unix_ms
            || location
                .valid_until_unix_ms
                .is_some_and(|t| packet.observed_at_unix_ms >= t)
        {
            return Err("site location evidence is future or expired at reception".into());
        }
        let result = LocatedPacket {
            packet,
            reception: ReceptionLocation {
                site_id: site_id.clone(),
                location: location.clone(),
            },
        };
        if serde_json::to_vec(&result)
            .map_err(|e| e.to_string())?
            .len()
            > 64 * 1024
        {
            return Err("located packet exceeds 64 KiB".into());
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registry() -> SiteRegistry {
        SiteRegistry {
            sites: BTreeMap::from([(
                "lab".into(),
                SiteLocation {
                    latitude: 47.6,
                    longitude: -122.3,
                    altitude_m: Some(30.0),
                    accuracy_m: Some(5.0),
                    source: LocationSource::Gps,
                    measured_at_unix_ms: 1000,
                    valid_until_unix_ms: None,
                },
            )]),
            receivers: BTreeMap::from([
                ("roof".into(), "lab".into()),
                ("desk".into(), "lab".into()),
            ]),
        }
    }
    fn packet(source: &str) -> DecodedPacket {
        DecodedPacket {
            source: source.into(),
            session_id: "session-1".into(),
            sequence: 1,
            observed_at_unix_ms: 2000,
            contract: "radioman.fixture.v1".into(),
            value: serde_json::json!({"position":{"latitude":48.0,"longitude":-123.0}}),
        }
    }
    #[test]
    fn shared_site_is_explicit_and_subject_position_is_unchanged() {
        let registry = registry();
        let original = packet("roof");
        let located = registry.locate(original.clone()).unwrap();
        assert_eq!(located.packet, original);
        assert_eq!(located.reception.location.latitude, 47.6);
        assert_eq!(
            registry.locate(packet("desk")).unwrap().reception,
            located.reception
        );
        assert!(registry.locate(packet("unknown")).is_err());
        let json = serde_json::to_string(&located).unwrap();
        assert_eq!(
            serde_json::from_str::<LocatedPacket>(&json).unwrap(),
            located
        );
    }
    #[test]
    fn invalid_assignment_location_and_stale_fixes_fail_loudly() {
        let mut r = registry();
        r.receivers.insert("other".into(), "missing".into());
        assert!(r.validate().is_err());
        r = registry();
        r.sites.get_mut("lab").unwrap().latitude = 91.0;
        assert!(r.locate(packet("roof")).is_err());
        r = registry();
        r.sites.get_mut("lab").unwrap().valid_until_unix_ms = Some(2000);
        assert!(r.locate(packet("roof")).is_err());
        r = registry();
        r.sites.get_mut("lab").unwrap().measured_at_unix_ms = 3000;
        assert!(r.locate(packet("roof")).is_err());
    }
}
