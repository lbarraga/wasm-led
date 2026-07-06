#![no_std]

use wasmtime::component::Linker;

wasmtime::component::bindgen!({
    path: "../../../wit/delay.wit",
    world: "host-delay",
});

use local::delay::delay::Host;

pub struct DelayCtx;

pub trait DelayView {
    fn delay_ctx(&mut self) -> &mut DelayCtx;
}

// The actual hardware implementation
impl Host for DelayCtx {
    fn delay_ms(&mut self, ms: u32) {
        embassy_time::block_for(embassy_time::Duration::from_millis(ms as u64));
    }
}

pub fn add_to_linker<T: DelayView + 'static>(linker: &mut Linker<T>) -> wasmtime::Result<()> {
    local::delay::delay::add_to_linker::<T, wasmtime::component::HasSelf<DelayCtx>>(
        linker,
        |host| host.delay_ctx(),
    )
}
