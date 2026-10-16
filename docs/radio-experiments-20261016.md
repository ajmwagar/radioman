# Receive experiments — 2026-10-16

## Outcome

The Pi's receivers initially enumerated and tuned but delivered no usable USB
samples. Individually resetting the Blog V4 and HackRF USB devices restored
sampling. Recent kernel logs also contained repeated undervoltage/normalization
events; `sudo vcgencmd get_throttled` reported `0x50000` (historical flags) and
61.3 C. This does not prove that undervoltage caused the stalled transfers.
Check the Pi supply, USB hub supply, cables, and connected antennas before
interpreting poor RF reception as a decoder problem.

- Weather: scanned all seven NOAA channels with both receivers. RTL's initial
  strongest peak was exactly 162.500 MHz, also the scan center; treat it as a
  possible tuner artifact. Other RTL channel contrasts were only 0.1–0.2 dB.
  Repeating with shifted scan bounds removed the 162.500 MHz peak; that
  channel's contrast fell to 0.2 dB, so the original peak is not evidence of
  a weather transmitter.
  HackRF's largest weather-channel contrast was 3.9 dB near 162.450 MHz, with
  2.2 dB near 162.550 MHz. These are peak-to-nearby-median spectral contrasts,
  not comparable calibrated receiver sensitivities or demodulated SNR. No clean
  weather broadcast has been confirmed. Saved 14-second audio samples from RTL
  at 162.550 MHz and HackRF at 162.450 MHz. Antennas were not physically verified
  or swapped, so this is not a controlled same-antenna comparison.
- ADS-B: existing RTL driver at 1090 MHz, 2.4 MS/s, 49.6 dB gain; readsb file
  decoder in UC8 mode with CRC correction disabled. Processed 154,271,744
  complex samples in roughly 64 seconds; 131,072 samples lost, zero valid
  messages, zero aircraft tracks. Preamble candidates are not received aircraft.
  No aircraft were injected into Panopticon/Galileo. File-mode decoder clock
  output included invalid epoch dates; those timestamps must not be ingested.
- L-band: HackRF completed five sweeps requested over 1500–1710 MHz at roughly
  98 kHz resolution (tool rounds coverage to 20 MHz hardware chunks). Saved a
  five-second, 1545 MHz, 2 MS/s signed-IQ recording: 20,000,000 bytes plus SigMF
  metadata. This demonstrates recording, not satellite identification, lock,
  or message/image decoding. Peaks may include local interference and receiver
  artifacts. RF amplifier and antenna power remained off throughout.

AIS and the original 162.550 MHz HackRF weather service are restored after
experiments. No new always-on decoder or third-party feed sharing was enabled.
The Debian readsb package was extracted into the experiment directory, not
installed as a system service.

## Artifacts

Pi CSV/log/audio directory: `/var/tmp/radioman-experiments-20261016`.
Pi SigMF: `/var/lib/radioman/experiments-20261016/lband-1545.sigmf-{data,meta}`.
Local copies and reports: `artifacts/experiments-20261016/`; local WAVs:
`artifacts/weather-rtl-20261016.wav` and
`artifacts/weather-hackrf-162450-20261016.wav`. Artifacts are gitignored.

## Manual runbook

Use `ssh radioman-pi` (the Mycelium certificate profile). Confirm storage,
`systemctl is-active radioman-weather radioman-ais-rx radioman-ais`, and current
kernel power/USB errors before testing. External radio tools do not participate
in Radioman's advisory lock: stop the owning services first and use a shell EXIT
trap to restore their original running state. Do not reset a hub or the USB
Ethernet device. If transfers stall, stop receiver owners and reset only the
specific receiver with `sudo usbreset 0bda:2838` or `sudo usbreset 1d50:6089`.

Weather scan (RTL requires pausing both AIS services):

```sh
sudo timeout -s INT -k 3 25 rtl_power -d 00000001 -g 40.2 \
  -f 162350000:162650000:1000 -i 3 -e 9 - > weather.csv
# Shift scan bounds and repeat to test whether a peak follows the tuner center.
```

HackRF weather sweep (pause weather; FFT planning can take tens of seconds):

```sh
sudo timeout -s INT -k 3 55 hackrf_sweep -a 0 -p 0 \
  -f 162:164 -w 5000 -l 24 -g 28 -N 4 > weather-hackrf.csv
```

ADS-B (pause both AIS services; use an existing writable output directory and
readsb binary, not an installer). `--device-type` must precede `--ifile`:

```sh
sudo timeout -s INT -k 3 65 rtl_sdr -d 00000001 -f 1090000000 \
  -s 2400000 -g 49.6 - 2>adsb-rx.log |
  readsb --device-type=ifile --ifile=- --iformat=UC8 --stats --no-fix \
    --write-json=adsb --write-json-every=5 >adsb-decode.log 2>&1
```

A timeout status of 124 is expected for bounded continuous receivers; check the
file sizes and decoder counters separately. Exit zero alone is not proof of RF
reception. Keep networking disabled for an isolated baseline.

L-band (pause weather; AIS can run on the separate RTL):

```sh
sudo timeout -s INT -k 3 45 hackrf_sweep -a 0 -p 0 \
  -f 1500:1710 -w 100000 -l 24 -g 28 -N 5 > lband.csv
sudo -u radioman timeout -k 3 20 radioman capture \
  /etc/radioman/radioman.toml hackrf/78d063dc29876f67 \
  1545000000 5 /var/lib/radioman/NEW-UNUSED-PREFIX 28
```

Analyze CSV without fetching the workspace dependencies:

```sh
rustc --edition 2024 examples/power-report.rs -o /tmp/radioman-power-report
/tmp/radioman-power-report weather.csv \
  162400000 162425000 162450000 162475000 162500000 162525000 162550000
rustc --edition 2024 --test examples/power-report.rs -o /tmp/power-report-tests
/tmp/power-report-tests
```

The report averages repeated bin observations in linear power before converting
back to dB, rejects nonfinite/empty data, and reports relative tool units. It
cannot compare absolute levels across receivers or identify transmitters.
Standalone compilation and both report tests passed. Workspace example checking
was attempted offline but blocked by the uncached pinned `stream-descriptors`
git dependency; no production binary was changed.

## Next physical checks

Confirm which antenna is connected to each receiver and its placement. For a
simple quarter-wave element, free-space starting lengths are about 46 cm at
162.55 MHz, 6.9 cm at 1090 MHz, and 4.9 cm at 1545 MHz; actual antennas also need
an appropriate ground plane/counterpoise, feedline, and polarization. An L-band
length alone does not provide satellite antenna gain. Choose a specific downlink
before choosing a directional antenna, filter/LNA, and decoder.

References: [NWS weather radio frequencies](https://www.weather.gov/nwr/),
[HackRF tools](https://hackrf.readthedocs.io/en/stable/hackrf_tools.html),
[Debian readsb upstream](https://github.com/adsbfi/readsb).
