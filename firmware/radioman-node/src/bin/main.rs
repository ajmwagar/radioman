#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::efuse;
use esp_hal::timer::timg::TimerGroup;
use radioman_node::{BOARD, CAPABILITIES, FIRMWARE_VERSION, STATUS_PROTOCOL_VERSION};

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.4.0
    // generator parameters: -o esp32s3 -o unstable-hal -o embassy -o log -o esp-backtrace --toolchain esp

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let mac = efuse::base_mac_address();
    esp_println::println!(
        "radioman.status v={} kind=boot board={} firmware={} id={} capabilities={} tx=disabled",
        STATUS_PROTOCOL_VERSION,
        BOARD,
        FIRMWARE_VERSION,
        mac,
        CAPABILITIES,
    );
    // TODO: Spawn some tasks
    let _ = spawner;

    loop {
        esp_println::println!(
            "radioman.status v={} kind=health state=ready tx=disabled",
            STATUS_PROTOCOL_VERSION
        );
        Timer::after(Duration::from_secs(1)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.2.2/examples
}
