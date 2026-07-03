#![no_std]
#![no_main]

extern crate alloc;

use defmt::info;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embedded_alloc::Heap;
use {defmt_rtt as _, panic_probe as _};

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};

use host_delay::{DelayCtx, DelayView};
use host_ws2812::{EmbassyWs2812Driver, Ws2812Ctx, Ws2812View};

wasmtime::component::bindgen!({
    path: "../guest/wit",
    world: "app",
    with: { "local:leds/ws2812.led-strip": host_ws2812::ActiveStrip }
});

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<embassy_rp::peripherals::DMA_CH0>;
});

const HEAP_SIZE: usize = 470 * 1024;
#[global_allocator]
static HEAP: Heap = Heap::empty();

static mut TLS_PTR: *mut u8 = core::ptr::null_mut();
#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_tls_get() -> *mut u8 {
    unsafe { TLS_PTR }
}
#[unsafe(no_mangle)]
pub extern "C" fn wasmtime_tls_set(ptr: *mut u8) {
    unsafe {
        TLS_PTR = ptr;
    }
}

pub struct HostState<D> {
    pub ws2812_ctx: Ws2812Ctx<D>,
    pub delay_ctx: DelayCtx,
}

impl<D> Ws2812View<D> for HostState<D> {
    fn ws2812_ctx(&mut self) -> &mut Ws2812Ctx<D> {
        &mut self.ws2812_ctx
    }
}

impl<D> DelayView for HostState<D> {
    fn delay_ctx(&mut self) -> &mut DelayCtx {
        &mut self.delay_ctx
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    {
        use core::mem::MaybeUninit;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }
    }
    info!("Heap initialized.");

    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);
    let program = PioWs2812Program::new(&mut common);

    let ws2812: PioWs2812<'_, _, 0, 100, _> =
        PioWs2812::new(&mut common, sm0, p.DMA_CH0, Irqs, p.PIN_16, &program);

    // Give the raw Embassy driver to the library to encapsulate it
    let encapsulated_driver = EmbassyWs2812Driver::new(ws2812);

    let mut table = ResourceTable::new();
    let strip_resource = table.push(host_ws2812::ActiveStrip).unwrap();

    let host_state = HostState {
        ws2812_ctx: Ws2812Ctx {
            table,
            driver: encapsulated_driver,
        },
        delay_ctx: DelayCtx,
    };

    let mut config = Config::new();
    config.target("pulley32").unwrap();
    config.wasm_component_model(true);
    config.gc_support(false);
    config.signals_based_traps(false);
    config.memory_init_cow(false);
    config.memory_guard_size(0);
    config.memory_reservation(0);
    config.max_wasm_stack(16 * 1024);
    config.memory_reservation_for_growth(0);

    let engine = Engine::new(&config).expect("Engine failed");
    let mut store = Store::new(&engine, host_state);
    let mut linker = Linker::new(&engine);

    // Hook our component bindings into the Wasmtime linker
    host_ws2812::add_to_linker(&mut linker).unwrap();
    host_delay::add_to_linker(&mut linker).unwrap();

    let guest_bytes = include_bytes!("guest.pulley");
    let component = unsafe { Component::deserialize(&engine, guest_bytes) }.unwrap();

    info!("Instantiating Component...");
    let app = App::instantiate(&mut store, &component, &linker).unwrap();

    info!("Starting guest execution...");
    app.call_run(&mut store, strip_resource).unwrap();
    info!("Guest finished.");
}
