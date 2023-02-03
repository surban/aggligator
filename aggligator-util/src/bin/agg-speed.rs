//! Aggligator speed test.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use rustls::{
    client::{ServerCertVerified, ServerCertVerifier},
    Certificate, ClientConfig, PrivateKey, RootCertStore, ServerConfig, ServerName,
};
use rustls_pemfile::{certs, pkcs8_private_keys};
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

use aggligator::{cfg::Cfg, dump::dump_to_json_line_file};
use aggligator_util::{
    cli::{init_log, load_cfg, print_default_cfg},
    monitor::{format_speed, interactive_monitor},
    speed::{speed_test, INTERVAL},
    transport::{
        tcp::{IpVersion, TcpAcceptor, TcpConnector},
        tls::{TlsClient, TlsServer},
        AcceptorBuilder, ConnectorBuilder, LinkTagBox,
    },
};

#[cfg(feature = "rfcomm")]
use aggligator_util::transport::rfcomm::{RfcommAcceptor, RfcommConnector};
#[cfg(feature = "rfcomm_profile")]
use aggligator_util::transport::rfcomm_profile::{RfcommProfileAcceptor, RfcommProfileConnector};

const TCP_PORT: u16 = 5700;
const DUMP_BUFFER: usize = 8192;

#[cfg(feature = "rfcomm")]
const RFCOMM_CHANNEL: u8 = 20;
#[cfg(feature = "rfcomm_profile")]
const RFCOMM_UUID: bluer::Uuid = bluer::Uuid::from_u128(0x7f95058c_c00e_44a9_9003_2ce90d60e2e7);

static TLS_CERT_PEM: &[u8] = include_bytes!("agg-speed-cert.pem");
static TLS_KEY_PEM: &[u8] = include_bytes!("agg-speed-key.pem");
static TLS_SERVER_NAME: &str = "aggligator.rs";

fn tls_cert() -> Certificate {
    let mut reader = BufReader::new(TLS_CERT_PEM);
    Certificate(certs(&mut reader).unwrap().pop().unwrap())
}

fn tls_key() -> PrivateKey {
    let mut reader = BufReader::new(TLS_KEY_PEM);
    PrivateKey(pkcs8_private_keys(&mut reader).unwrap().pop().unwrap())
}

/// Accepts every TLS server certificate.
///
/// For speed test only! Do not use in production code!
struct TlsNullVerifier;

impl ServerCertVerifier for TlsNullVerifier {
    fn verify_server_cert(
        &self, _end_entity: &Certificate, _intermediates: &[Certificate], _server_name: &ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>, _ocsp_response: &[u8], _now: std::time::SystemTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
}

fn tls_client_config() -> ClientConfig {
    let mut root_store = RootCertStore::empty();
    root_store.add(&tls_cert()).unwrap();
    let mut cfg =
        ClientConfig::builder().with_safe_defaults().with_root_certificates(root_store).with_no_client_auth();
    cfg.dangerous().set_certificate_verifier(Arc::new(TlsNullVerifier));
    cfg
}

fn tls_server_config() -> ServerConfig {
    ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(vec![tls_cert()], tls_key())
        .unwrap()
}

/// Run speed test using a connection consisting of aggregated TCP links.
///
/// This uses Aggligator to combine multiple TCP links into one connection,
/// providing the combined speed and resilience to individual link faults.
#[derive(Parser)]
#[command(author, version)]
pub struct SpeedCli {
    /// Configuration file.
    #[arg(long)]
    cfg: Option<PathBuf>,
    /// Dump analysis data to file.
    #[arg(long, short = 'd')]
    dump: Option<PathBuf>,
    /// Use unacked limit link steering mode.
    #[arg(long, short = 'u')]
    unacked_limit: bool,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    init_log();

    let cli = SpeedCli::parse();
    let cfg = load_cfg(&cli.cfg, cli.unacked_limit)?;
    let dump = cli.dump.clone();

