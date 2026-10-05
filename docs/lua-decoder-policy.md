# Reloadable packet policy

The optional `lua` feature exposes `decoder::lua::PacketPolicy` in an isolated
Lua 5.4 VM. Rust owns device arbitration, IQ samples, native DSP, timestamps,
session/sequence, transport, and publishing. Lua chooses a validated decoder
configuration and returns a stream ID or nil for each decoded packet.

```lua
return {
  config = {
    decoder = 'fixture', source = 'roof-rtl',
    frequency_hz = 1090000000, sample_rate_hz = 2400000,
  },
  on_packet = function(packet)
    return 'radio/observations'
  end,
}
```

`mode_s_1090` is a registered native decoder and the `radioman adsb` command
owns an RTL-SDR, decodes live CU8 samples, and emits canonical packets over
UDP. `fixture` remains illustrative. There is no Unibus sender, automatic file
watcher, or dynamic native plugin ABI here; transport adaptation remains a
separate host boundary.

The host must resolve decoder names against a registry, validate the tuning
against the physical device, acquire exclusive ownership, and invoke a native
`Decoder` implementation through `DecoderSession`. The wrapper rejects duplicate
start, process/stop while stopped, IQ blocks above 1 MiB, decoded batches above
`MAX_BATCH_PACKETS`, malformed packets, and mismatched receiver sources. Native
implementations must also bound allocation internally; post-return validation
cannot prevent a malicious native decoder from allocating. Lua never receives IQ.
Spectrum/census producers can use the same packet wrapper with their existing
owner-defined schema and reception metadata, without relabeling them ADS-B.

Resource bounds: 64 KiB script and serialized packet, 4 MiB Lua heap, 100,000
instruction hook per call. Only table/string/math libraries are loaded; no
filesystem, OS, package loading, or protected-call bypass. Each call returns at
most one route. Errors propagate; no silent drop-on-error policy. Nil is an
intentional filter result. Limits are guardrails for trusted policy, not an
adversarial isolation/security claim.

Reload parses and validates a fresh VM before replacement. Decoder/source/tuning
changes are rejected by reload: host stop/arbitrate/start must be explicit.
Reception metadata remains in the original Rust packet; hook mutation affects
only its temporary Lua copy. The host validates allowed destination streams and
payload schemas before constructing canonical Unibus `StreamEvent`; it handles
bounded buffering/loss, leases, sequence, and reconnect independently.

Manual runbook: select a registered native decoder, confirm device/tuning,
start native receive, validate decoded packets, apply route/filter, publish to
an allowed stream, inspect transport loss/health, stop on decoder failure.
Panopticon consumes retained state; Elmers consumes reconciled business effects,
not one workflow per IQ sample or packet.

Verify locally: `cargo check --features lua`, then `cargo test --features lua`.

## Receiver site location

`location::SiteRegistry` stores one location per physical site and explicitly
assigns receiver source IDs to site IDs. A captured GPS measurement can establish
a fixed site's coordinates without GPS attached to every receiver. Sharing a
LAN does not assign a physical site.

After Lua routing, the host calls `registry.locate(original_packet)`. The returned
`LocatedPacket` keeps the packet unchanged and adds separate `reception` metadata:
site ID, latitude/longitude, optional altitude/accuracy, GPS/surveyed evidence,
measurement time and optional validity deadline. Coordinates are WGS84 decimal
degrees; altitude is meters and its datum must be normalized by the provider.
Unknown altitude and accuracy remain absent. The deadline is exclusive; no
deadline denotes a fixed site, not an indefinitely fresh mobile GPS fix. Moving
equipment requires updating the assignment; mobile fixes require deadlines.
Invalid registry references, coordinates, future/expired evidence, and total
serialized size above 64 KiB fail loudly.

This is a tested library API, not a GPS collector or deployed registry. The
live ADS-B command accepts an explicit fixed receiver location and never
borrows GPS from another site.

Panopticon should use aircraft coordinates from ADS-B payloads for aircraft
tracks and retain reception location separately as provenance. RF summaries and
receiver-health records use site coordinates, explicitly labeled `receiver_site`.
Never replace an unknown aircraft position with receiver position. Digges'
required altitude still needs a missing-altitude representation before live
ingestion; do not invent measured height. Location evidence age and packet/track
freshness are distinct.

Elmers can group retained receiver health and reports by stable site ID, reconcile
one incident per receiver/site condition, and keep aircraft identity separate
from observer identity. These consumers are not wired by this library addition.
