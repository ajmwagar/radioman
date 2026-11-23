# Soil radio / DSP boundary

The RF console's Python bridge is a web transport, not the DSP destination.
The receive path belongs in the Rust native owner:

```text
USB / SoapySDR / rtl_sdr
  → native IQ blocks
  → rustfft spectrum row
  → Demodulator (AM / NFM / WFM / USB / LSB)
  → bounded PCM frames
  → soil:radio consumer or soil:playback sink
```

`src/dsp.rs` is the first native implementation. It uses the existing
`num-complex` and `rustfft` dependencies; no monolithic DSP framework is
required. A resampling/filter crate is optional:

- `rubato` if fractional sample-rate conversion needs to be high quality;
- `biquad` if the one-pole de-emphasis/low-pass chain needs steeper filters.

The current detector is deliberately small and testable:

- FM discriminator with peak-deviation scaling;
- AM envelope with a slow DC blocker;
- 75 us de-emphasis for NFM/WFM;
- cascaded one-pole anti-alias low-pass;
- bounded linear fractional resampler to 48 kHz;
- output clamped to `[-1, 1]`.

## WIT boundary

`wit/soil-radio.wit` exposes a receiver resource with bounded `spectrum` and
`audio` operations. The host owns enumeration, leases, USB, and the clock.

`wit/soil-playback.wit` is the mixer/patchbay above a host-owned PCM sink. A
Spore can connect sources, set gain, pause, and resume; it cannot enumerate or
name physical audio devices.

The next adapter is a wasm component that exports `soil:radio` (or
`soil:playback`) and imports `loam:plugin/ipc@0.1.0`. Its sidecar can be the
native Rust owner. The adapter must not be the place that opens USB or holds a
device lease.

## Migration

1. Keep the existing HTTP JSON bridge only as the web transport.
2. Route FFT and demod blocks through the Rust owner.
3. Add the wasm adapter and package it as a Spore plugin.
4. Move browser audio to `soil:playback` over the host sink; do not push PCM
   through the Python bridge.
