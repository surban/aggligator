//! Tunnel TCP connections in aggregated connections.

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use futures::{future, FutureExt};
use socket2::SockRef;
use std::{
    collections::{HashMap, HashSet},
    io::stdout,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::exit,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    select,
    sync::{broadcast, mpsc, oneshot, watch},
    task::block_in_place,
    time::sleep,
};

use aggligator::{
    alc::{ReceiverStream, SenderSink},
    cfg::Cfg,
    dump::dump_to_json_line_file,
    exec,
    transport::{AcceptorBuilder, ConnectingTransport, ConnectorBuilder, LinkTagBox},
};
use aggligator_monitor::monitor::{interactive_monitor, watch_tags};
use aggligator_transport_tcp::{IpVersion, TcpAcceptor, TcpConnector, TcpLinkFilter};
use aggligator_util::{init_log, load_cfg, parse_tcp_link_filter, print_default_cfg};

#[cfg(feature = "bluer")]
use aggligator_transport_bluer::rfcomm::{RfcommAcceptor, RfcommConnector};

#[cfg(feature = "usb-device")]
use aggligator_transport_usb::{upc, usb_gadget};

const TCP_PORT: u16 = 5800;
const FLUSH_DELAY: Option<Duration> = Some(Duration::from_millis(10));
const DUMP_BUFFER: usize = 8192;

#[cfg(any(feature = "usb-host", feature = "usb-device"))]
mod usb {
    pub const VID: u16 = u16::MAX - 2;
    pub const PID: u16 = u16::MAX - 2;
    pub const MANUFACTURER: &str = env!("CARGO_PKG_NAME");
    pub const PRODUCT: &str = env!("CARGO_BIN_NAME");
    pub const CLASS: u8 = 255;
    pub const SUB_CLASS: u8 = 255;
    pub const PROTOCOL: u8 = 255;
    pub const INTERFACE_CLASS: u8 = 255;
    pub const INTERFACE_SUB_CLASS: u8 = 240;
    pub const INTERFACE_PROTOCOL: u8 = 1;
    pub const DEFAULT_INTERFACE_NAME: &str = "agg-tunnel";
}

/// Forward TCP ports through a connection of aggregated links.
///
/// This uses Aggligator to combine multiple TCP links into one connection,
/// providing the combined speed and resilience to individual link faults.
#[derive(Parser)]
#[command(name = "agg-tunnel", author, version)]
pub struct TunnelCli {
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
    /// Tunnel client.
    Client(ClientCli),
    /// Tunnel server.
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

    let cli = TunnelCli::parse();
    let cfg = load_cfg(&cli.cfg)?;
    let dump = cli.dump.clone();

    match cli.command {
        Commands::Client(client) => client.run(cfg, dump).await?,
        Commands::Server(server) => server.run(cfg, dump).await?,
        Commands::ShowCfg => print_default_cfg(),
        Commands::ManPages => clap_mangen::generate_to(TunnelCli::command(), ".")?,
        Commands::Markdown => println!("{}", clap_markdown::help_markdown::<TunnelCli>()),
    }

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
    /// Do not display the link monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Display all possible (including disconnected) links in the link monitor.
    #[arg(long, short = 'a')]
    all_links: bool,
    /// Ports to forward from server to client.
    ///
    /// Takes the form `server_port:client_port` and can be specified multiple times.
    ///
    /// The port must have been enabled on the server.
    #[arg(long, short = 'p', value_parser = parse_key_val::<u16, u16>, required=true)]
    port: Vec<(u16, u16)>,
    /// Forward ports on all local interfaces.
    ///
    /// If unspecified only loopback connections are accepted.
    #[arg(long, short = 'g')]
    global: bool,
    /// Exit after handling one connection.
    #[arg(long)]
    once: bool,
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
    /// Bluetooth RFCOMM server address.
    #[cfg(feature = "bluer")]
    #[arg(long)]
    rfcomm: Option<aggligator_transport_bluer::rfcomm::SocketAddr>,
    /// USB device serial number (equals hostname of speed test device).
    ///
    /// Use - to match any device.
    #[cfg(feature = "usb-host")]
    #[arg(long)]
    usb: Option<String>,
    /// USB interface name.
    #[cfg(feature = "usb-host")]
    #[arg(long, default_value=usb::DEFAULT_INTERFACE_NAME)]
    usb_interface_name: String,
}

