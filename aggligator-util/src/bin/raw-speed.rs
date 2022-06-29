//! Raw connections for comparison of performance.

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use crossterm::{
    cursor::{MoveTo, MoveToNextLine},
    event::{poll, read, Event, KeyCode, KeyEvent},
    execute,
    style::{Print, Stylize},
    terminal,
    terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType},
    tty::IsTty,
};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use std::{
    collections::{HashMap, HashSet},
    io::stdout,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::{lookup_host, TcpListener, TcpSocket, TcpStream},
    sync::{mpsc, mpsc::error::TryRecvError, watch},
    task::block_in_place,
    time::{sleep, timeout},
};

use aggligator_util::{cli::init_log, monitor::format_speed, speed, speed::INTERVAL};

const PORT: u16 = 5701;
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Run speed test using a separate TCP connection for each interface.
///
/// Useful for comparing performance to an aggregated connection using
/// the `agg-speed` tool.
#[derive(Parser)]
#[command(author, version)]
pub struct RawSpeedCli {
    /// Client or server.
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Raw speed test client.
    Client(RawClientCli),
    /// Raw speed test server.
    Server(RawServerCli),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_log();
    match RawSpeedCli::parse().command {
        Commands::Client(client) => client.run().await,
        Commands::Server(server) => server.run().await,
    }
}

#[derive(Args)]
pub struct RawClientCli {
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
    /// Do not display the monitor.
    #[arg(long, short = 'n')]
    no_monitor: bool,
    /// Server name or IP address and port number.
    target: String,
}

impl RawClientCli {
    async fn resolve_target(&self) -> Result<SocketAddr> {
        if self.ipv4 && self.ipv6 {
            return Err(anyhow!("IPv4 and IPv6 options are mutually exclusive"));
        }

        let mut target = self.target.clone();
        if !target.contains(':') {
            target.push_str(&format!(":{PORT}"));
        }

        for addr in lookup_host(&target).await? {
            if (addr.is_ipv4() && self.ipv6) || (addr.is_ipv6() && self.ipv4) {
                continue;
            }

            return Ok(addr);
        }

        Err(anyhow!("cannot resolve IP address of target"))
    }

    async fn tcp_connect(iface: &[u8], target: SocketAddr) -> Result<TcpStream> {
        let socket = match target.ip() {
            IpAddr::V4(_) => TcpSocket::new_v4(),
            IpAddr::V6(_) => TcpSocket::new_v6(),
        }
        .context("cannot create socket")?;
        socket.bind_device(Some(iface)).context("cannot bind to interface")?;
        Ok(socket.connect(target).await?)
    }

    #[allow(clippy::type_complexity)]
    async fn test_links(
        target: SocketAddr, send_only: bool, recv_only: bool, limit: Option<usize>, time: Option<Duration>,
        speeds_tx: Option<mpsc::Sender<(String, Option<(f64, f64)>)>>,
    ) -> Result<()> {
        let mut connected = HashSet::new();
        let (disconnected_tx, mut disconnected_rx) = mpsc::channel(16);

        while !speeds_tx.as_ref().map(|tx| tx.is_closed()).unwrap_or_default() {
            while let Ok(iface) = disconnected_rx.try_recv() {
                connected.remove(&iface);
            }

            let interfaces = NetworkInterface::show().context("cannot get network interfaces")?;
            let iface_names: HashSet<_> = interfaces.into_iter().map(|iface| iface.name).collect();

            for iface in iface_names {
                if connected.contains(&iface) {
                    continue;
                }
                connected.insert(iface.clone());

                let iface_disconnected_tx = disconnected_tx.clone();
                let iface_speeds_tx = speeds_tx.clone();
                tokio::spawn(async move {
                    if iface_speeds_tx.is_none() {
                        eprintln!("Trying TCP connection from {iface}");
                    }

                    match timeout(TCP_CONNECT_TIMEOUT, Self::tcp_connect(iface.as_bytes(), target)).await {
                        Ok(Ok(strm)) => {
                            if iface_speeds_tx.is_none() {
                                eprintln!("TCP connection established from {iface}");
                            }

                            let (read, write) = strm.into_split();
                            let task_iface = iface.clone();

                            let speed_tx = match iface_speeds_tx.clone() {
                                Some(iface_speeds_tx) => {
                                    let iface = iface.clone();
                                    let (tx, mut rx) = watch::channel(Default::default());
                                    tokio::spawn(async move {
                                        while let Ok(()) = rx.changed().await {
                                            let speed = *rx.borrow_and_update();
                                            if iface_speeds_tx.send((iface.clone(), Some(speed))).await.is_err() {
                                                break;
                                            }
                                        }
                                        let _ = iface_speeds_tx.send((iface.clone(), None)).await;
                                    });
                                    Some(tx)
                                }
                                None => None,
                            };

                            let _ = speed::speed_test(
                                &iface, read, write, limit, time, !recv_only, !send_only, false, INTERVAL,
                                speed_tx,
                            )
                            .await;

                            if iface_speeds_tx.is_none() {
                                eprintln!("TCP connection from {task_iface} done");
                            }
                        }
                        Ok(Err(err)) => {
                            if iface_speeds_tx.is_none() {
                                eprintln!("TCP connection from {iface} failed: {}", &err);
                            }
                        }
                        Err(_) => {
                            if iface_speeds_tx.is_none() {
                                eprintln!("TCP connection from {iface} timed out");
                            }
                        }
                    }
                    if iface_speeds_tx.is_none() {
                        eprintln!();
                    }

                    let _ = iface_disconnected_tx.send(iface).await;
                });
            }

            sleep(Duration::from_secs(3)).await;
        }

        Ok(())
    }

