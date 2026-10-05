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

This is a tested **library boundary**, not a live decoder or daemon integration.
`fixture` is illustrative, not a registered backend. There is no ADS-B decoder,
Unibus sender, automatic file watcher, or dynamic native plugin ABI here.

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
