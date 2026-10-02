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

Bounded, decimated spectrum frames may travel through Unibus. Each frame carries
one FFT power row; consumers derive its frequency axis, peaks, persistence, and
waterfall history. Raw IQ uses a direct `udp://` or `quic://` endpoint announced
by Radioman.

## Development

```sh
cargo check
cargo test
cargo run -- validate-config ../radioman-pi/config/radioman.toml
```

The first hardware milestone is receive-only RTL-SDR. HackRF receive follows;
transmit is a separate safety-reviewed capability with frequency/power limits,
short leases, audit receipts, and a host-local enable interlock.
