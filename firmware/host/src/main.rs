#![no_std]
#![no_main]

extern crate alloc;

pub mod core0_networking;
pub mod core1_wasm;

use core::sync::atomic::AtomicUsize;
use defmt::info;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::dma;
use embassy_rp::flash::{Async, Flash};
use embassy_rp::multicore::{Stack, spawn_core1};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH2, DMA_CH3, PIO0};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embedded_alloc::LlffHeap as Heap;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use host_ws2812::EmbassyWs2812Driver;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>,
                 dma::InterruptHandler<DMA_CH2>,
                 dma::InterruptHandler<DMA_CH3>;
});

const HEAP_SIZE: usize = 350 * 1024;
#[global_allocator]
static HEAP: Heap = Heap::empty();

// Global Sync Atomics for Multicore Communication
pub static WASM_PTR: AtomicUsize = AtomicUsize::new(0);
pub static WASM_LEN: AtomicUsize = AtomicUsize::new(0);
static CORE1_STACK: StaticCell<Stack<32768>> = StaticCell::new(); // 32KB Stack

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // 1. Initialize the Global Heap FIRST
    {
        use core::mem::MaybeUninit;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }
    }
    info!("Global Heap initialized.");

    let p = embassy_rp::init(Default::default());

    // 2. Hardware Split: Prepare WS2812 for Core 1
    let mut pio = Pio::new(p.PIO0, Irqs);

    static PROGRAM_CELL: StaticCell<PioWs2812Program<'static, PIO0>> = StaticCell::new();
    let program = PROGRAM_CELL.init(PioWs2812Program::new(&mut pio.common));

    let ws2812: PioWs2812<'static, _, 1, 100, _> =
        PioWs2812::new(&mut pio.common, pio.sm1, p.DMA_CH2, Irqs, p.PIN_16, program);
    let encapsulated_driver = EmbassyWs2812Driver::new(ws2812);

    // 3. Spawn Core 1 (The Wasm Worker)
    let core1_stack = CORE1_STACK.init(Stack::new());
    spawn_core1(p.CORE1, core1_stack, move || {
        core1_wasm::run_wasm_worker(encapsulated_driver);
    });

    // 4. Set up hardware needed for Core 0 Networking
    let pwr = embassy_rp::gpio::Output::new(p.PIN_23, embassy_rp::gpio::Level::Low);
    let cs = embassy_rp::gpio::Output::new(p.PIN_25, embassy_rp::gpio::Level::High);

    let spi = cyw43_pio::PioSpi::new(
        &mut pio.common,
        pio.sm0,
        cyw43_pio::RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    let flash = Flash::<_, Async, { 2 * 1024 * 1024 }>::new(p.FLASH, p.DMA_CH3, Irqs);

    // 5. Hand over to Core 0 (Wi-Fi, OTA, and UDP)
    core0_networking::run_core0(spawner, pwr, spi, flash).await;
}