    fn monitor(header: &str, mut speeds_rx: mpsc::Receiver<(String, Option<(f64, f64)>)>) -> Result<()> {
        enable_raw_mode()?;

        let mut speeds = HashMap::new();

        'main: loop {
            loop {
                match speeds_rx.try_recv() {
                    Ok((iface, Some(speed))) => {
                        speeds.insert(iface, speed);
                    }
                    Ok((iface, None)) => {
                        speeds.remove(&iface);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break 'main,
                }
            }

            let (_cols, rows) = terminal::size().unwrap();
            execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0)).unwrap();
            execute!(stdout(), Print(header.white().bold()), MoveToNextLine(2)).unwrap();
            execute!(stdout(), Print("                       TX             RX    ".grey()), MoveToNextLine(1))
                .unwrap();

            let mut total_tx = 0.;
            let mut total_rx = 0.;
            for (tx, rx) in speeds.values() {
                total_tx += *tx;
                total_rx += *rx;
            }
            execute!(
                stdout(),
                Print("Total               ".grey()),
                Print(format_speed(total_tx)),
                Print("    "),
                Print(format_speed(total_rx)),
                MoveToNextLine(2),
            )
            .unwrap();

            let mut speeds: Vec<_> = speeds.clone().into_iter().collect();
            speeds.sort_by_key(|(iface, _)| iface.clone());
            for (iface, (tx, rx)) in speeds {
                execute!(
                    stdout(),
                    Print(format!("{:20}", iface).cyan()),
                    Print(format_speed(tx)),
                    Print("    "),
                    Print(format_speed(rx)),
                    MoveToNextLine(1),
                )
                .unwrap();
            }

            execute!(
                stdout(),
                MoveTo(0, rows - 2),
                Print("Press q to quit.".to_string().grey()),
                MoveToNextLine(1)
            )
            .unwrap();

            if poll(Duration::from_secs(1))? {
                if let Event::Key(KeyEvent { code: KeyCode::Char('q'), .. }) = read()? {
                    break;
                }
            }
        }

        disable_raw_mode()?;
        Ok(())
    }

    pub async fn run(mut self) -> Result<()> {
        if !stdout().is_tty() {
            self.no_monitor = true;
        }

        let target = self.resolve_target().await.context("cannot resolve target")?;
        let header = format!("Connecting to raw speed test server at {}", &target);

        let limit = self.limit.map(|mb| mb * 1_048_576);
        let time = self.time.map(Duration::from_secs);

        if self.no_monitor {
            eprintln!("{}", header);
            Self::test_links(
                target,
                self.send_only,
                self.recv_only,
                self.limit,
                self.time.map(Duration::from_secs),
                None,
            )
            .await?;
        } else {
            let (speeds_tx, speeds_rx) = mpsc::channel(16);
            tokio::spawn(Self::test_links(target, self.send_only, self.recv_only, limit, time, Some(speeds_tx)));
            block_in_place(|| Self::monitor(&header, speeds_rx))?;
        }

        Ok(())
    }
}

#[derive(Args)]
pub struct RawServerCli {
    /// TCP port.
    #[arg(default_value_t = PORT)]
    port: u16,
}

impl RawServerCli {
    async fn tcp_serve(addr: SocketAddr) -> Result<()> {
        let tcp_listener = TcpListener::bind(addr).await.context("cannot listen")?;
        eprintln!("Raw speed test server listening on {}\n", addr);

        loop {
            let (socket, src) = tcp_listener.accept().await?;
            eprintln!("Accepted TCP connection from {src}");

            let (read, write) = socket.into_split();
            tokio::spawn(async move {
                let _ = speed::speed_test(
                    &src.to_string(),
                    read,
                    write,
                    None,
                    None,
                    true,
                    true,
                    false,
                    INTERVAL,
                    None,
                )
                .await;
                eprintln!("TCP connection from {src} done");
            });
        }
    }

    pub async fn run(self) -> Result<()> {
        Self::tcp_serve(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), self.port)).await
    }
}
