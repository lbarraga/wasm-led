use core::sync::atomic::Ordering;
use defmt::info;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};

use host_delay::{DelayCtx, DelayView};
use host_ws2812::{LedDriver, Ws2812Ctx, Ws2812View};

use crate::{WASM_LEN, WASM_PTR};

wasmtime::component::bindgen!({
    path: "../guest/wit",
    world: "app",
    with: { "local:leds/ws2812.led-strip": host_ws2812::ActiveStrip }
});

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

pub fn run_wasm_worker<D: LedDriver + Send + 'static>(driver: D) -> ! {
    info!("Core 1: Waiting for Wasm binary to be ready...");

    let mut total_bytes = WASM_LEN.load(Ordering::Acquire);
    while total_bytes == 0 {
        cortex_m::asm::wfe(); // Sleep until Core 0 triggers SEV
        total_bytes = WASM_LEN.load(Ordering::Acquire);
    }

    let wasm_ptr = WASM_PTR.load(Ordering::Acquire) as *const u8;

    let mut table = ResourceTable::new();
    let strip_resource = table.push(host_ws2812::ActiveStrip).unwrap();

    let host_state = HostState {
        ws2812_ctx: Ws2812Ctx { table, driver },
        delay_ctx: DelayCtx,
    };

    let mut config = wasmtime::Config::new();
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

    host_ws2812::add_to_linker(&mut linker).unwrap();
    host_delay::add_to_linker(&mut linker).unwrap();

    info!("Core 1: Executing Wasm Component directly from memory (XIP)...");
    let guest_bytes = unsafe { core::slice::from_raw_parts(wasm_ptr, total_bytes) };

    let component = match unsafe { Component::deserialize(&engine, guest_bytes) } {
        Ok(c) => c,
        Err(_) => {
            info!("Core 1: Validation Error: The memory block is not a valid Wasmtime Component.");
            loop {
                cortex_m::asm::wfi();
            }
        }
    };

    let app = App::instantiate(&mut store, &component, &linker).unwrap();
    info!("Core 1: Starting guest execution...");
    app.call_run(&mut store, strip_resource).unwrap();

    info!("Core 1: Guest finished.");
    loop {
        cortex_m::asm::wfi();
    }
}
