//! Read-only service projection. Radioman owns device indexing and tuner state;
//! Unibus only distributes this inventory. Commands remain with the DCP owner.

use stream_descriptors::service::{
    HealthState, SERVICE_CATALOG_KIND, SERVICE_HEALTH_KIND, ServiceCatalog, ServiceHealth,
    ServiceOperation, ServiceResource,
};

use crate::{DeviceIndex, ServiceStatus};

pub fn project(
    devices: &DeviceIndex,
    status: &ServiceStatus,
) -> Result<(ServiceCatalog, ServiceHealth), String> {
    devices.validate()?;
    if status.schema_version != crate::CONTRACT_VERSION || status.observed_at_ms == 0 {
        return Err("invalid tuner status evidence".into());
    }
    let service_id = format!("radioman.{}", devices.node_id);
    let reference = format!("node/{}", devices.node_id);
    let until = status
        .observed_at_ms
        .checked_add(6000)
        .ok_or("status lease overflow")?;
    let mut resources = Vec::new();
    for radio in &devices.radios {
        resources.push(ServiceResource {
            id: format!("radio/{}", radio.id),
            kind: "radio.sdr".into(),
            display_name: radio.label.clone(),
            stream_ids: vec![],
            // The DCP catalog owns specific revision-bound actions and parameter
            // validation. This advertises its request family, not a new executor.
            operations: if radio.receive && status.error.is_none() {
                vec![ServiceOperation {
                    id: "dcp".into(),
                    request_kind: "dcp.execute.v1".into(),
                    requires: "dcp.client".into(),
                }]
            } else {
                vec![]
            },
        });
    }
    for radio in &devices.packet_radios {
        resources.push(ServiceResource {
            id: format!("radio/{}", radio.id),
            kind: "radio.modem".into(),
            display_name: radio.label.clone(),
            stream_ids: vec![],
            operations: vec![],
        });
    }
    let catalog = ServiceCatalog {
        contract: SERVICE_CATALOG_KIND.into(),
        service_id: service_id.clone(),
        adapter_kind: "radioman".into(),
        owner_reference: reference.clone(),
        presenter_reference: reference,
        observed_at_unix_ms: status.observed_at_ms,
        valid_until_unix_ms: until,
        resources,
    };
    let health = ServiceHealth {
        contract: SERVICE_HEALTH_KIND.into(),
        service_id,
        state: if status.error.is_some() {
            HealthState::Degraded
        } else {
            HealthState::Healthy
        },
        reason: status.error.as_ref().map(|_| "tuner_status_error".into()),
        observed_at_unix_ms: status.observed_at_ms,
        valid_until_unix_ms: until,
    };
    catalog.validate()?;
    health.validate()?;
    Ok((catalog, health))
}

pub fn unreachable(
    devices: &DeviceIndex,
    observed_at_ms: u64,
) -> Result<(ServiceCatalog, ServiceHealth), String> {
    let status = ServiceStatus {
        schema_version: crate::CONTRACT_VERSION,
        active: None,
        queued_session_ids: vec![],
        observed_at_ms,
        error: Some("control_unreachable".into()),
    };
    let (catalog, mut health) = project(devices, &status)?;
    health.state = HealthState::Disconnected;
    health.reason = Some("control_socket_unreachable".into());
    Ok((catalog, health))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_status_are_projected_without_claiming_streams_or_tuner_capacity() {
        let devices: DeviceIndex = serde_json::from_value(serde_json::json!({
            "schema_version":1,"node_id":"radio-pi","radios":[{
                "id":"rtl-sdr","kind":"rtl_sdr","label":"RTL-SDR","serial":null,
                "frequency":{"minimum_hz":24000000,"maximum_hz":1766000000},
                "sample_rates_hz":[2048000],"sample_formats":["cu8"],"receive":true,"transmit":false
            }],"packet_radios":[]
        }))
        .unwrap();
        let mut status = ServiceStatus {
            schema_version: 1,
            active: None,
            queued_session_ids: vec![],
            observed_at_ms: 100,
            error: None,
        };
        let (catalog, health) = project(&devices, &status).unwrap();
        assert_eq!(catalog.resources[0].id, "radio/rtl-sdr");
        assert!(catalog.resources[0].stream_ids.is_empty());
        assert_eq!(catalog.valid_until_unix_ms, 6100);
        assert_eq!(health.state, HealthState::Healthy);
        status.error = Some("private upstream failure".into());
        let (catalog, health) = project(&devices, &status).unwrap();
        assert_eq!(health.state, HealthState::Degraded);
        assert!(
            !serde_json::to_string(&(catalog, health))
                .unwrap()
                .contains("private")
        );
        let (catalog, health) = unreachable(&devices, 100).unwrap();
        assert!(catalog.resources[0].operations.is_empty());
        assert_eq!(health.state, HealthState::Disconnected);
        status.observed_at_ms = u64::MAX;
        assert!(project(&devices, &status).is_err());
    }
}
