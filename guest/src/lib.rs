#![no_std]
extern crate alloc;
use alloc::vec;

wit_bindgen::generate!({
    path: "wit",
    world: "app",
    generate_all
});

use local::delay::delay::delay_ms;
use local::leds::ws2812::Color;

struct MainApp;

impl Guest for MainApp {
    fn run(strip: &LedStrip) {
        let led_amount = strip.get_led_amount() as usize;

        let red = Color { r: 50, g: 0, b: 0 };
        let white = Color {
            r: 50,
            g: 50,
            b: 50,
        };

        let mut colors = vec![white; led_amount];

        loop {
            // front to back loop
            for i in 0..led_amount {
                colors[i] = red;
                strip.write(&colors);
                delay_ms(50);
                colors[i] = white;
            }

            // back to front loop
            for i in (0..led_amount).rev() {
                colors[i] = red;
                strip.write(&colors);
                delay_ms(50);
                colors[i] = white;
            }
        }
    }
}

export!(MainApp);
