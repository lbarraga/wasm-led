#![no_std]
extern crate alloc;

use alloc::vec::Vec;

wit_bindgen::generate!({
    path: "wit",
    world: "app",
    generate_all
});

use local::leds::ws2812::Color;
// --> Import the generated guest delay function
use local::delay::delay::delay_ms;

struct MainApp;

impl Guest for MainApp {
    fn run(strip: &LedStrip) {
        let green = Color { r: 0, g: 50, b: 0 };
        let off = Color { r: 0, g: 0, b: 0 };

        let mut greens = Vec::with_capacity(100);
        let mut offs = Vec::with_capacity(100);

        for _ in 0..100 {
            greens.push(green);
            offs.push(off);
        }

        // Blink 5 times
        loop {
            strip.write(&greens);
            delay_ms(500); // 500ms delay
            strip.write(&offs);
            delay_ms(500); // 500ms delay
        }
    }
}

export!(MainApp);