    match cli.command {
        Commands::Client(client) => client.run(cfg, dump).await?,
        Commands::Server(server) => server.run(cfg, dump).await?,
        Commands::ShowCfg => print_default_cfg(),
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
    /// Bluetooth RFCOMM server address.
    #[cfg(feature = "rfcomm")]
    #[arg(long, value_parser=parse_rfcomm)]
    rfcomm: Option<bluer::rfcomm::SocketAddr>,
    /// Bluetooth RFCOMM profile server address.
    #[cfg(feature = "rfcomm_profile")]
    #[arg(long)]
    rfcomm_profile: Option<bluer::Address>,
}

#[cfg(feature = "rfcomm")]
fn parse_rfcomm(arg: &str) -> Result<bluer::rfcomm::SocketAddr> {
    match arg.parse::<bluer::rfcomm::SocketAddr>() {
        Ok(addr) => Ok(addr),
        Err(err) => match arg.parse::<bluer::Address>() {
            Ok(addr) => Ok(bluer::rfcomm::SocketAddr::new(addr, RFCOMM_CHANNEL)),
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
            tokio::spawn(dump_to_json_line_file(dump, rx));
        }
        if self.tls {
            builder.wrap(TlsClient::new(
                Arc::new(tls_client_config()),
                ServerName::try_from(TLS_SERVER_NAME).unwrap(),
            ));
        }

        let mut connector = builder.build();
        let mut targets = Vec::new();

        if !self.tcp.is_empty() {
            let mut tcp_connector =
                TcpConnector::new(self.tcp.clone(), TCP_PORT).await.context("cannot resolve TCP target")?;
            tcp_connector.set_ip_version(IpVersion::from_only(self.ipv4, self.ipv6)?);
            targets.push(tcp_connector.to_string());
            connector.add(tcp_connector);
        }

        #[cfg(feature = "rfcomm")]
        if let Some(addr) = self.rfcomm {
            let rfcomm_connector = RfcommConnector::new(addr);
            targets.push(addr.to_string());
            connector.add(rfcomm_connector);
        }

        #[cfg(feature = "rfcomm_profile")]
        if let Some(addr) = self.rfcomm_profile {
            let rfcomm_profile_connector = RfcommProfileConnector::new(addr, RFCOMM_UUID)
                .await
                .context("RFCOMM profile connector failed")?;
            targets.push(addr.to_string());
            connector.add(rfcomm_profile_connector);
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
        tokio::spawn(async move {
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
            tokio::spawn(async move {
                loop {
                    let (send, recv) = *speed_rx.borrow_and_update();
                    let speed = format!(
                        "{}{}\r\n{}{}\r\n",
                        "Upstream:   ".grey(),
                        format_speed(send),
                        "Downstream: ".grey(),
                        format_speed(recv)
                    );
                    let header = format!("{}\r\n\r\n{}", title.clone().white().bold(), speed);

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
            let task = tokio::spawn(speed_test);
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
                    control.terminated().await;
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
        control.terminated().await;
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
    #[cfg(feature = "rfcomm")]
    #[arg(long, default_value_t = RFCOMM_CHANNEL)]
    rfcomm: u8,
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
                tokio::spawn(dump_to_json_line_file(dump.clone(), rx));
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

        #[cfg(feature = "rfcomm")]
        match RfcommAcceptor::new(bluer::rfcomm::SocketAddr::new(bluer::Address::any(), self.rfcomm)).await {
            Ok(rfcomm) => {
                acceptor.add(rfcomm);
                ports.push(format!("RFCOMM channel {}", self.rfcomm));
            }
            Err(err) => eprintln!("Cannot listen on RFCOMM channel {}: {err}", self.rfcomm),
        }

        #[cfg(feature = "rfcomm_profile")]
        match RfcommProfileAcceptor::new(RFCOMM_UUID).await {
            Ok(rfcomm_profile) => {
                acceptor.add(rfcomm_profile);
                ports.push("RFCOMM profile".to_string());
            }
            Err(err) => eprintln!("Cannot listen on RFCOMM profile {RFCOMM_UUID}: {err}"),
        }

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

                tokio::spawn(async move {
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
            let task = tokio::spawn(task);

            let header_rx = watch::channel(format!("{title}\r\n").white().bold().to_string()).1;
            block_in_place(|| interactive_monitor(header_rx, control_rx, 1, None, Some(tag_error_rx), None))?;

            task.abort();
            if let Ok(res) = task.await {
                res?
            }
        }

        Ok(())
    }
}
