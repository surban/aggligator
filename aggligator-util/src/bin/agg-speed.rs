//! Aggligator speed test.

use aggligator_util::net::adv::{tls_connect_links_and_monitor, tls_listen};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{style::Stylize, tty::IsTty};
use rustls::{
    client::{DangerousClientConfig, ServerCertVerified, ServerCertVerifier},
    Certificate, ClientConfig, PrivateKey, RootCertStore, ServerConfig, ServerName,
};
use rustls_pemfile::{certs, pkcs8_private_keys};
use serde::Serialize;
use std::{
    io::{stdout, BufReader},
    net::{Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::exit,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::block_in_place,
};

use aggligator::{cfg::Cfg, connect::Server};
use aggligator_util::{
    cli::{init_log, load_cfg, print_default_cfg},
    monitor::{format_speed, interactive_monitor},
    net::adv::{
        alc_connect_and_dump, alc_listen_and_monitor, tcp_connect_links_and_monitor, tcp_listen, IpVersion,
        TargetSet,
    },
    speed::{speed_test, INTERVAL},
};

const PORT: u16 = 5700;

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
    DangerousClientConfig { cfg: &mut cfg }.set_certificate_verifier(Arc::new(TlsNullVerifier));
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
    let cfg = load_cfg(&cli.cfg)?;

    match SpeedCli::parse().command {
        Commands::Client(client) => client.run(cfg).await?,
        Commands::Server(server) => server.run(cfg).await?,
        Commands::ShowCfg => print_default_cfg(),
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
    /// Dump analysis data to file.
    #[arg(long, short = 'd')]
    dump: Option<PathBuf>,
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
    /// Server name or IP addresses and port number.
    #[arg(required = true)]
    target: Vec<String>,
}

impl ClientCli {
    pub async fn run(mut self, cfg: Cfg) -> Result<()> {
        if !stdout().is_tty() {
            self.no_monitor = true;
        }

        let target = TargetSet::new(self.target.clone(), PORT, IpVersion::from_args(self.ipv4, self.ipv6)?)
            .await
            .context("cannot resolve target")?;
        let title = format!("Speed test against {target} {}", if self.tls { "with TLS" } else { "" });

        let (outgoing, control) = alc_connect_and_dump(cfg, self.dump).await;

        let (control_tx, control_rx) = mpsc::channel(1);
        let (tags_tx, tags_rx) = watch::channel(Default::default());
        let (tag_err_tx, tag_err_rx) = mpsc::channel(8);
        let (disabled_ifs_tx, disabled_tags_rx) = watch::channel(Default::default());
        let (header_tx, header_rx) = watch::channel(Default::default());
        let (speed_tx, mut speed_rx) = watch::channel(Default::default());

        let _ = control_tx.send((control.clone(), String::new())).await;
        drop(control_tx);

        let links_target = target.clone();
        let tls = self.tls;
        let connect_control = control.clone();
        tokio::spawn(async move {
            let res = if tls {
                tls_connect_links_and_monitor(
                    connect_control,
                    links_target,
                    ServerName::try_from(TLS_SERVER_NAME).unwrap(),
                    Arc::new(tls_client_config()),
                    tags_tx,
                    tag_err_tx,
                    disabled_tags_rx,
                )
                .await
            } else {
                tcp_connect_links_and_monitor(
                    connect_control,
                    links_target,
                    tags_tx,
                    tag_err_tx,
                    disabled_tags_rx,
                )
                .await
            };

            if let Err(err) = res {
                eprintln!("Connecting links failed: {err}");
                exit(10);
            }
        });

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
            let ch = outgoing.connect().await.context("cannot establish aggligator connection")?;
            let (r, w) = ch.into_stream().into_split();
            anyhow::Ok(
                speed_test(
                    &target.to_string(),
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
                    self.all_links.then_some(disabled_ifs_tx),
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
    /// Dump analysis data to file.
    #[arg(long, short = 'd')]
    dump: Option<PathBuf>,
    /// Do not display the link monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Exit after handling one connection.
    #[arg(long)]
    oneshot: bool,
    /// Encrypt all links using TLS.
    #[arg(long)]
    tls: bool,
    /// TCP port.
    #[arg(default_value_t = PORT)]
    port: u16,
}

impl ServerCli {
    pub async fn run(mut self, cfg: Cfg) -> Result<()> {
        if !stdout().is_tty() {
            self.no_monitor = true;
        }

        let title = format!(
            "Speed test server listening on port {} {}",
            self.port,
            if self.tls { "with TLS" } else { "" }
        );

        let server = Server::new(cfg);
        let listener = server.listen().await?;

        let tls = self.tls;
        let task = async move {
            if tls {
                tls_listen(
                    server,
                    SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), self.port),
                    Arc::new(tls_server_config()),
                )
                .await
            } else {
                tcp_listen(server, SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), self.port)).await
            }
        };

        let oneshot = self.oneshot;
        if self.no_monitor {
            tokio::spawn(alc_listen_and_monitor(
                listener,
                move |ch| async move {
                    let id = ch.id();
                    let (r, w) = ch.into_split();
                    let res =
                        speed_test(&id.to_string(), r, w, None, None, true, true, false, INTERVAL, None).await;
                    if oneshot {
                        exit(res.is_err() as _);
                    }
                },
                mpsc::channel(1).0,
                self.dump.clone(),
            ));
            task.await?
        } else {
            let (control_tx, control_rx) = mpsc::channel(8);
            tokio::spawn(alc_listen_and_monitor(
                listener,
                move |ch| async move {
                    let id = ch.id();
                    let (r, w) = ch.into_split();
                    let (tx, _rx) = watch::channel(Default::default());
                    let res =
                        speed_test(&id.to_string(), r, w, None, None, true, true, false, INTERVAL, Some(tx))
                            .await;
                    if oneshot {
                        exit(res.is_err() as _);
                    }
                },
                control_tx,
                self.dump.clone(),
            ));
            let task = tokio::spawn(task);

            let header_rx = watch::channel(format!("{title}\r\n").white().bold().to_string()).1;
            block_in_place(|| interactive_monitor(header_rx, control_rx, 1, None, None, None))?;

            task.abort();
            if let Ok(res) = task.await {
                res?
            }
        }

        Ok(())
    }
}
