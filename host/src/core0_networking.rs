use alloc::format;
use core::sync::atomic::Ordering;
use cyw43::aligned_bytes;
use cyw43_pio::PioSpi;
use defmt::{info, unwrap, warn};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{Config, StackResources};
use embassy_rp::flash::{Async, Flash};
use embassy_rp::gpio::Output;
use embassy_rp::peripherals::{FLASH, PIO0};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use embedded_storage_async::nor_flash::NorFlash;
use static_cell::StaticCell;

use crate::{WASM_LEN, WASM_PTR};

pub const CONFIG_OFFSET: u32 = 0x17F000;
pub const CONFIG_FLASH_PTR: *const u8 = 0x1017F000 as *const u8;
pub const WASM_OFFSET: u32 = 0x180000;
pub const WASM_FLASH_PTR: *const u8 = 0x10180000 as *const u8;

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

/// Holds the safely extracted components of our XIP Flash URI
struct ParsedUri<'a> {
    ip: embassy_net::Ipv4Address,
    port: u16,
    ip_str: &'a str,
    path: &'a str,
}

/// Reads the raw bytes and returns a clean struct or a descriptive error
fn parse_flash_uri(buf: &[u8]) -> Result<ParsedUri<'_>, &'static str> {
    let uri_len = buf.iter().position(|&b| b == 0 || b == 0xFF).unwrap_or(0);
    if uri_len == 0 {
        return Err("No valid URI found in flash.");
    }

    let uri =
        core::str::from_utf8(&buf[..uri_len]).map_err(|_| "Flash URI contains invalid UTF-8")?;

    info!("Found URI in Flash: {}", uri);

    let (ip_port, path) = uri
        .split_once('/')
        .ok_or("URI is missing the '/' separating host and path")?;

    let (ip_str, port_str) = ip_port.split_once(':').unwrap_or((ip_port, "80"));

    // Utilizing FromStr for clean, idiomatic parsing
    let ip: embassy_net::Ipv4Address = ip_str.parse().map_err(|_| "Invalid IPv4 address format")?;

    let port: u16 = port_str.parse().map_err(|_| "Bad port number")?;

    Ok(ParsedUri {
        ip,
        port,
        ip_str,
        path,
    })
}

