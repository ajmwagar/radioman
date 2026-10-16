# Weather radio and satellite receive

Radioman supports receive-only HackRF spectrum, narrowband FM audio, and bounded
SigMF IQ recordings. AIS can continue on a separate RTL-SDR. Satellite image or
message decoding is not implemented here: feed the IQ recording to a decoder
such as SatDump after verifying the antenna, frequency, bandwidth, and signal.

## Prerequisites

Build with Rust 1.89 or newer. The pinned `stream-descriptors` dependency needs
access to the FuturePresentLabs/unibus repository. Install `hackrf`,
`libsoapysdr-dev`, `soapysdr-module-hackrf`, `cmake`, and `build-essential` on the Pi.
For HackRF NFM, build the external receive tool:

```sh
git clone https://github.com/rxseger/rx_tools.git
cd rx_tools
git checkout 811b21c4c8a592515279bd19f7460c6e4ff0551c
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j2
sudo cmake --install build
```

RTL-SDR continues to use `rtl_sdr` and `rtl_fm`. HackRF IQ uses
`hackrf_transfer -r`, signed 8-bit IQ at 2 MS/s. Its RF amplifier and antenna
power are off. Gain is VGA gain rounded to the nearest 2 dB, with LNA fixed at
16 dB. No transmit path is added. Spectrum rates and formats are derived from
the selected backend in both the DCP catalog and the receiver process.

## Weather audio

Install the built Radioman binary to `/usr/local/bin/radioman`, then install
`deploy/radioman-weather.service` to `/etc/systemd/system/` and copy
`deploy/weather.env.example` to `/etc/radioman/weather.env`. Set its radio ID
from `radioman device-index /etc/radioman/radioman.toml`, local weather frequency,
and destination. The example selects Seattle's 162.550 MHz channel.

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now radioman-weather
journalctl -u radioman-weather -n 40 --no-pager
```

The stream is headerless stereo float32 little-endian PCM at 48 kHz. The
demodulator produces 24 kHz mono, duplicated into stereo and repeated twice
in time to match the existing 48 kHz PCM interface. Datagram size is 960 bytes.
For the example multicast destination, listen from the same LAN:

```sh
ffplay -f f32le -ar 48000 -ch_layout stereo 'udp://239.255.77.1:50072?reuse=1'
```

If a computer has multiple interfaces, add `&localaddr=YOUR_LAN_IP` to the URL.
This is a LAN audio stream, not an authenticated Internet endpoint. Raw weather
audio does not provide decoded SAME alerts or structured weather observations.

## L-band spectrum and recordings

Weather, spectrum, and capture share a receiver lock. Stop weather before using
its HackRF for another purpose; starting a second receiver fails visibly.
The manual weather service is independent of the DCP session queue, so stop it
before submitting a HackRF spectrum session through DCP. External tools such as
`rtl_ais` do not participate in Radioman's locks; keep them on their own device.

```sh
sudo systemctl stop radioman-weather
# An example frequency, not a claim that an antenna can receive this downlink:
sudo -u radioman timeout 15 radioman spectrum /etc/radioman/radioman.toml \
  1694100000 239.255.77.1:50070 20 null hackrf/78d063dc29876f67
sudo -u radioman timeout 20 radioman capture /etc/radioman/radioman.toml \
  hackrf/78d063dc29876f67 1694100000 5 /var/lib/radioman/lband-test 20
sudo systemctl start radioman-weather
```

Capture accepts 1–60 seconds, refuses to overwrite files, and writes
`.sigmf-data` plus `.sigmf-meta`. HackRF data is `ci8` at 2 MS/s (4 MB/s).
Only completed captures receive valid metadata; errors leave incomplete files
for diagnosis. The external timeout bounds a stalled driver. The current fixed
2 MS/s backend is for narrow L-band experiments, not arbitrary wideband modes.
Successful USB sampling does not prove satellite lock, image decoding, antenna
gain, polarization, or amplifier power. Choose those for the intended satellite.

## Recovery

Before replacing a deployed binary, retain a copy. To roll back, stop weather,
restore that copy to `/usr/local/bin/radioman`, and restart `radioman` and
`radioman-dcp`. AIS has separate services and need not be stopped.
