#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embassy_time::Timer;
use smart_leds::RGB8;
use {defmt_rtt as _, panic_probe as _};

// Program metadata for `picotool info`.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"WS2812 Cylon Example"),
    embassy_rp::binary_info::rp_program_description!(
        c"White background with a red dot bouncing back and forth"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

// Bind PIO interrupts for the WS2812 driver
bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
});

// Update this to match the number of LEDs on your strip
const NUM_LEDS: usize = 100;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!("Starting WS2812 Animation!");

    // Set up the PIO state machine and DMA for the WS2812 driver.
    // Change `p.PIN_16` below if your data wire is connected to a different GPIO pin.
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);
    let program = PioWs2812Program::new(&mut common);
    let mut ws2812 = PioWs2812::new(&mut common, sm0, p.DMA_CH0, p.PIN_16, &program);

    // 20% brightness calculation (255 * 0.20 ≈ 51)
    const WHITE: RGB8 = RGB8::new(100, 0, 0);
    const RED: RGB8 = RGB8::new(5, 5, 5);

    let mut position: i32 = 0;
    let mut moving_forward = true;

    loop {
        // Initialize the entire strip to white
        let mut leds = [WHITE; NUM_LEDS];

        // Overwrite the current position with the red dot
        leds[position as usize] = RED;

        // Push the array to the LED strip via PIO and DMA
        ws2812.write(&leds).await;

        // Update the dot's position for the next frame
        if moving_forward {
            position += 1;
            if position >= (NUM_LEDS as i32) - 1 {
                moving_forward = false;
            }
        } else {
            position -= 1;
            if position <= 0 {
                moving_forward = true;
            }
        }

        // 50ms delay dictates the speed of the moving dot
        Timer::after_millis(25).await;
    }
}
