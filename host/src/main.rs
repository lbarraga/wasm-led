#![no_std]
#![no_main]

extern crate alloc;

use cyw43::aligned_bytes;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use defmt::{info, unwrap};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::dma;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, PIO0};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embassy_time::{Duration, Timer};
use embedded_alloc::LlffHeap as Heap;
use embedded_io_async::Write; // Required for socket.write_all()
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};

use host_delay::{DelayCtx, DelayView};
use host_ws2812::{EmbassyWs2812Driver, Ws2812Ctx, Ws2812View};

wasmtime::component::bindgen!({
    path: "../guest/wit",
    world: "app",
    with: { "local:leds/ws2812.led-strip": host_ws2812::ActiveStrip }
});

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => PioInterruptHandler<PIO0>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>, dma::InterruptHandler<DMA_CH2>;
});

const HEAP_SIZE: usize = 350 * 1024;
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

// ------------------------------------------------------------------------
// BACKGROUND TASKS FOR WIFI & NETWORK
// ------------------------------------------------------------------------

#[embassy_executor::task]
async fn cyw43_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

// ------------------------------------------------------------------------
// MAIN ENTRY
// ------------------------------------------------------------------------

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // 1. Initialize the CYW43 Firmware (Paths updated to match your tree)
    let fw = aligned_bytes!("43439A0.bin");
    let clm = aligned_bytes!("43439A0_clm.bin");
    let nvram = aligned_bytes!("nvram_rp2040.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);

    // Share PIO0: sm0 for WiFi, sm1 for WS2812
    let mut pio = Pio::new(p.PIO0, Irqs);

    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        RM2_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;

    // Correctly unwrap the task token, NOT the spawn result
    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // 2. Initialize the Network Stack
    let config = Config::dhcpv4(Default::default());
    let seed = 0x0123_4567_89ab_cdef; // Consider replacing with true RNG later

    static RESOURCES: StaticCell<StackResources<2>> = StaticCell::new();
    let (stack, net_runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<2>::new()),
        seed,
    );
    spawner.spawn(unwrap!(net_task(net_runner)));

    // 3. Connect to WiFi
    info!("Joining WiFi network...");

    let ssid = env!("WIFI_SSID");
    let pass = env!("WIFI_PASS");

    loop {
        // Use the new join() API with JoinOptions
        match control
            .join(ssid, cyw43::JoinOptions::new(pass.as_bytes()))
            .await
        {
            Ok(_) => break,
            Err(_) => {
                info!("Join failed. Retrying in 1s...");
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    }
    info!("WiFi joined! Waiting for DHCP lease...");

    // Wait until we have an IP address
    stack.wait_config_up().await;
    let ip = stack.config_v4().unwrap().address.address();
    info!("Network up! Assigned IP: {}", ip);

    // 4. Perform the simple HTTP Ping
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(10)));

    let target_ip = embassy_net::Ipv4Address::new(192, 168, 0, 213);
    let target_port = 8080;

    info!("Connecting to PC...");
    if let Ok(_) = socket
        .connect(embassy_net::IpEndpoint::new(target_ip.into(), target_port))
        .await
    {
        info!("Connected! Sending ping...");
        let request = b"GET / HTTP/1.1\r\nHost: pico2w\r\nConnection: close\r\n\r\n";
        if let Ok(_) = socket.write_all(request).await {
            socket.flush().await.ok();
            info!("Ping successfully sent.");
        }
    } else {
        info!("Failed to connect to PC. Moving on to Wasmtime...");
    }

    socket.close();

    // 5. WASMTIME INITIALIZATION
    {
        use core::mem::MaybeUninit;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }
    }
    info!("Heap initialized for Wasmtime.");

    let program = PioWs2812Program::new(&mut pio.common);

    // Note: WS2812 now uses sm1 and DMA_CH2 so it doesn't conflict with WiFi
    let ws2812: PioWs2812<'_, _, 1, 100, _> = PioWs2812::new(
        &mut pio.common,
        pio.sm1,
        p.DMA_CH2,
        Irqs,
        p.PIN_16,
        &program,
    );

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

    let guest_bytes = include_bytes!("guest.pulley");
    let component = unsafe { Component::deserialize(&engine, guest_bytes) }.unwrap();

    info!("Instantiating Component...");
    let app = App::instantiate(&mut store, &component, &linker).unwrap();

    info!("Starting guest execution...");
    app.call_run(&mut store, strip_resource).unwrap();
    info!("Guest finished.");
}
