#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use core::str::FromStr;
use defmt::{info, warn};
use edge_http::Method;
use edge_http::io::Error;
use edge_http::io::server::DefaultServer;
use edge_nal::TcpBind;
use embassy_executor::Spawner;
use embassy_net::{Ipv4Cidr, Runner, Stack, StackResources};
use embassy_time::Timer;
use embedded_io_async::Write;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println as _;
use esp_radio::wifi::ap::AccessPointConfig;
use esp_radio::wifi::{self, ControllerConfig, Interface, WifiController};

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const GW_IP_ADDR_ENV: Option<&'static str> = option_env!("GATEWAY_IP");

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.4.0
    // generator parameters: -o esp32 -o unstable-hal -o embassy -o alloc -o wifi -o defmt -o esp-backtrace -o helix

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    info!("Embassy initialized!");

    let access_point_config = wifi::Config::AccessPoint(
        AccessPointConfig::default().with_ssid("esp-radio".try_into().unwrap()),
    );

    info!("Starting wifi");
    let device = esp_radio::wifi::Interface::access_point();
    let controller = esp_radio::wifi::WifiController::new(
        peripherals.WIFI,
        ControllerConfig::default().with_initial_config(access_point_config),
    )
    .unwrap();
    info!("Wifi started");

    let gw_ip_addr_str = GW_IP_ADDR_ENV.unwrap_or("192.168.2.1");
    let gw_ip_addr =
        core::net::Ipv4Addr::from_str(gw_ip_addr_str).expect("failed to parse gatewa ip");

    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(gw_ip_addr, 24),
        gateway: Some(gw_ip_addr),
        dns_servers: Default::default(),
    });

    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | (rng.random() as u64);

    let (stack, runner) = embassy_net::new(
        device,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    spawner.spawn(connection(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(run_dhcp(stack, gw_ip_addr_str).unwrap());

    info!(
        "Connect to the AP 'esp-radio' and point your browser to http://{}:8080/",
        gw_ip_addr_str
    );
    info!("DHCP is enabled so there is no need to configure a static IP, just in case:");

    // Wait for DHCP config
    stack.wait_config_up().await;

    stack.config_v4().inspect(|c| info!("ipv4 config: {:?}", c));

    // Network is ready

    use core::net::{Ipv4Addr, SocketAddrV4};

    use edge_nal_embassy::{Tcp, TcpBuffers};

    let buffers = TcpBuffers::<3, 1024, 1024>::new();
    let socket = Tcp::new(stack, &buffers);

    let acceptor = socket
        .bind(core::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            8080,
        )))
        .await
        .unwrap();

    let mut server = DefaultServer::new();

    loop {
        // TODO: Read more about the Q parameter
        // Currently any number larger than 1 causes panicking
        // And replace run_with_socket_queue with run also causes
        // panicking
        _ = server
            .run_with_socket_queue::<_, _, 1>(None, acceptor, WsHandler)
            .await
            .inspect_err(|_| warn!("HTTP server error"));
        Timer::after_millis(500).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.2.2/examples
}

#[embassy_executor::task]
async fn run_dhcp(stack: Stack<'static>, gw_ip_addr: &'static str) {
    use core::net::Ipv4Addr;

    use edge_dhcp::{
        io::{self, DEFAULT_SERVER_PORT},
        server::{Server, ServerOptions},
    };

    use edge_nal::UdpBind;
    use edge_nal_embassy::{Udp, UdpBuffers};

    let ip = Ipv4Addr::from_str(gw_ip_addr).expect("dhcp task failed to parse gateway ip");

    let mut buf = [0u8; 1500];

    let mut gw_buf = [Ipv4Addr::UNSPECIFIED];

    let buffers = UdpBuffers::<3, 1024, 1024, 10>::new();
    let unbound_socket = Udp::new(stack, &buffers);
    let mut bound_socket = unbound_socket
        .bind(core::net::SocketAddr::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            DEFAULT_SERVER_PORT,
        )))
        .await
        .unwrap();

    loop {
        _ = io::server::run(
            &mut Server::<_, 64>::new_with_et(ip),
            &ServerOptions::new(ip, Some(&mut gw_buf)),
            &mut bound_socket,
            &mut buf,
        )
        .await
        .inspect_err(|_| warn!("DHCP server error"));
        Timer::after_millis(5000).await;
    }
}

#[embassy_executor::task]
async fn connection(controller: WifiController<'static>) {
    use esp_radio::wifi::ap::EventInfo;

    info!("start connection task");
    loop {
        let ev = controller
            .wait_for_access_point_connected_event_async()
            .await;
        match ev {
            Ok(EventInfo::Connected(info)) => {
                info!("Station connected: {:?}", info);
            }
            Ok(EventInfo::Disconnected(info)) => {
                info!("Station disconnected {:?}", info);
            }
            _ => {}
        }
        Timer::after_millis(5000).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) {
    runner.run().await
}

struct WsHandler;

impl edge_http::io::server::Handler for WsHandler {
    type Error<E>
        = Error<E>
    where
        E: core::fmt::Debug;

    async fn handle<T, const N: usize>(
        &self,
        _task_id: impl core::fmt::Display + Copy,
        connection: &mut edge_http::io::server::Connection<'_, T, N>,
    ) -> Result<(), Self::Error<T::Error>>
    where
        T: embedded_io_async::Read + embedded_io_async::Write + edge_nal::TcpSplit,
    {
        let headers = connection.headers()?;

        if headers.method != Method::Get {
            connection
                .initiate_response(405, Some("Method Not Allowed"), &[])
                .await?;
        } else if headers.path == "/" {
            connection
                .initiate_response(200, Some("OK"), &[("Content-Type", "text/html")])
                .await?;
            connection
                .write_all(include_bytes!("../index.html"))
                .await?;
        } else if headers.path == "/button" {
            connection
                .initiate_response(200, Some("OK"), &[("Content-Type", "text/html")])
                .await?;
            connection
                .write_all(
                    b"\
                <p><b>Button Pressed!</b><p>\
                ",
                )
                .await?;
        } else {
            connection
                .initiate_response(404, Some("Not Found"), &[])
                .await?;
        }

        Ok(())
    }
}