impl ClientCli {
    async fn run(self, cfg: Cfg, dump: Option<PathBuf>) -> Result<()> {
        let no_monitor = self.no_monitor || !stdout().is_tty();
        let once = self.once;

        let ports: Vec<_> =
            self.port.clone().into_iter().map(|(s, c)| if s == 0 { (c, c) } else { (s, c) }).collect();

        let mut watch_conn: Vec<Box<dyn ConnectingTransport>> = Vec::new();
        let mut targets = Vec::new();

        let tcp_connector = if !self.tcp.is_empty() {
            match TcpConnector::new(self.tcp.clone(), TCP_PORT).await {
                Ok(mut tcp) => {
                    tcp.set_ip_version(IpVersion::from_only(self.ipv4, self.ipv6)?);
                    tcp.set_link_filter(self.tcp_link_filter);
                    targets.push(tcp.to_string());
                    watch_conn.push(Box::new(tcp.clone()));
                    Some(tcp)
                }
                Err(err) => {
                    eprintln!("cannot use TCP target: {err}");
                    None
                }
            }
        } else {
            None
        };

        #[cfg(feature = "bluer")]
        let rfcomm_connector = match self.rfcomm {
            Some(addr) => {
                let rfcomm_connector = RfcommConnector::new(addr);
                targets.push(addr.to_string());
                watch_conn.push(Box::new(rfcomm_connector.clone()));
                Some(rfcomm_connector)
            }
            None => None,
        };

        #[cfg(feature = "usb-host")]
        let usb_connector = {
            if let Some(serial) = &self.usb {
                targets.push(format!("USB {serial}"));
            }

            let usb = self.usb.clone();
            move || match &usb {
                Some(serial) => {
                    let filter_serial = serial.clone();
                    let filter_interface_name = self.usb_interface_name.clone();
                    let filter =
                        move |dev: &aggligator_transport_usb::DeviceInfo,
                              iface: &aggligator_transport_usb::InterfaceInfo| {
                            dev.vendor_id == usb::VID
                                && dev.product_id == usb::PID
                                && dev.manufacturer.as_deref() == Some(usb::MANUFACTURER)
                                && dev.product.as_deref() == Some(usb::PRODUCT)
                                && (dev.serial_number.as_deref() == Some(filter_serial.as_str())
                                    || filter_serial == "-")
                                && dev.class_code == usb::CLASS
                                && dev.sub_class_code == usb::SUB_CLASS
                                && dev.protocol_code == usb::PROTOCOL
                                && iface.class_code == usb::INTERFACE_CLASS
                                && iface.sub_class_code == usb::INTERFACE_SUB_CLASS
                                && iface.protocol_code == usb::INTERFACE_PROTOCOL
                                && iface.description.as_deref() == Some(filter_interface_name.as_str())
                        };

                    match aggligator_transport_usb::UsbConnector::new(filter) {
                        Ok(c) => Some(c),
                        Err(err) => {
                            eprintln!("cannot use USB target: {err}");
                            None
                        }
                    }
                }
                None => None,
            }
        };

        if targets.is_empty() {
            bail!("No connection transports.");
        }

        let target = targets.join(", ");
        let title = format!(
            "Tunneling ports of {target} (remote->local): {}",
            ports.iter().map(|(s, l)| format!("{s}->{l}")).collect::<Vec<_>>().join(" ")
        );

        let (tag_err_tx, tag_err_rx) = broadcast::channel(128);
        let (disabled_tags_tx, disabled_tags_rx) = watch::channel(HashSet::new());
        let (control_tx, control_rx) = broadcast::channel(8);
        let all_tags_rx = self.all_links.then(|| watch_tags(watch_conn));

        let mut port_tasks = Vec::new();
        for (server_port, client_port) in ports {
            let listeners = if self.global {
                let socket = TcpSocket::new_v6()?;
                let _ = SockRef::from(&socket).set_only_v6(false);
                socket
                    .bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), client_port))
                    .context(format!("cannot bind to local port {client_port}"))?;
                vec![socket.listen(16)?]
            } else {
                let listener_v4 = TcpListener::bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), client_port))
                    .await
                    .context(format!("cannot bind to local IPv4 port {client_port}"))?;
                let listener_v6 = TcpListener::bind(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), client_port))
                    .await
                    .context(format!("cannot bind to local IPv6 port {client_port}"))?;
                vec![listener_v4, listener_v6]
            };

            let control_tx = control_tx.clone();
            let tag_err_tx = tag_err_tx.clone();
            let disabled_tags_rx = disabled_tags_rx.clone();
            let port_cfg = cfg.clone();
            let tcp_connector = tcp_connector.clone();
            #[cfg(feature = "bluer")]
            let rfcomm_connector = rfcomm_connector.clone();
            #[cfg(feature = "usb-host")]
            let usb_connector = usb_connector.clone();
            let dump = dump.clone();
            port_tasks.push(async move {
                loop {
                    let (socket, src) =
                        future::select_all(listeners.iter().map(|l| l.accept().boxed())).await.0?;

                    let mut builder = ConnectorBuilder::new(port_cfg.clone());
                    if let Some(dump) = dump.clone() {
                        let (tx, rx) = mpsc::channel(DUMP_BUFFER);
                        builder.task().dump(tx);
                        exec::spawn(dump_to_json_line_file(dump, rx));
                    }

                    let mut connector = builder.build();
                    if let Some(c) = tcp_connector.clone() {
                        connector.add(c);
                    }
                    #[cfg(feature = "bluer")]
                    if let Some(c) = rfcomm_connector.clone() {
                        connector.add(c);
                    }
                    #[cfg(feature = "usb-host")]
                    if let Some(c) = usb_connector() {
                        connector.add(c);
                    }
                    let control = connector.control();
                    let outgoing = connector.channel().unwrap();

                    let mut conn_tag_err_rx = connector.link_errors();
                    let tag_err_tx = tag_err_tx.clone();
                    exec::spawn(async move {
                        while let Ok(err) = conn_tag_err_rx.recv().await {
                            let _ = tag_err_tx.send(err);
                        }
                    });
                    let mut disabled_tags_rx = disabled_tags_rx.clone();
                    exec::spawn(async move {
                        loop {
                            let disabled_tags: HashSet<LinkTagBox> =
                                (*disabled_tags_rx.borrow_and_update()).clone();
                            connector.set_disabled_tags(disabled_tags);
                            if disabled_tags_rx.changed().await.is_err() {
                                break;
                            }
                        }
                    });

                    let _ = control_tx.send((control.clone(), format!("{src}: {server_port}->{client_port}")));

                    exec::spawn(async move {
                        if no_monitor {
                            eprintln!("Incoming connection from {src} requests port {client_port}");
                        }

                        let ch = outgoing.await?;
                        let (server_read, mut server_write) = ch.into_stream().into_split();
                        server_write.write_u16(server_port).await?;

                        let (client_read, client_write) = socket.into_split();
                        exec::spawn(forward(client_read, server_write));
                        forward(server_read, client_write).await?;

                        if no_monitor {
                            eprintln!("Incoming connection from {src} done");
                        }

                        if once {
                            exit(0);
                        }

                        anyhow::Ok(())
                    });
                }

                #[allow(unreachable_code)]
                anyhow::Ok(())
            });
        }
        let task = future::try_join_all(port_tasks);

        if self.no_monitor {
            drop(control_rx);
            eprintln!("{title}");
            task.await?;
        } else {
            let task = exec::spawn(task);

            let header_rx = watch::channel(format!("{title}\r\n").bold().to_string()).1;
            block_in_place(|| {
                interactive_monitor(
                    header_rx,
                    control_rx,
                    1,
                    all_tags_rx,
                    Some(tag_err_rx),
                    self.all_links.then_some(disabled_tags_tx),
                )
            })?;
            task.abort();
            if let Ok(res) = task.await {
                res?;
            }
        }

        Ok(())
    }
}

