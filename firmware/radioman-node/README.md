# radioman-node

Receive-safe Rust firmware for the LILYGO T-Beam Supreme attached to the
Radioman Pi.

The MCU owns board bring-up, watchdog recovery, GPS, display, and LoRa PHY.
Radioman on the Pi owns scheduling, experiments, routing, decoding, and Unibus.
The boundary is a narrow, versioned USB status/command protocol; Mycelium may
advertise the resulting capability but is not a firmware dependency.

## First slice

- identifies itself from the ESP32-S3 eFuse MAC;
- emits one parseable `radioman.status` health record per second;
- uses a 10-second hardware watchdog;
- advertises receive capabilities while explicitly reporting `tx=disabled`;
- never configures or transmits through the SX1262.

GPS, OLED, and LoRa receive drivers will be advertised only as each one lands
behind the stable protocol surface in a small, independently recoverable
increment.

## Build

Install the Espressif Rust tools once, then:

```sh
. "$HOME/export-esp.sh"
cargo check
cargo build --release
espflash save-image --chip esp32s3 --flash-size 8mb --flash-mode qio \
  --flash-freq 80mhz --merge --skip-padding \
  target/xtensa-esp32s3-none-elf/release/radioman-node radioman-node.bin
```

## Recovery and install

Before replacing a board image, read and checksum all 8 MiB of flash. The
known-good backup for board `48:ca:43:5b:03:74` is retained on radioman-pi under
`~/radioman-backups/`.

Install from radioman-pi with `esptool`. The Debian package currently lacks its
ESP32-S3 RAM stub, so pass `--no-stub`. Unplug/replug the board if a damaged RST
button prevents an automatic reset.

```sh
sudo esptool --chip esp32s3 --port /dev/ttyACM0 --no-stub \
  write_flash 0x0 radioman-node.bin
```

Restore the preserved image with the same command and backup file. Do not
enable LoRa transmit until the antenna, regional band, and host-side lease
guard are all known and enforced.
