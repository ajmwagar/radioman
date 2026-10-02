#![no_std]

/// Version of the line-oriented USB status contract.
pub const STATUS_PROTOCOL_VERSION: u8 = 1;

/// Board model this firmware is built for.
pub const BOARD: &str = "lilygo-t-beam-supreme";

/// Firmware release identifier reported to the host.
pub const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The first slice is deliberately receive-only. The host must not infer a
/// transmit capability merely because the board contains an SX1262.
pub const CAPABILITIES: &str = "health";