#[derive(Parser)]
pub struct ServerCli {
    /// Do not display the link monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Ports to forward to clients.
    ///
    /// Takes the form `port` or `target:port` and can be specified multiple times.
    ///
    /// Target can be a host name or IP address. If unspecified localhost is used as target.
    #[arg(long, short = 'p', value_parser = parse_key_val::<String, u16>, required=true)]
    port: Vec<(String, u16)>,
    /// TCP port to listen on.
    #[arg(long)]
    tcp: Option<u16>,
    /// RFCOMM channel number to listen on.
    #[cfg(feature = "bluer")]
    #[arg(long)]
    rfcomm: Option<u8>,
    /// Listen on USB device controller (UDC).
    #[cfg(feature = "usb-device")]
    #[arg(long)]
    usb: bool,
    /// USB interface name.
    #[cfg(feature = "usb-host")]
    #[arg(long, default_value=usb::DEFAULT_INTERFACE_NAME)]
    usb_interface_name: String,
}

impl ServerCli {
    async fn run(self, cfg: Cfg, dump: Option<PathBuf>) -> Result<()> {
        let no_monitor = self.no_monitor || !stdout().is_tty();

        let ports: Arc<HashMap<_, _>> = Arc::new(
            self.port
                .clone()
                .into_iter()
                .map(|(target, port)| {
                    (port, if target.is_empty() { format!("127.0.0.1:{port}") } else { target })
                })
                .collect(),
        );

        let title = format!(
            "Serving targets: {}",
            ports.iter().map(|(port, target)| format!("{target}->{port}")).collect::<Vec<_>>().join(" ")
        );

        let mut builder = AcceptorBuilder::new(cfg);
        if let Some(dump) = dump {
            builder.set_task_cfg(move |task| {
                let (tx, rx) = mpsc::channel(DUMP_BUFFER);
                task.dump(tx);
                exec::spawn(dump_to_json_line_file(dump.clone(), rx));
            });
        }

        let acceptor = builder.build();
        let mut server_ports = Vec::new();

        if let Some(port) = self.tcp {
            match TcpAcceptor::new([SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port)]).await {
                Ok(tcp) => {
                    server_ports.push(format!("TCP {tcp}"));
                    acceptor.add(tcp);
                }
                Err(err) => eprintln!("Cannot listen on TCP port {port}: {err}"),
            }
        }

