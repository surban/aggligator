//! Aggligator speed test.

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme,
};
use rustls_pemfile::{certs, private_key};
use serde::Serialize;
use std::{
    collections::HashSet,
    io::{stdout, BufReader},
    net::{Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::exit,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::block_in_place,
};

use aggligator::{
    cfg::Cfg,
    dump::dump_to_json_line_file,
    exec,
    transport::{AcceptorBuilder, ConnectorBuilder, LinkTagBox},
};
use aggligator_monitor::{
    monitor::{format_speed, interactive_monitor},
    speed::{speed_test, INTERVAL},
};
use aggligator_transport_tcp::{IpVersion, TcpAcceptor, TcpConnector, TcpLinkFilter};
use aggligator_transport_websocket::{WebSocketAcceptor, WebSocketConnector};
use aggligator_util::{init_log, load_cfg, parse_tcp_link_filter, print_default_cfg};
use aggligator_wrapper_tls::{TlsClient, TlsServer};

#[cfg(feature = "bluer")]
use aggligator_transport_bluer::rfcomm::{RfcommAcceptor, RfcommConnector};
#[cfg(feature = "bluer")]
use aggligator_transport_bluer::rfcomm_profile::{RfcommProfileAcceptor, RfcommProfileConnector};

#[cfg(feature = "usb-device")]
use aggligator_transport_usb::{upc, usb_gadget};

const TCP_PORT: u16 = 5700;
const DUMP_BUFFER: usize = 8192;

const WEBSOCKET_PORT: u16 = 8080;
const WEBSOCKET_PATH: &str = "/agg-speed";

#[cfg(any(feature = "usb-host", feature = "usb-device"))]
mod usb {
    pub const VID: u16 = u16::MAX - 1;
    pub const PID: u16 = u16::MAX - 1;
    pub const MANUFACTURER: &str = env!("CARGO_PKG_NAME");
    pub const PRODUCT: &str = env!("CARGO_BIN_NAME");
    pub const CLASS: u8 = 255;
    pub const SUB_CLASS: u8 = 255;
    pub const PROTOCOL: u8 = 255;
    pub const INTERFACE_CLASS: u8 = 255;
    pub const INTERFACE_SUB_CLASS: u8 = 230;
    pub const INTERFACE_PROTOCOL: u8 = 231;
    pub const INTERFACE_NAME: &str = "speed test";
}

#[cfg(feature = "bluer")]
const RFCOMM_CHANNEL: u8 = 20;
#[cfg(feature = "bluer")]
const RFCOMM_UUID: aggligator_transport_bluer::rfcomm_profile::Uuid =
    aggligator_transport_bluer::rfcomm_profile::Uuid::from_u128(0x7f95058c_c00e_44a9_9003_2ce90d60e2e7);

static TLS_CERT_PEM: &[u8] = include_bytes!("agg-speed-cert.pem");
static TLS_KEY_PEM: &[u8] = include_bytes!("agg-speed-key.pem");
static TLS_SERVER_NAME: &str = "aggligator.rs";

fn tls_cert() -> CertificateDer<'static> {
    let mut reader = BufReader::new(TLS_CERT_PEM);
    let mut certs = certs(&mut reader);
    certs.next().unwrap().unwrap()
}

fn tls_key() -> PrivateKeyDer<'static> {
    let mut reader = BufReader::new(TLS_KEY_PEM);
    private_key(&mut reader).unwrap().unwrap()
}

/// Accepts every TLS server certificate.
///
/// For speed test only! Do not use in production code!
#[derive(Debug)]
struct TlsNullVerifier;

