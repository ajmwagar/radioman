# Receive-only radio mesh projection

`radioman mesh-serve CONFIG.toml` subscribes through the existing
`unibus-mqtt::subscriber::ReportSubscriber`, which supplies TLS certificate
pinning, bounded payloads, exact topic subscriptions and no publication API.
Unibus MQTT bridge ownership and upstream radio uplinks remain unchanged.

Runbook: enroll the broker leaf certificate SHA256 independently, provision a
broker account whose ACL grants read access to the required exact topics, put
its password in a private file, configure those topics below, run mesh-serve,
and inspect `/api/mesh/health` before using `/api/mesh/snapshot`. No radio
transmission, public broker subscription, device configuration or deployment
is performed by this command. HTTP binds loopback only; remote consumers use
an authenticated existing Unibus service or SSH forward.

```toml
listen = "127.0.0.1:18683"
mqtt_endpoint = "127.0.0.1:8883"
client_id = "radioman-mesh"
username = "radioman-readonly"
password_file = "/etc/radioman/mesh-mqtt-password"
certificate_sha256 = "REPLACE_WITH_64_HEX_ENROLLED_LEAF_CERTIFICATE_SHA256"

[[subscriptions]]
topic = "msh/US/2/json/LongFast/!YOUR_GATEWAY_ID"
format = "meshtastic-json"

[[subscriptions]]
topic = "radio/meshcore/observations"
format = "radioman-node-v1"
```

At most 8 subscriptions, 64 KiB per message, 128 nodes and 256 links are retained.
Records expire 24h after their original observation. The oldest observation is
evicted at capacity. Snapshot JSON is bounded at most 128 KiB with these limits.
Bad messages are logged and recorded in health; mutation is atomic. MQTT
connection failures are visible in health while cached observations preserve
their original age. A cached snapshot is not proof the MQTT source is live.

## Snapshot ABI v1

`GET /api/mesh/snapshot` returns:

```json
{"version":1,"observed_at":1730000060,"nodes":[{"protocol":"meshcore","id":"PUBLIC_KEY","label":"Repeater","last_heard":1730000000,"position":{"lat":47.65,"lon":-122.35,"updated_at":1730000000},"rssi_dbm":-95,"snr_db":7.5,"role":"repeater"}],"links":[]}
```

Times are Unix seconds. Optional node fields are `position`,
`battery_percent`, `rssi_dbm`, `snr_db`, `role`. Position optionally includes
`precision_m`. Nodes without advertised positions stay in the list and must
not be placed on a map. `position.updated_at` is independent from `last_heard`:
telemetry does not refresh a previous fix. Position age must remain visible;
no interpolation or invented GPS fix is provided. Links contain
`protocol,from,to,last_heard`; they describe directed reported neighbors,
not assumptions based on gateway receipt or geographic proximity.

Meshtastic inputs follow the firmware MQTT JSON serializer: `from` is the
origin node number, `timestamp` is receiver Unix time, `payload.latitude_i`
and `longitude_i` are degrees times 1e7, and position `time`/`timestamp` is
retained separately. `nodeinfo` supplies `longname`/numeric role;
`telemetry` supplies `battery_level` (101 means external power, so is not a
battery percentage); `neighborinfo` supplies explicit neighbor node IDs.
Unsupported/encrypted/protobuf packets are rejected visibly. Retained
messages without original timestamps are rejected. Live unretained messages
without rx timestamps use local receive time; zero positions are rejected
because Meshtastic uses them for an unset location.

References: [firmware JSON serializer](https://github.com/meshtastic/firmware/blob/master/src/serialization/MeshPacketSerializer.cpp),
[MQTT configuration](https://meshtastic.org/docs/configuration/module/mqtt/).

## MeshCore input boundary

MeshCore does not specify a universal MQTT JSON contract. The
`radioman-node-v1` adapter accepts a narrow owner exporter envelope:

```json
{"version":1,"node":{"protocol":"meshcore","id":"PUBLIC_KEY","label":"Repeater","last_heard":1730000000,"position":{"lat":47.65,"lon":-122.35,"updated_at":1730000000},"role":"repeater"}}
```

An exporter must derive these fields from actual advertisements/contact
observations. It must not stamp a cached contact as newly heard. No raw
MeshCore packet decoder or serial receiver is claimed here; wiring that
exporter requires the actual device/feed owner schema. This input supports
both protocols, but native Meshtastic JSON should use the native adapter.
MeshCore links remain empty until an owner publishes documented observed
neighbor information through an explicit ABI extension.

Identity limits are 64 UTF8 bytes for IDs, 32 for labels and roles. Native
Meshtastic labels are trimmed, control characters removed, and truncated at
UTF8 character boundaries. Observation timestamps allow at most 30s future
clock skew. The loopback HTTP staging endpoint is not a room visitor ACL;
room view/control authorization belongs to the authenticated publishing
boundary, which must be installed before exposing it to room participants.
