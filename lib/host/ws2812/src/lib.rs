#![no_std]
extern crate alloc;

use alloc::vec::Vec;
use embassy_rp::pio_programs::ws2812::{PioWs2812, RgbColorOrder};
use smart_leds::RGB8;
use wasmtime::component::{Linker, Resource, ResourceTable};

wasmtime::component::bindgen!({
    path: "../../../wit/ws2812.wit",
    world: "host-ws2812",
    with: { "local:leds/ws2812.led-strip": ActiveStrip }
});

use local::leds::ws2812::{Color, Host, HostLedStrip};

pub struct ActiveStrip;

pub trait LedDriver {
    fn write_colors(&mut self, colors: &[RGB8]);
}

// --- The Hardware Implementation (Now Encapsulated in the Library) ---
pub struct EmbassyWs2812Driver<
    'd,
    P: embassy_rp::pio::Instance,
    const S: usize,
    const N: usize,
    ORDER: RgbColorOrder,
> {
    #[allow(dead_code)] // Keep driver alive to ensure PIO state machine isn't dropped
    _driver: PioWs2812<'d, P, S, N, ORDER>,
}

impl<'d, P: embassy_rp::pio::Instance, const S: usize, const N: usize, ORDER: RgbColorOrder>
    EmbassyWs2812Driver<'d, P, S, N, ORDER>
{
    pub fn new(driver: PioWs2812<'d, P, S, N, ORDER>) -> Self {
        Self { _driver: driver }
    }
}

impl<'d, P: embassy_rp::pio::Instance, const S: usize, const N: usize, ORDER: RgbColorOrder>
    LedDriver for EmbassyWs2812Driver<'d, P, S, N, ORDER>
{
    fn write_colors(&mut self, colors: &[RGB8]) {
        // Hardcoded hardware addresses for RP2350 (and RP2040) PIO0
        const PIO0_BASE: u32 = 0x5020_0000;
        const PIO0_FSTAT: *const u32 = (PIO0_BASE + 0x04) as *const u32;

        let txf_offset = 0x10 + (S as u32 * 4);
        let txf_addr = (PIO0_BASE + txf_offset) as *mut u32;
        let txfull_bit = 1_u32 << (16 + S as u32);

        // Iterate over the slice directly - no try_into() needed!
        for color in colors {
            let r = color.r as u32;
            let g = color.g as u32;
            let b = color.b as u32;
            let word = (g << 24) | (r << 16) | (b << 8); // Standard GRB format

            unsafe {
                while core::ptr::read_volatile(PIO0_FSTAT) & txfull_bit != 0 {}
                core::ptr::write_volatile(txf_addr, word);
            }
        }

        embassy_time::block_for(embassy_time::Duration::from_micros(55));
    }
}

// --- Wasmtime Context ---
pub struct Ws2812Ctx<D> {
    pub table: ResourceTable,
    pub driver: D,
}

pub trait Ws2812View<D> {
    fn ws2812_ctx(&mut self) -> &mut Ws2812Ctx<D>;
}

impl<D: LedDriver + Send + 'static> Host for Ws2812Ctx<D> {}

impl<D: LedDriver + Send + 'static> HostLedStrip for Ws2812Ctx<D> {
    fn write(&mut self, rep: Resource<ActiveStrip>, colors: Vec<Color>) {
        let _ = self
            .table
            .get(&rep)
            .expect("Guest passed an invalid resource handle");

        const NUM_LEDS: usize = 100;
        let mut fixed_array = [RGB8::default(); NUM_LEDS];
        for (i, c) in colors.into_iter().enumerate().take(NUM_LEDS) {
            fixed_array[i] = RGB8::new(c.r, c.g, c.b);
        }

        self.driver.write_colors(&fixed_array);
    }

    fn drop(&mut self, rep: Resource<ActiveStrip>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

pub fn add_to_linker<T: Ws2812View<D> + 'static, D: LedDriver + Send + 'static>(
    linker: &mut Linker<T>,
) -> wasmtime::Result<()> {
    local::leds::ws2812::add_to_linker::<T, wasmtime::component::HasSelf<Ws2812Ctx<D>>>(
        linker,
        |host| host.ws2812_ctx(),
    )
}
