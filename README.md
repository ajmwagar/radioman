# Radioman

Radioman owns software-defined-radio devices, tuning, exclusive sessions, DSP,
recording, and the high-rate IQ data plane. Unibus may route Radioman's bounded
commands and status events, but the Unibus router does not understand SDR
semantics and never carries raw IQ as ordinary messages.

## Boundary

- Radioman owns radio identity, validation, arbitration, drivers, FFT/DSP,
  SigMF recording, and IQ transport.
- Unibus is an optional command/status adapter and routes opaque envelopes by
  configured capability.
- Mycelium is an optional observation provider. Radioman must also work from
  local enumeration or explicit configuration.
- Discovery establishes availability, never permission. Unibus grants remain
  the authority for remote control.

The initial crate contains the versioned receive contracts and deterministic
exclusive-session state machine. It deliberately has no HackRF/RTL-SDR FFI and
does not pretend that validation starts hardware. Backends will implement the
accepted transitions behind a narrow owner-side interface.

## Initial contracts

- `Command::StartRx` / `Command::Stop`
- `ReceiverStatus`
- `IqStreamAnnouncement`
- `SpectrumFrame` (`radioman.spectrum.v1`)
- `RadioDescriptor`
- `DeviceIndex` (SDRs plus packet-radio modems)

`radioman device-index CONFIG.toml` emits the validated, provider-neutral
inventory. Config schema 2 stores SDRs in `[[radios]]` and packet modems in
`[[packet_radios]]`; device IDs are unique across both collections. An indexed
HackRF does not imply transmit authority: transmit remains false until a
separate safety-reviewed capability and host-local interlock exist.

Bounded, decimated spectrum frames may travel through Unibus. Each frame carries
one FFT power row; consumers derive its frequency axis, peaks, persistence, and
waterfall history. Raw IQ uses a direct `udp://` or `quic://` endpoint announced
by Radioman.

## DCP provider

Radioman derives a DCP 0.1 catalog from its validated `DeviceIndex` and live
`ServiceStatus`. When the tuner is idle it advertises
`radioman.spectrum.start`; while owned it advertises
`radioman.tuner.retune` and `radioman.session.stop`. It never advertises a
transmit decision. Every execute request is checked against the exact catalog
and state revisions before a typed `ServiceCommand` reaches the owner process.

The HTTP surface is loopback-only and bearer authenticated:

```sh
radioman dcp-serve CONFIG.toml /run/radioman/control.sock \
  127.0.0.1:7894 192.168.10.82:50070 /etc/radioman/dcp.token
```

It serves `/.well-known/dcp`, `/v1/decisions`, and
`/v1/decisions/execute`. The generic `unibus-dcp` edge leases this catalog onto
UniBus using `unibus/config/dcp-radioman.json`. Spectrum and IQ payloads remain
on their announced UDP/QUIC paths; DCP and UniBus carry only decisions and
descriptions.

The same bearer-authenticated listener also serves `/v1/catalog` and
`/v1/health` using Unibus's shared service inventory contracts. These read-only
routes do not require DCP-Version; the native DCP routes still do. Inventory
comes from the registered device index and current tuner status, with a
six-second lease. A failed control socket reports disconnected and advertises
no control operation. Hardware connectivity/free capacity is not inferred
from configuration, and the legacy spectrum stream is not mislabeled as v2.
The optional `unibus-services publish` client can distribute these documents
using a locally stored bearer token. Radioman itself gains no Unibus/Mycelium
runtime dependency; controls remain revision-bound DCP actions.

## Development

HackRF, NOAA weather audio, and L-band IQ capture setup are documented in
[weather and satellite receive](docs/weather-and-satellite.md).
The [2026-10-16 receive experiments](docs/radio-experiments-20261016.md) record
weather comparisons, ADS-B decoding counters, L-band captures, USB recovery,
and repeatable bounded commands.

```sh
cargo check
cargo test
cargo run -- validate-config ../radioman-pi/config/radioman.toml
```

### GPU spectrum plugin

`rf-gpu` is an opt-in `wgpu` front-end for FFTs and live spectrum frames. It
receives raw interleaved IQ over UDP and emits the same bounded
`SpectrumFrame` JSON as the CPU spectrum path:

```sh
cargo run --release --features rf-gpu -- rf-gpu \
  0.0.0.0:51001 94900000 127.0.0.1:50070 cs8 30
```

The device side can send raw IQ with the always-available `iq-stream` command:

```sh
radioman iq-stream /etc/radioman/radioman.toml hackrf 94900000 \
  192.168.2.74:51001 30
```

Both sides deliberately use raw interleaved IQ; packet boundaries are not
sample boundaries. `rf-gpu` accumulates a 1024-sample window and drops stale
windows under backpressure. Audio demodulation remains local to the device.

The feature is off by default, so the normal Radioman binary does not pull in
`wgpu`. The GPU boundary is intentionally limited to spectrum/waterfall
generation; audio demodulation stays on the CPU and real-time path. The
`SpectrumConsumer` boundary is where a later GPU waterfall texture or ML
classifier can attach without changing the IQ wire format.

### Native ADS-B decoder

`rf-adsb` is a feature-gated native Mode S/ADS-B receive path. It owns the RTL
IQ stream, demodulates 1090 MHz pulses, validates CRC, decodes identification
and global CPR positions, and emits bounded JSON aircraft events over UDP:

```sh
cargo run --release --features rf-adsb -- rf-adsb \
  /etc/radioman/radioman.toml rtl-sdr 192.168.2.74:50100 24
```

The event stream contains `icao`, `raw_hex`, callsign, altitude, and
latitude/longitude when an even/odd CPR pair has been received. This path does
not produce audio.

### Native AIS decoder

`rf-ais` is a feature-gated dual-channel AIS receive path. It downconverts the
two 25 kHz AIS channels around 162 MHz, FM-discriminates GMSK, recovers the bit
clock, reverses NRZI, removes HDLC stuffing, validates the FCS, and emits JSON
vessel events:

```sh
cargo run --release --features rf-ais -- rf-ais \
  /etc/radioman/radioman.toml rtl-sdr/00000001 192.168.2.74:50101 24
```

The decoder reports message type, MMSI, payload, and position for message types
1–3. It is independent of the ADS-B path and can be enabled separately.

The first hardware milestone is receive-only RTL-SDR. HackRF receive follows;
transmit is a separate safety-reviewed capability with frequency/power limits,
short leases, audit receipts, and a host-local enable interlock.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