pub async fn run_core0(
    spawner: Spawner,
    pwr: Output<'static>,
    spi: PioSpi<'static, PIO0, 0>,
    mut flash: Flash<'static, FLASH, Async, { 2 * 1024 * 1024 }>,
) -> ! {
    let fw = aligned_bytes!("43439A0.bin");
    let clm = aligned_bytes!("43439A0_clm.bin");
    let nvram = aligned_bytes!("nvram_rp2040.bin");

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let net_config = Config::dhcpv4(Default::default());
    static RESOURCES: StaticCell<StackResources<2>> = StaticCell::new();
    let (stack, net_runner) = embassy_net::new(
        net_device,
        net_config,
        RESOURCES.init(StackResources::<2>::new()),
        0x0123_4567_89ab_cdef,
    );
    spawner.spawn(unwrap!(net_task(net_runner)));

    info!("Joining WiFi network...");
    loop {
        if control
            .join(
                env!("WIFI_SSID"),
                cyw43::JoinOptions::new(env!("WIFI_PASS").as_bytes()),
            )
            .await
            .is_ok()
        {
            break;
        }
        Timer::after(Duration::from_secs(1)).await;
    }
    stack.wait_config_up().await;
    info!(
        "Network up! Assigned IP: {}",
        stack.config_v4().unwrap().address.address()
    );

    let mut uri_buf = [0u8; 256];
    let mut ota_success = false;

    // Read directly from XIP memory
    unsafe {
        core::ptr::copy_nonoverlapping(CONFIG_FLASH_PTR, uri_buf.as_mut_ptr(), 256);
    }

    match parse_flash_uri(&uri_buf) {
        Ok(parsed) => {
            info!("Connecting to IP: {}, Port: {}...", parsed.ip, parsed.port);
            let mut rx_buffer = [0; 2048];
            let mut tx_buffer = [0; 2048];
            let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
            socket.set_timeout(Some(Duration::from_secs(10)));

            if socket
                .connect(embassy_net::IpEndpoint::new(parsed.ip.into(), parsed.port))
                .await
                .is_ok()
            {
                let request = format!(
                    "GET /{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    parsed.path, parsed.ip_str
                );
                socket.write_all(request.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();

                let mut current_offset = WASM_OFFSET;
                let mut total_bytes_written = 0;
                let mut page_buf = [0u8; 4096];
                let mut page_idx = 0;
                let mut header_buf = [0u8; 1024];
                let mut header_len = 0;
                let mut body_start = 0;

                loop {
                    if let Ok(n) = socket.read(&mut header_buf[header_len..]).await {
                        if n == 0 {
                            break;
                        }
                        header_len += n;
                        if let Some(pos) = header_buf[..header_len]
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                        {
                            body_start = pos + 4;
                            break;
                        }
                    } else {
                        break;
                    }
                }

                if header_buf[..header_len].windows(6).any(|w| w == b"200 OK") {
                    for &byte in &header_buf[body_start..header_len] {
                        page_buf[page_idx] = byte;
                        page_idx += 1;
                    }

                    let mut temp_rx_buf = [0u8; 1024];
                    loop {
                        match socket.read(&mut temp_rx_buf).await {
                            Ok(0) => break,
                            Ok(n) => {
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
                                        page_idx = 0;
                                        page_buf.fill(0);
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }

                    if page_idx > 0 {
                        flash
                            .erase(current_offset, current_offset + 4096)
                            .await
                            .unwrap();
                        flash.write(current_offset, &page_buf).await.unwrap();
                        total_bytes_written += page_idx;
                    }

                    info!(
                        "OTA Download complete! Total bytes: {}",
                        total_bytes_written
                    );
                    socket.close();

                    ota_success = true;
                    WASM_PTR.store(WASM_FLASH_PTR as usize, Ordering::Release);
                    WASM_LEN.store(total_bytes_written, Ordering::Release);
                    cortex_m::asm::sev();
                } else {
                    warn!("Error: Server did not return 200 OK.");
                }
            } else {
                warn!("Failed to connect to the Wasm URI Server.");
            }
        }
        Err(e) => info!("{}", e),
    }

    if !ota_success {
        info!("OTA failed or skipped. Booting the embedded fallback Wasm binary...");
        let fallback_wasm = include_bytes!("guest.pulley");

        WASM_PTR.store(fallback_wasm.as_ptr() as usize, Ordering::Release);
        WASM_LEN.store(fallback_wasm.len(), Ordering::Release);
        cortex_m::asm::sev();
    }

    let mut rx_meta = [PacketMetadata::EMPTY; 3];
    let mut rx_buf = [0; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 3];
    let mut tx_buf = [0; 1024];
    let mut udp_socket =
        UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);

    udp_socket.bind(8080).unwrap();
    info!("Core 0: Listening for URI updates on UDP port 8080...");

    loop {
        let mut buf = [0u8; 256];
        if let Ok((n, _)) = udp_socket.recv_from(&mut buf).await {
            if let Ok(uri_str) = core::str::from_utf8(&buf[..n]) {
                let clean_uri = uri_str.trim();
                info!(
                    "Received new URI via UDP: '{}'. Saving and Rebooting...",
                    clean_uri
                );

                let mut page_buf = [0u8; 4096];
                page_buf[..clean_uri.len()].copy_from_slice(clean_uri.as_bytes());

                flash
                    .erase(CONFIG_OFFSET, CONFIG_OFFSET + 4096)
                    .await
                    .unwrap();
                flash.write(CONFIG_OFFSET, &page_buf).await.unwrap();

                cortex_m::peripheral::SCB::sys_reset();
            }
        }
    }
}
