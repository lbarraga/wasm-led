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
use embassy_rp::flash::{Async, Flash};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH2, DMA_CH3, PIO0};
use embassy_rp::pio::{InterruptHandler as PioInterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embassy_time::{Duration, Timer};
use embedded_alloc::LlffHeap as Heap;
use embedded_io_async::Write; // Required for socket.write_all()
use embedded_storage_async::nor_flash::NorFlash;
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
    // WiFi (CH0), WS2812 (CH2), Flash (CH3)
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>,
                 dma::InterruptHandler<DMA_CH2>,
                 dma::InterruptHandler<DMA_CH3>;
});

// Optimized heap size to leave room for Wasmtime's system stack
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

    // 1. Initialize the CYW43 Firmware
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

    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    // 2. Initialize the Network Stack
    let config = Config::dhcpv4(Default::default());
    let seed = 0x0123_4567_89ab_cdef;

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

    stack.wait_config_up().await;
    let ip = stack.config_v4().unwrap().address.address();
    info!("Network up! Assigned IP: {}", ip);

    // 4. Download and Flash the WASM Component (OTA)
    info!("Connecting to PC to download Wasm component...");

    let mut rx_buffer = [0; 2048];
    let mut tx_buffer = [0; 2048];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(Duration::from_secs(10)));

    let target_ip = embassy_net::Ipv4Address::new(192, 168, 0, 132);
    let target_port = 8080;

    info!("Attempting to connect to {}:{}...", target_ip, target_port);

    if let Err(e) = socket
        .connect(embassy_net::IpEndpoint::new(target_ip.into(), target_port))
        .await
    {
        info!(
            "Connection failed: {:?}. Is the server running and firewall open?",
            e
        );
        socket.close();
        loop {
            Timer::after(Duration::from_secs(1)).await;
        }
    }

    info!("Connected! Sending HTTP GET...");
    let request = b"GET /guest.pulley HTTP/1.1\r\nHost: 192.168.0.213\r\nConnection: close\r\n\r\n";

    if let Err(e) = socket.write_all(request).await {
        info!("Failed to write request: {:?}", e);
        socket.close();
        loop {
            Timer::after(Duration::from_secs(1)).await;
        }
    }

    if let Err(e) = socket.flush().await {
        info!("Failed to flush socket: {:?}", e);
        socket.close();
        loop {
            Timer::after(Duration::from_secs(1)).await;
        }
    }

    // Initialize Flash peripheral mapping memory to 0x180000 (0x10180000 absolute)
    let flash_base_offset: u32 = 0x180000;
    let mut flash = Flash::<_, Async, { 2 * 1024 * 1024 }>::new(p.FLASH, p.DMA_CH3, Irqs);
    let mut current_offset = flash_base_offset;
    let mut total_bytes_written = 0;

    let mut page_buf = [0u8; 4096];
    let mut page_idx = 0;

    let mut header_buf = [0u8; 1024];
    let mut header_len = 0;
    let mut body_start = 0;

    // Extract headers
    loop {
        let n = socket.read(&mut header_buf[header_len..]).await.unwrap();
        if n == 0 {
            break;
        }
        header_len += n;

        // Look for double CRLF which indicates the end of HTTP headers
        if let Some(pos) = header_buf[..header_len]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            body_start = pos + 4;
            break;
        }
    }

    // VALIDATION 1: Check for HTTP 200 OK
    if !header_buf[..header_len].windows(6).any(|w| w == b"200 OK") {
        info!("Error: Server did not return a valid file (Not 200 OK). Aborting OTA.");
        socket.close();
        // Halt gracefully
        loop {
            Timer::after(Duration::from_secs(1)).await;
        }
    }

    info!("Headers skipped. Streaming Wasm binary to flash...");

    // Copy any body bytes that were pulled in with the header read
    for &byte in &header_buf[body_start..header_len] {
        page_buf[page_idx] = byte;
        page_idx += 1;
    }

    // Stream the actual WASM body from the socket to Flash
    let mut temp_rx_buf = [0u8; 1024];
    loop {
        let n = match socket.read(&mut temp_rx_buf).await {
            Ok(0) => break, // EOF reached
            Ok(n) => n,
            Err(_) => {
                info!("Network read error.");
                break;
            }
        };

        for &byte in &temp_rx_buf[..n] {
            page_buf[page_idx] = byte;
            page_idx += 1;

            if page_idx == 4096 {
                flash
                    .erase(current_offset, current_offset + 4096)
                    .await
                    .unwrap();
                flash.write(current_offset, &page_buf).await.unwrap();
                current_offset += 4096;
                total_bytes_written += 4096;
                info!("Flashed {} bytes...", total_bytes_written);

                page_idx = 0;
                page_buf.fill(0);
            }
        }
    }

    // Flush any remaining data to the final block
    if page_idx > 0 {
        flash
            .erase(current_offset, current_offset + 4096)
            .await
            .unwrap();
        flash.write(current_offset, &page_buf).await.unwrap();
        total_bytes_written += page_idx; // Track true Wasm binary size, not padded size
        info!("Flashed final block. Total bytes: {}", total_bytes_written);
    }

    socket.close();
    info!("WASM Download complete!");

    // 5. WASMTIME INITIALIZATION
    {
        use core::mem::MaybeUninit;
        static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
        unsafe { HEAP.init(core::ptr::addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) }
    }
    info!("Heap initialized for Wasmtime.");

    let program = PioWs2812Program::new(&mut pio.common);

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

    info!("Executing Wasm Component directly from Flash (XIP)...");

    // Create a byte slice pointing directly to the written flash sector
    let wasm_flash_ptr = 0x10180000 as *const u8;
    let guest_bytes = unsafe { core::slice::from_raw_parts(wasm_flash_ptr, total_bytes_written) };

    // VALIDATION 2: Safely handle deserialization failures
    let component = match unsafe { Component::deserialize(&engine, guest_bytes) } {
        Ok(c) => c,
        Err(_) => {
            info!("Validation Error: The downloaded file is not a valid Wasmtime Component.");
            // Halt gracefully
            loop {
                Timer::after(Duration::from_secs(1)).await;
            }
        }
    };

    info!("Instantiating Component...");
    let app = App::instantiate(&mut store, &component, &linker).unwrap();

    info!("Starting guest execution...");
    app.call_run(&mut store, strip_resource).unwrap();
    info!("Guest finished.");
}