        #[cfg(feature = "bluer")]
        if let Some(ch) = self.rfcomm {
            match RfcommAcceptor::new(aggligator_transport_bluer::rfcomm::SocketAddr::new(
                aggligator_transport_bluer::rfcomm::Address::any(),
                ch,
            ))
            .await
            {
                Ok(rfcomm) => {
                    acceptor.add(rfcomm);
                    server_ports.push(format!("RFCOMM channel {ch}"));
                }
                Err(err) => eprintln!("Cannot listen on RFCOMM channel {ch}: {err}"),
            }
        }

        #[cfg(feature = "usb-device")]
        let _usb_reg = if self.usb {
            fn register_usb(
                serial: &str, interface_name: &str,
            ) -> Result<(usb_gadget::RegGadget, upc::device::UpcFunction, std::ffi::OsString)> {
                let udc = usb_gadget::default_udc()?;
                let udc_name = udc.name().to_os_string();

                let (upc, func_hnd) = upc::device::UpcFunction::new(
                    upc::device::InterfaceId::new(upc::Class::new(
                        usb::INTERFACE_CLASS,
                        usb::INTERFACE_SUB_CLASS,
                        usb::INTERFACE_PROTOCOL,
                    ))
                    .with_name(interface_name),
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
            match register_usb(&serial, &self.usb_interface_name) {
                Ok((usb_reg, upc, udc_name)) => {
                    acceptor.add(aggligator_transport_usb::UsbAcceptor::new(upc, &udc_name));
                    server_ports.push(format!("UDC {} ({serial})", udc_name.to_string_lossy()));
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

        if server_ports.is_empty() {
            bail!("No listening transports.");
        }

        let (control_tx, control_rx) = broadcast::channel(16);

        let task = async move {
            loop {
                let (ch, control) = acceptor.accept().await?;
                let id = ch.id();

                let control_tx = control_tx.clone();
                let (target_tx, target_rx) = oneshot::channel();
                exec::spawn(async move {
                    let name = if let Ok((port, target)) = target_rx.await {
                        format!("{port} -> {target}")
                    } else {
                        "unknown".to_string()
                    };
                    let _ = control_tx.send((control, name));
                });

                let ports = ports.clone();
                exec::spawn(async move {
                    let (client_read, client_write) = ch.into_stream().into_split();
                    if let Err(err) =
                        Self::handle_client(ports, client_write, client_read, !no_monitor, target_tx).await
                    {
                        if no_monitor {
                            eprintln!("Connection {id} failed: {err}");
                        }
                    }
                });
            }

            #[allow(unreachable_code)]
            anyhow::Ok(())
        };

        if no_monitor {
            drop(control_rx);
            eprintln!("{title}");
            task.await?
        } else {
            let task = exec::spawn(task);

            let header_rx = watch::channel(format!("{title}\r\n").bold().to_string()).1;
            interactive_monitor(header_rx, control_rx, 1, None, None, None)?;

            task.abort();
            if let Ok(res) = task.await {
                res?
            }
        }

        Ok(())
    }

    async fn handle_client(
        ports: Arc<HashMap<u16, String>>, client_write: SenderSink, mut client_read: ReceiverStream, quiet: bool,
        target_tx: oneshot::Sender<(u16, String)>,
    ) -> Result<()> {
        let port = client_read.read_u16().await?;

        if let Some(target) = ports.get(&port) {
            let _ = target_tx.send((port, target.clone()));
            if !quiet {
                eprintln!("Client wants port {port} which connects to {target}");
            }

            let socket = TcpStream::connect(target).await?;
            let (target_read, target_write) = socket.into_split();

            if !quiet {
                eprintln!("Connection to {target} established, starting forwarding");
            }

            exec::spawn(forward(client_read, target_write));
            forward(target_read, client_write).await?;

            if !quiet {
                eprintln!("Forwarding for {target} done");
            }
        } else if !quiet {
            eprintln!("Client wants port {port} which is not published");
        }

        Ok(())
    }
}

async fn forward(mut read: impl AsyncRead + Unpin, mut write: impl AsyncWrite + Unpin) -> Result<()> {
    loop {
        let mut buf = vec![0; 65_536];

        #[allow(clippy::unused_io_amount)] // workaround for clippy bug, IO amount is handled
        let n = match FLUSH_DELAY {
            Some(delay) => select! {
                res = read.read(&mut buf) => res?,
                () = sleep(delay) => {
                    write.flush().await?;
                    read.read(&mut buf).await?
                }
            },
            None => read.read(&mut buf).await?,
        };

        if n == 0 {
            break;
        } else {
            buf.truncate(n);
        }

        write.write_all(&buf).await?;
    }

    write.flush().await?;
    Ok(())
}

fn parse_key_val<T, U>(s: &str) -> std::result::Result<(T, U), Box<dyn std::error::Error + Send + Sync + 'static>>
where
    T: std::str::FromStr + Default,
    T::Err: std::error::Error + Send + Sync + 'static,
    U: std::str::FromStr,
    U::Err: std::error::Error + Send + Sync + 'static,
{
    match s.rfind(':') {
        Some(pos) => Ok((s[..pos].parse()?, s[pos + 1..].parse()?)),
        None => Ok((Default::default(), s.parse()?)),
    }
}