impl ServerCertVerifier for TlsNullVerifier {
    fn verify_server_cert(
        &self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp_response: &[u8], _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

fn tls_client_config() -> ClientConfig {
    let mut root_store = RootCertStore::empty();
    root_store.add(tls_cert()).unwrap();
    let mut cfg = ClientConfig::builder().with_root_certificates(root_store).with_no_client_auth();
    cfg.dangerous().set_certificate_verifier(Arc::new(TlsNullVerifier));
    cfg
}

fn tls_server_config() -> ServerConfig {
    ServerConfig::builder().with_no_client_auth().with_single_cert(vec![tls_cert()], tls_key()).unwrap()
}

fn debug_warning() -> String {
    match cfg!(debug_assertions) {
        true => "⚠ debug build: speeds will be slow ⚠\n".red().to_string(),
        false => String::new(),
    }
}

/// Run speed test using a connection consisting of aggregated TCP links.
///
/// This uses Aggligator to combine multiple TCP links into one connection,
/// providing the combined speed and resilience to individual link faults.
#[derive(Parser)]
#[command(name = "agg-speed", author, version)]
pub struct SpeedCli {
    /// Configuration file.
    #[arg(long)]
    cfg: Option<PathBuf>,
    /// Dump analysis data to file.
    #[arg(long, short = 'd')]
    dump: Option<PathBuf>,
    /// Client or server.
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Raw speed test client.
    Client(ClientCli),
    /// Raw speed test server.
    Server(ServerCli),
    /// Shows the default configuration.
    ShowCfg,
    /// Generate manual pages for this tool in current directory.
    #[command(hide = true)]
    ManPages,
    /// Generate markdown page for this tool.
    #[command(hide = true)]
    Markdown,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_log();

    let cli = SpeedCli::parse();
    let cfg = load_cfg(&cli.cfg)?;
    let dump = cli.dump.clone();

    if cfg!(debug_assertions) {
        eprintln!("{}", debug_warning());
    }

    match cli.command {
        Commands::Client(client) => client.run(cfg, dump).await?,
        Commands::Server(server) => server.run(cfg, dump).await?,
        Commands::ShowCfg => print_default_cfg(),
        Commands::ManPages => clap_mangen::generate_to(SpeedCli::command(), ".")?,
        Commands::Markdown => println!("{}", clap_markdown::help_markdown::<SpeedCli>()),
    }

    tracing::debug!("exiting main");
    Ok(())
}

#[derive(Parser)]
pub struct ClientCli {
    /// Use IPv4.
    #[arg(long, short = '4')]
    ipv4: bool,
    /// Use IPv6.
    #[arg(long, short = '6')]
    ipv6: bool,
    /// Limit test data to specified number of MB.
    #[arg(long, short = 'l')]
    limit: Option<usize>,
    /// Limit test duration to specified number of seconds.
    #[arg(long, short = 't')]
    time: Option<u64>,
    /// Only measure send speed.
    #[arg(long, short = 's')]
    send_only: bool,
    /// Only measure receive speed.
    #[arg(long, short = 'r')]
    recv_only: bool,
    /// Block the receiver
    #[arg(long, short = 'b')]
    recv_block: bool,
    /// Do not display the link monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Display all possible (including disconnected) links in the link monitor.
    #[arg(long, short = 'a')]
    all_links: bool,
    /// Output speed report in JSON format.
    #[arg(long, short = 'j')]
    json: bool,
    /// Encrypt all links using TLS, without authenticating server.
    ///
    /// Warning: no server authentication is performed!
    #[arg(long)]
    tls: bool,
    /// TCP server name or IP addresses and port number.
    #[arg(long)]
    tcp: Vec<String>,
    /// TCP link filter.
    ///
    /// none: no link filtering.
    ///
    /// interface-interface: one link for each pair of local and remote interface.
    ///
    /// interface-ip: one link for each pair of local interface and remote IP address.
    #[arg(long, value_parser = parse_tcp_link_filter, default_value = "interface-interface")]
    tcp_link_filter: TcpLinkFilter,
    /// WebSocket hosts or URLs.
    ///
    /// Default server port number is 8080 and path is /agg-speed.
    #[arg(long)]
    websocket: Vec<String>,
    /// Bluetooth RFCOMM server address.
    #[cfg(feature = "bluer")]
    #[arg(long, value_parser=parse_rfcomm)]
    rfcomm: Option<aggligator_transport_bluer::rfcomm::SocketAddr>,
    /// Bluetooth RFCOMM profile server address.
    #[cfg(feature = "bluer")]
    #[arg(long)]
    rfcomm_profile: Option<aggligator_transport_bluer::rfcomm_profile::Address>,
    /// USB device serial number (equals hostname of speed test device).
    #[cfg(feature = "usb-host")]
    #[arg(long)]
    usb: Option<String>,
}

#[cfg(feature = "bluer")]
fn parse_rfcomm(arg: &str) -> Result<aggligator_transport_bluer::rfcomm::SocketAddr> {
    match arg.parse::<aggligator_transport_bluer::rfcomm::SocketAddr>() {
        Ok(addr) => Ok(addr),
        Err(err) => match arg.parse::<aggligator_transport_bluer::rfcomm::Address>() {
            Ok(addr) => Ok(aggligator_transport_bluer::rfcomm::SocketAddr::new(addr, RFCOMM_CHANNEL)),
            Err(_) => Err(err.into()),
        },
    }
}

impl ClientCli {
    pub async fn run(mut self, cfg: Cfg, dump: Option<PathBuf>) -> Result<()> {
        if !stdout().is_tty() {
            self.no_monitor = true;
        }

        let mut builder = ConnectorBuilder::new(cfg);
        if let Some(dump) = dump.clone() {
            let (tx, rx) = mpsc::channel(DUMP_BUFFER);
            builder.task().dump(tx);
            exec::spawn(dump_to_json_line_file(dump, rx));
        }
        if self.tls {
            builder.wrap(TlsClient::new(
                Arc::new(tls_client_config()),
                ServerName::try_from(TLS_SERVER_NAME).unwrap(),
            ));
        }

        let mut connector = builder.build();
        let mut targets = Vec::new();
        let ip_version = IpVersion::from_only(self.ipv4, self.ipv6)?;

        if !self.tcp.is_empty() {
            let mut tcp_connector =
                TcpConnector::new(self.tcp.clone(), TCP_PORT).await.context("cannot resolve TCP target")?;
            tcp_connector.set_ip_version(ip_version);
            tcp_connector.set_link_filter(self.tcp_link_filter);
            targets.push(tcp_connector.to_string());
            connector.add(tcp_connector);
        }

        #[cfg(feature = "bluer")]
        if let Some(addr) = self.rfcomm {
            let rfcomm_connector = RfcommConnector::new(addr);
            targets.push(addr.to_string());
            connector.add(rfcomm_connector);
        }

        #[cfg(feature = "bluer")]
        if let Some(addr) = self.rfcomm_profile {
            let rfcomm_profile_connector = RfcommProfileConnector::new(addr, RFCOMM_UUID)
                .await
                .context("RFCOMM profile connector failed")?;
            targets.push(addr.to_string());
            connector.add(rfcomm_profile_connector);
        }

        #[cfg(feature = "usb-host")]
        if let Some(serial) = &self.usb {
            let filter_serial = serial.clone();
            let filter = move |dev: &aggligator_transport_usb::DeviceInfo,
                               iface: &aggligator_transport_usb::InterfaceInfo| {
                dev.vendor_id == usb::VID
                    && dev.product_id == usb::PID
                    && dev.manufacturer == usb::MANUFACTURER
                    && dev.product == usb::PRODUCT
                    && dev.serial_number == filter_serial
                    && dev.class_code == usb::CLASS
                    && dev.sub_class_code == usb::SUB_CLASS
                    && dev.protocol_code == usb::PROTOCOL
                    && iface.class_code == usb::INTERFACE_CLASS
                    && iface.sub_class_code == usb::INTERFACE_SUB_CLASS
                    && iface.protocol_code == usb::INTERFACE_PROTOCOL
                    && iface.description == usb::INTERFACE_NAME
            };
            let usb_connector =
                aggligator_transport_usb::UsbConnector::new(filter).context("cannot initialize USB")?;
            targets.push(format!("USB {serial}"));
            connector.add(usb_connector);
        }

        if !self.websocket.is_empty() {
            let websockets = self.websocket.iter().map(|url| {
                let mut url = url.clone();
                if !url.starts_with("ws") {
                    url = format!("ws://{url}:{WEBSOCKET_PORT}{WEBSOCKET_PATH}");
                }
                url
            });
            let mut ws_connector =
                WebSocketConnector::new(websockets).await.context("cannot resolve WebSocket target")?;
            ws_connector.set_ip_version(ip_version);
            targets.push(ws_connector.to_string());
            connector.add(ws_connector);
        }

        if targets.is_empty() {
            bail!("No connection transports.");
        }

        let target = targets.join(", ");
        let title = format!("Speed test against {target} {}", if self.tls { "with TLS" } else { "" });

        let outgoing = connector.channel().unwrap();
        let control = connector.control();

        let tags_rx = connector.available_tags_watch();
        let tag_err_rx = connector.link_errors();
        let (disabled_tags_tx, mut disabled_tags_rx) = watch::channel(HashSet::new());
        exec::spawn(async move {
            loop {
                let disabled_tags: HashSet<LinkTagBox> = (*disabled_tags_rx.borrow_and_update()).clone();
                connector.set_disabled_tags(disabled_tags);

                if disabled_tags_rx.changed().await.is_err() {
                    break;
                }
            }
        });

        let (control_tx, control_rx) = broadcast::channel(8);
        let (header_tx, header_rx) = watch::channel(Default::default());
        let (speed_tx, mut speed_rx) = watch::channel(Default::default());

        let _ = control_tx.send((control.clone(), String::new()));
        drop(control_tx);

        if !self.no_monitor {
            exec::spawn(async move {
                loop {
                    let (send, recv) = *speed_rx.borrow_and_update();
                    let speed = format!(
                        "{}{}\r\n{}{}\r\n",
                        "Upstream:   ".grey(),
                        format_speed(send),
                        "Downstream: ".grey(),
                        format_speed(recv)
                    );
                    let header = format!("{}\r\n\r\n{}{}", title.clone().bold(), speed, debug_warning());

                    if header_tx.send(header).is_err() {
                        break;
                    }

                    if speed_rx.changed().await.is_err() {
                        break;
                    }
                }
            });
        }

        let speed_test = async move {
            let ch = outgoing.await.context("cannot establish aggligator connection")?;
            let (r, w) = ch.into_stream().into_split();
            anyhow::Ok(
                speed_test(
                    &target,
                    r,
                    w,
                    self.limit.map(|mb| mb * 1_048_576),
                    self.time.map(Duration::from_secs),
                    !self.recv_only,
                    !self.send_only,
                    self.recv_block,
                    INTERVAL,
                    if self.no_monitor { None } else { Some(speed_tx) },
                )
                .await?,
            )
        };

        let (tx_speed, rx_speed) = if self.no_monitor {
            drop(tag_err_rx);
            let res = speed_test.await;
            res?
        } else {
            let task = exec::spawn(speed_test);
            block_in_place(|| {
                interactive_monitor(
                    header_rx,
                    control_rx,
                    1,
                    self.all_links.then_some(tags_rx),
                    Some(tag_err_rx),
                    self.all_links.then_some(disabled_tags_tx),
                )
            })?;

            task.abort();
            match task.await {
                Ok(res) => res?,
                Err(_) => {
                    println!("Exiting...");
                    control.terminated().await?;
                    return Ok(());
                }
            }
        };

        if self.json {
            let report = SpeedReport {
                data_limit: self.limit,
                time_limit: self.time,
                send_speed: tx_speed,
                recv_speed: tx_speed,
            };
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        } else {
            println!("Upstream:   {}", format_speed(tx_speed));
            println!("Downstream: {}", format_speed(rx_speed));
        }

        println!("Exiting...");
        control.terminated().await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeedReport {
    data_limit: Option<usize>,
    time_limit: Option<u64>,
    send_speed: f64,
    recv_speed: f64,
}

#[derive(Parser)]
pub struct ServerCli {
    /// Listen on each network interface individually.
    #[arg(long, short = 'i')]
    individual_interfaces: bool,
    /// Do not display the link monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Exit after handling one connection.
    #[arg(long)]
    oneshot: bool,
    /// Encrypt all links using TLS.
    #[arg(long)]
    tls: bool,
    /// TCP port to listen on.
    #[arg(long, default_value_t = TCP_PORT)]
    tcp: u16,
    /// RFCOMM channel number to listen on.
    #[cfg(feature = "bluer")]
    #[arg(long, default_value_t = RFCOMM_CHANNEL)]
    rfcomm: u8,
    /// Listen on USB device controller (UDC).
    #[cfg(feature = "usb-device")]
    #[arg(long)]
    usb: bool,
    /// WebSocket (HTTP) port to listen on.
    #[arg(long, default_value_t = WEBSOCKET_PORT)]
    websocket: u16,
}

impl ServerCli {
    pub async fn run(mut self, cfg: Cfg, dump: Option<PathBuf>) -> Result<()> {
        if !stdout().is_tty() {
            self.no_monitor = true;
        }

        let mut builder = AcceptorBuilder::new(cfg);
        if let Some(dump) = dump {
            builder.set_task_cfg(move |task| {
                let (tx, rx) = mpsc::channel(DUMP_BUFFER);
                task.dump(tx);
                exec::spawn(dump_to_json_line_file(dump.clone(), rx));
            });
        }
        if self.tls {
            builder.wrap(TlsServer::new(Arc::new(tls_server_config())));
        }

        let acceptor = builder.build();
        let mut ports = Vec::new();

        let tcp_acceptor_res = if self.individual_interfaces {
            TcpAcceptor::all_interfaces(self.tcp).await
        } else {
            TcpAcceptor::new([SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), self.tcp)]).await
        };
        match tcp_acceptor_res {
            Ok(tcp) => {
                ports.push(format!("TCP {tcp}"));
                acceptor.add(tcp);
            }
            Err(err) => eprintln!("Cannot listen on TCP port {}: {err}", self.tcp),
        }

        #[cfg(feature = "bluer")]
        match RfcommAcceptor::new(aggligator_transport_bluer::rfcomm::SocketAddr::new(
            aggligator_transport_bluer::rfcomm::Address::any(),
            self.rfcomm,
        ))
        .await
        {
            Ok(rfcomm) => {
                acceptor.add(rfcomm);
                ports.push(format!("RFCOMM channel {}", self.rfcomm));
            }
            Err(err) => eprintln!("Cannot listen on RFCOMM channel {}: {err}", self.rfcomm),
        }

        #[cfg(feature = "bluer")]
        match RfcommProfileAcceptor::new(RFCOMM_UUID).await {
            Ok(rfcomm_profile) => {
                acceptor.add(rfcomm_profile);
                ports.push("RFCOMM profile".to_string());
            }
            Err(err) => eprintln!("Cannot listen on RFCOMM profile {RFCOMM_UUID}: {err}"),
        }

        #[cfg(feature = "usb-device")]
        let _usb_reg = if self.usb {
            fn register_usb(
                serial: &str,
            ) -> Result<(usb_gadget::RegGadget, upc::device::UpcFunction, std::ffi::OsString)> {
                let udc = usb_gadget::default_udc()?;
                let udc_name = udc.name().to_os_string();

                let (upc, func_hnd) = upc::device::UpcFunction::new(
                    upc::device::InterfaceId::new(upc::Class::new(
                        usb::INTERFACE_CLASS,
                        usb::INTERFACE_SUB_CLASS,
                        usb::INTERFACE_PROTOCOL,
                    ))
                    .with_name(usb::INTERFACE_NAME),
                );

                let reg = usb_gadget::Gadget::new(
                    usb_gadget::Class::new(usb::CLASS, usb::SUB_CLASS, usb::PROTOCOL),
                    usb_gadget::Id::new(usb::VID, usb::PID),
                    usb_gadget::Strings::new(usb::MANUFACTURER, usb::PRODUCT, serial),
                )
                .with_os_descriptor(usb_gadget::OsDescriptor::microsoft())
                .with_config(usb_gadget::Config::new("config").with_function(func_hnd))
                .bind(&udc)?;

                Ok((reg, upc, udc_name))
            }

            let serial = gethostname::gethostname().to_string_lossy().to_string();
            match register_usb(&serial) {
                Ok((usb_reg, upc, udc_name)) => {
                    acceptor.add(aggligator_transport_usb::UsbAcceptor::new(upc, &udc_name));
                    ports.push(format!("UDC {} ({serial})", udc_name.to_string_lossy()));
                    Some(usb_reg)
                }
                Err(err) => {
                    eprintln!("Cannot listen on USB: {err}");
                    None
                }
            }
        } else {
            None
        };

        let (wsa, router) = WebSocketAcceptor::new(WEBSOCKET_PATH);
        acceptor.add(wsa);
        ports.push(format!("WebSocket {}", self.websocket));
        let websocket_addr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), self.websocket);
        exec::spawn(async move {
            if let Err(err) = axum_server::bind(websocket_addr)
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await
            {
                eprintln!("Cannot listen on WebSocket {}: {err}", websocket_addr);
            }
        });

        if ports.is_empty() {
            bail!("No listening transports.");
        }

        let ports = ports.join(", ");
        let title = format!("Speed test server listening on {ports} {}", if self.tls { "with TLS" } else { "" });

        let tag_error_rx = acceptor.link_errors();
        let (control_tx, control_rx) = broadcast::channel(8);
        let no_monitor = self.no_monitor;
        let oneshot = self.oneshot;
        let task = async move {
            loop {
                let (ch, control) = acceptor.accept().await?;
                let _ = control_tx.send((control, String::new()));

                exec::spawn(async move {
                    let id = ch.id();
                    let (r, w) = ch.into_stream().into_split();
                    let (speed_tx, _speed_rx) = watch::channel(Default::default());
                    let speed_tx_opt = if no_monitor { None } else { Some(speed_tx) };
                    let res =
                        speed_test(&id.to_string(), r, w, None, None, true, true, false, INTERVAL, speed_tx_opt)
                            .await;
                    if oneshot {
                        exit(res.is_err() as _);
                    }
                });
            }

            #[allow(unreachable_code)]
            anyhow::Ok(())
        };

        if self.no_monitor {
            task.await?;
        } else {
            let task = exec::spawn(task);

            let header_rx = watch::channel(format!("{}\r\n{}", title.bold(), debug_warning())).1;
            block_in_place(|| interactive_monitor(header_rx, control_rx, 1, None, Some(tag_error_rx), None))?;

            task.abort();
            if let Ok(res) = task.await {
                res?
            }
        }

        Ok(())
    }
}
