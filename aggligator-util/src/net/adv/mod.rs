//! Advanced tools for working with connections consisting of aggregated TCP links.
//!
//! # Example
//! See the source of the `agg-speed` and `agg-tunnel` tools on how to use
//! these functions.
//!

#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
mod tls;
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub use tls::*;

use aggligator::{
    alc::Stream,
    cfg::Cfg,
    connect::{connect, Outgoing},
    control::{Direction, Link},
    dump::dump_to_json_line_file,
};
use futures::Future;
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use std::{
    collections::HashSet,
    fmt,
    future::IntoFuture,
    io::{Error, ErrorKind, Result},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{lookup_host, TcpListener, TcpSocket, TcpStream},
    sync::{mpsc, watch},
    time::{sleep, timeout},
};

use aggligator::{
    connect::{Listener, Server},
    control::Control,
    io::{IoRx, IoRxBox, IoTx, IoTxBox},
};

use crate::TagError;

/// Timeout for establishing a TCP connection.
pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval for retrying failed TCP links.
pub const LINK_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// Number of analysis dump events to buffer.
pub const DUMP_BUFFER: usize = 8192;

/// IP protocol version.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IpVersion {
    /// IP version 4.
    IPv4,
    /// IP version 6.
    IPv6,
    /// Both IP versions.
    #[default]
    Both,
}

impl IpVersion {
    /// Create from "only" arguments.
    pub fn from_args(only_ipv4: bool, only_ipv6: bool) -> Result<Self> {
        match (only_ipv4, only_ipv6) {
            (false, false) => Ok(Self::Both),
            (true, false) => Ok(Self::IPv4),
            (false, true) => Ok(Self::IPv6),
            (true, true) => {
                Err(Error::new(ErrorKind::InvalidInput, "IPv4 and IPv6 options are mutally exclusive"))
            }
        }
    }

    /// Whether only IPv4 should be supported.
    pub fn is_only_ipv4(&self) -> bool {
        matches!(self, Self::IPv4)
    }

    /// Whether only IPv6 should be supported.
    pub fn is_only_ipv6(&self) -> bool {
        matches!(self, Self::IPv6)
    }
}

/// Target hosts or IPs.
///
/// It can consist of one or more target hostnames or IP addresses.
#[derive(Debug, Clone)]
pub struct TargetSet {
    targets: Vec<String>,
    version: IpVersion,
}

impl fmt::Display for TargetSet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.targets.len() > 1 {
            write!(f, "[{}]", self.targets.join(", "))
        } else {
            write!(f, "{}", &self.targets[0])
        }
    }
}

impl TargetSet {
    /// Create a new target set and check that it is resolvable to at least one IP address.
    ///
    /// If a target does not specify a port number, the `default_port` is used.
    pub async fn new(mut targets: Vec<String>, default_port: u16, version: IpVersion) -> Result<Self> {
        if targets.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one target is required"));
        }

        for target in &mut targets {
            if !target.contains(':') {
                target.push_str(&format!(":{}", default_port));
            }
        }

        let this = Self { targets, version };

        let addrs = this.resolve().await?;
        tracing::info!("{} resolves to: {:?}", &this, addrs);

        Ok(this)
    }

    /// Resolve to socket addresses.
    pub async fn resolve(&self) -> Result<Vec<SocketAddr>> {
        let mut all_addrs = HashSet::new();

        for target in &self.targets {
            all_addrs.extend(lookup_host(target).await?.filter(|addr| {
                !((addr.is_ipv4() && self.version.is_only_ipv6())
                    || (addr.is_ipv6() && self.version.is_only_ipv4()))
            }));
        }

        let mut all_addrs: Vec<_> = all_addrs.into_iter().collect();
        all_addrs.sort();

        if all_addrs.is_empty() {
            return Err(Error::new(ErrorKind::NotFound, "cannot resolve IP address of target"));
        }

        Ok(all_addrs)
    }
}

/// Link tag for TCP link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TcpLinkTag {
    /// Local interface name.
    pub interface: String,
    /// Remote address.
    pub remote: SocketAddr,
    /// Link direction.
    pub direction: Direction,
}

impl fmt::Display for TcpLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dir = match self.direction {
            Direction::Incoming => "<-",
            Direction::Outgoing => "->",
        };
        write!(f, "{:16} {dir} {}", &self.interface, self.remote)
    }
}

impl TcpLinkTag {
    /// Creates a new link tag for a TCP link.
    pub fn new(interface: &str, remote: SocketAddr, direction: Direction) -> Self {
        Self { interface: interface.to_string(), remote, direction }
    }
}

/// Gets the list of local network interfaces from the operating system.
///
/// Filters out interfaces that are most likely useless.
pub fn local_interfaces() -> Result<Vec<NetworkInterface>> {
    Ok(NetworkInterface::show()
        .map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?
        .into_iter()
        .filter(|iface| !iface.name.starts_with("ifb"))
        .collect())
}

/// Returns the interface usable for connecting to target.
///
/// Filters interfaces out that either have no IP address or only support
/// an IP protocol version that does not match the target address.
pub fn interface_names_for_target(interfaces: &[NetworkInterface], target: SocketAddr) -> HashSet<String> {
    interfaces
        .iter()
        .cloned()
        .filter_map(|iface| match &iface.addr {
            Some(addr) if addr.ip().is_unspecified() => None,
            Some(addr) if addr.ip().is_loopback() != target.ip().is_loopback() => None,
            Some(addr) if addr.ip().is_ipv4() && target.is_ipv4() => Some(iface.name),
            Some(addr) if addr.ip().is_ipv6() && target.is_ipv6() => Some(iface.name),
            _ => None,
        })
        .collect()
}

/// Periodically sends tags of potential links for connecting to target over the provided channel.
///
/// This function monitors the locally available network interfaces and IP addresses that
/// `target` resolves to and generates a set of link tags for connecting to `target`.
/// This set is sent over `tags_tx`.
///
/// The function does not ensure that a link could indeed be established; it just generates
/// a set of potentially possible links.
pub async fn monitor_potential_link_tags(
    target: TargetSet, tags_tx: watch::Sender<Vec<TcpLinkTag>>,
) -> Result<()> {
    loop {
        let interfaces = local_interfaces()?;
        let mut tags = Vec::new();

        for target in target.resolve().await? {
            for iface in interface_names_for_target(&interfaces, target) {
                let tag = TcpLinkTag::new(&iface, target, Direction::Outgoing);
                tags.push(tag.clone());
            }
        }

        tags.sort();
        if tags_tx.send(tags).is_err() {
            break;
        }

        sleep(LINK_RETRY_INTERVAL).await;
    }

    Ok(())
}

/// Connects links of the aggregated connection to the target via all possible local interfaces
/// and target's IP addresses using TCP.
///
/// This function monitors the locally available network interfaces and IP addresses that
/// `target` resolves to and tries to establish a link from every local interface to every
/// IP address of the target. If the link is successfully established, it is added to the
/// connection specified via `control`.
///
/// Links that cannot be established or that fail are retried periodically.
///
/// The function returns when the connection is terminated.
pub async fn tcp_connect_links(control: Control<IoTxBox, IoRxBox, TcpLinkTag>, target: TargetSet) -> Result<()> {
    tcp_connect_links_and_monitor(
        control,
        target,
        watch::channel(Default::default()).0,
        mpsc::channel(1).0,
        watch::channel(Default::default()).1,
    )
    .await
}

/// Connects links of the aggregated connection to the target via all possible local interfaces
/// and target's IP addresses using TCP, wrapping each outgoing stream through a function.
///
/// This function monitors the locally available network interfaces and IP addresses that
/// `target` resolves to and tries to establish a link from every local interface to every
/// IP address of the target. If the link is successfully established, it is added to the
/// connection specified via `control`.
///
/// Links that cannot be established or that fail are retried periodically.
///
/// Each outgoing connection is wrapped through `wrap_fn`, which can, for example,
/// set up TLS encryption or perform some other form of authentication before
/// the link is used.
///
/// The function returns when the connection is terminated.
pub async fn tcp_connect_links_wrapped<R, W, F, Fut>(
    control: Control<IoTx<W>, IoRx<R>, TcpLinkTag>, target: TargetSet, wrap_fn: F,
) -> Result<()>
where
    F: FnOnce(TcpStream) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Result<(R, W)>> + Send,
    R: AsyncRead + Send + Sync + Unpin + 'static,
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    tcp_connect_links_and_monitor_wrapped(
        control,
        target,
        wrap_fn,
        watch::channel(Default::default()).0,
        mpsc::channel(1).0,
        watch::channel(Default::default()).1,
    )
    .await
}

/// Boxes a TCP stream so that it can be used with IoTxBox and IoRxBox.
async fn box_tcp_stream(
    stream: TcpStream,
) -> Result<(Pin<Box<dyn AsyncRead + Send + Sync + 'static>>, Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>)> {
    let (r, w) = stream.into_split();
    Ok((Box::pin(r), Box::pin(w)))
}

/// Connects links of the aggregated connection to the target via all possible local interfaces
/// and target's IP addresses using TCP, providing an additional interface for the interactive monitor.
///
/// This function monitors the locally available network interfaces and IP addresses that
/// `target` resolves to and tries to establish a link from every local interface to every
/// IP address of the target. If the link is successfully established, it is added to the
/// connection specified via `control`.
///
/// Links that cannot be established or that fail are retried periodically.
///
/// Additionally the following arguments are useful when using the [interactive monitor](crate::monitor):
///   * potential links are sent via the channel `tags_tx`,
///   * errors that occur on individual links are sent via the channel `tag_error_tx`,
///   * the channel `disabled_tags_rx` is used to receive a set of links that should
///     not be used.
///
/// The function returns when the connection is terminated.
pub async fn tcp_connect_links_and_monitor(
    control: Control<IoTxBox, IoRxBox, TcpLinkTag>, target: TargetSet, tags_tx: watch::Sender<Vec<TcpLinkTag>>,
    tag_err_tx: mpsc::Sender<TagError<TcpLinkTag>>, disabled_tags_rx: watch::Receiver<HashSet<TcpLinkTag>>,
) -> Result<()> {
    tcp_connect_links_and_monitor_wrapped(control, target, box_tcp_stream, tags_tx, tag_err_tx, disabled_tags_rx)
        .await
}

/// Connects links of the aggregated connection to the target via all possible local interfaces
/// and target's IP addresses using TCP and wrapping each outgoing stream through a function,
/// and providing an additional interface for the interactive monitor.
///
/// This function monitors the locally available network interfaces and IP addresses that
/// `target` resolves to and tries to establish a link from every local interface to every
/// IP address of the target. If the link is successfully established, it is added to the
/// connection specified via `control`.
///
/// Links that cannot be established or that fail are retried periodically.
///
/// Each outgoing connection is wrapped through `wrap_fn`, which can, for example,
/// set up TLS encryption or perform some other form of authentication before
/// the link is used.
///
/// Additionally the following arguments are useful when using the [interactive monitor](crate::monitor):
///   * potential links are sent via the channel `tags_tx`,
///   * errors that occur on individual links are sent via the channel `tag_error_tx`,
///   * the channel `disabled_tags_rx` is used to receive a set of links that should
///     not be used.
///
/// The function returns when the connection is terminated.
pub async fn tcp_connect_links_and_monitor_wrapped<R, W, F, Fut>(
    control: Control<IoTx<W>, IoRx<R>, TcpLinkTag>, target: TargetSet, wrap_fn: F,
    tags_tx: watch::Sender<Vec<TcpLinkTag>>, tag_err_tx: mpsc::Sender<TagError<TcpLinkTag>>,
    mut disabled_tags_rx: watch::Receiver<HashSet<TcpLinkTag>>,
) -> Result<()>
where
    F: FnOnce(TcpStream) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Result<(R, W)>> + Send,
    R: AsyncRead + Send + Sync + Unpin + 'static,
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    async fn do_connect(iface: &[u8], ifaces: &[NetworkInterface], target: SocketAddr) -> Result<TcpStream> {
        let socket = match target.ip() {
            IpAddr::V4(_) => TcpSocket::new_v4(),
            IpAddr::V6(_) => TcpSocket::new_v6(),
        }?;

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        socket.bind_device(Some(iface))?;
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        let _ = ifaces;

        #[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
        {
            let mut bound = false;

            for ifn in ifaces {
                if ifn.name.as_bytes() == iface {
                    let Some(addr) = ifn.addr else { continue };
                    match (addr.ip(), target.ip()) {
                        (IpAddr::V4(_), IpAddr::V4(_)) => (),
                        (IpAddr::V6(_), IpAddr::V6(_)) => (),
                        _ => continue,
                    }

                    if addr.ip().is_loopback() != target.ip().is_loopback() {
                        continue;
                    }

                    tracing::debug!("binding to {addr:?} on interface {}", &ifn.name);
                    socket.bind(SocketAddr::new(addr.ip(), 0))?;
                    bound = true;
                    break;
                }
            }

            if !bound {
                return Err(Error::new(ErrorKind::NotFound, "no IP address for interface"));
            }
        }

        socket.connect(target).await
    }

    let mut running = HashSet::new();
    let (disconnected_tx, mut disconnected_rx) = mpsc::channel(16);

    while !control.is_terminated() {
        tracing::debug!("Trying to establish new links...");

        let interfaces = local_interfaces()?;
        let disabled = disabled_tags_rx.borrow_and_update().clone();
        let mut tags = Vec::new();

        while let Ok(tag) = disconnected_rx.try_recv() {
            running.remove(&tag);
        }

        match target.resolve().await {
            Ok(targets) => {
                for target in targets {
                    for iface in interface_names_for_target(&interfaces, target) {
                        let tag = TcpLinkTag::new(&iface, target, Direction::Outgoing);
                        tags.push(tag.clone());

                        if running.contains(&tag) || disabled.contains(&tag) {
                            continue;
                        }
                        running.insert(tag.clone());

                        let control = control.clone();
                        let interfaces = interfaces.clone();
                        let disconnected_tx = disconnected_tx.clone();
                        let tag_err_tx = tag_err_tx.clone();
                        let wrap_fn = wrap_fn.clone();
                        tokio::spawn(async move {
                            tracing::debug!("Trying TCP connection for {tag}");

                            match timeout(TCP_CONNECT_TIMEOUT, do_connect(iface.as_bytes(), &interfaces, target))
                                .await
                            {
                                Ok(Ok(strm)) => {
                                    tracing::debug!("TCP connection established for {tag}");

                                    // Wrap socket.
                                    match timeout(TCP_CONNECT_TIMEOUT, wrap_fn(strm)).await {
                                        Ok(Ok((read, write))) => {
                                            match control.add_io(read, write, tag.clone(), iface.as_bytes()).await
                                            {
                                                Ok(link) => {
                                                    tracing::info!("Link established for {tag}");

                                                    let reason = link.disconnected().await;
                                                    tracing::warn!("Link for {tag} disconnected: {reason}");

                                                    let _ = tag_err_tx
                                                        .send(TagError::new(control.id(), link.tag(), reason))
                                                        .await;
                                                }
                                                Err(err) => {
                                                    tracing::warn!("Establishing link for {tag} failed: {err}");
                                                    let _ = tag_err_tx
                                                        .send(TagError::new(control.id(), &tag, err))
                                                        .await;
                                                }
                                            }
                                        }
                                        Ok(Err(err)) => {
                                            tracing::warn!("Wrapping connection to {tag} failed: {err}");
                                            let _ = tag_err_tx.send(TagError::new(control.id(), &tag, err)).await;
                                        }
                                        Err(_) => {
                                            tracing::warn!("Wrapping connection to {tag} timed out");
                                            let _ = tag_err_tx
                                                .send(TagError::new(control.id(), &tag, "wrapping timeout"))
                                                .await;
                                        }
                                    }
                                }
                                Ok(Err(err)) => {
                                    tracing::warn!("TCP connection for {tag} failed: {}", &err);
                                    let _ = tag_err_tx.send(TagError::new(control.id(), &tag, err)).await;
                                }
                                Err(_) => {
                                    tracing::warn!("TCP connection for {tag} timed out");
                                    let _ = tag_err_tx
                                        .send(TagError::new(control.id(), &tag, "TCP connect timeout"))
                                        .await;
                                }
                            }

                            let _ = disconnected_tx.send(tag.clone()).await;
                        });
                    }
                }
            }
            Err(err) => tracing::warn!("cannot lookup target: {err}"),
        }

        tags.sort();
        tags_tx.send_replace(tags);

        sleep(LINK_RETRY_INTERVAL).await;
    }

    Ok(())
}

/// Listens for incoming connections using aggregated TCP links.
///
/// This function accepts incoming ALC connections from `listener`, configures
/// the link filter on each incoming connection to reject redundant links
/// (between the same pair of local and remote interfaces) and calls `work_fn`
/// on each established connection.
#[allow(clippy::type_complexity)]
pub async fn alc_listen<R, W, F>(
    listener: Listener<IoTx<W>, IoRx<R>, TcpLinkTag>, work_fn: impl Fn(Stream) -> F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
    R: AsyncRead + Send + Sync + Unpin + 'static,
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    alc_listen_and_monitor(listener, work_fn, mpsc::channel(1).0, None).await
}

/// Listens for incoming connections using aggregated TCP links,
/// providing an additional interface for the interactive monitor.
///
/// This function accepts incoming ALC connections from `listener`, configures
/// the link filter on each incoming connection to reject redundant links
/// (between the same pair of local and remote interfaces) and calls `work_fn`
/// on each established connection.
///
/// Additionally the following arguments are useful when using the [interactive monitor](crate::monitor):
///   * a control handle of each incoming connections is sent over the channel `control_tx`,
///   * if `dump` is sent, analysis data is saved to the specified file.
#[allow(clippy::type_complexity)]
pub async fn alc_listen_and_monitor<R, W, F>(
    mut listener: Listener<IoTx<W>, IoRx<R>, TcpLinkTag>, work_fn: impl Fn(Stream) -> F,
    control_tx: mpsc::Sender<(Control<IoTx<W>, IoRx<R>, TcpLinkTag>, String)>, dump: Option<PathBuf>,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
    R: AsyncRead + Send + Sync + Unpin + 'static,
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    loop {
        let mut inc = listener.next().await.map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?;
        let id = inc.id();
        tracing::info!("Accepting incoming connection {id} consisting of links {:?}", inc.link_tags());

        let (mut task, ch, mut control) = inc.accept().await;

        #[cfg(feature = "dump")]
        if let Some(dump) = &dump {
            let (tx, rx) = mpsc::channel(DUMP_BUFFER);
            task.dump(tx);
            tokio::spawn(aggligator::dump::dump_to_json_line_file(dump.clone(), rx));
        }
        #[cfg(not(feature = "dump"))]
        let _ = dump;

        task.set_link_filter(tcp_link_filter);
        tokio::spawn(task.into_future());

        if control_tx.is_closed() {
            tokio::spawn(async move {
                while !control.is_terminated() {
                    tracing::info!("Connection {id} now consists of links {:?}", control.links());
                    control.links_changed().await;
                }

                tracing::info!("Connection {id} terminated");
            });
        } else {
            let _ = control_tx.send((control, String::new())).await;
        }

        let f = work_fn(ch.into_stream());
        tokio::spawn(async move {
            f.await;
            tracing::info!("Incoming connection {id} done");
        });
    }
}

/// Starts establishing an outgoing connection consisting of aggregated TCP links.
///
/// Uses the specified configuration `cfg`.
pub async fn alc_connect<R, W>(cfg: Cfg) -> (Outgoing, Control<IoTx<W>, IoRx<R>, TcpLinkTag>)
where
    R: AsyncRead + Send + Sync + Unpin + 'static,
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    alc_connect_and_dump(cfg, None::<PathBuf>).await
}

/// Starts establishing an outgoing connection consisting of aggregated TCP links,
/// dumping analysis data to a file.
///
/// Uses the specified configuration `cfg`.
/// If `dump` is sent, analysis data is saved to the specified file.
pub async fn alc_connect_and_dump<R, W>(
    cfg: Cfg, dump: Option<impl AsRef<Path>>,
) -> (Outgoing, Control<IoTx<W>, IoRx<R>, TcpLinkTag>)
where
    R: AsyncRead + Send + Sync + Unpin + 'static,
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    let (mut task, outgoing, control) = connect(cfg);

    if let Some(dump) = &dump {
        let (tx, rx) = mpsc::channel(DUMP_BUFFER);
        task.dump(tx);
        let dump = dump.as_ref().to_path_buf();
        tokio::spawn(dump_to_json_line_file(dump, rx));
    }

    tokio::spawn(task.into_future());

    (outgoing, control)
}

/// Link filter that only allows one TCP link between each pair
/// of local and remote interfaces.
pub async fn tcp_link_filter(new: Link<TcpLinkTag>, existing: Vec<Link<TcpLinkTag>>) -> bool {
    let intro = format!(
        "Judging {} link {} {} ({}) on {}",
        new.direction(),
        match new.direction() {
            Direction::Incoming => "from",
            Direction::Outgoing => "to",
        },
        new.tag().remote,
        String::from_utf8_lossy(new.remote_user_data()),
        &new.tag().interface
    );

    match existing.into_iter().find(|link| {
        link.tag().interface == new.tag().interface && link.remote_user_data() == new.remote_user_data()
    }) {
        Some(other) => {
            tracing::debug!("{intro} => link {} is redundant, rejecting.", other.tag().remote);
            false
        }
        None => {
            tracing::debug!("{intro} => accepted.");
            true
        }
    }
}

/// TCP listener for link aggregator.
///
/// Listens on `addr` and accepts incoming TCP connections, which are then
/// forwarded to the specified link aggregator server.
pub async fn tcp_listen(server: Server<IoTxBox, IoRxBox, TcpLinkTag>, addr: SocketAddr) -> Result<()> {
    tcp_listen_wrapped(server, addr, box_tcp_stream).await
}

/// TCP listener for link aggregator that wraps each incoming stream through a function.
///
/// Listens on `addr` and accepts incoming TCP connections, which are then
/// forwarded to the specified link aggregator server.
///
/// Each incoming connection is wrapped through `wrap_fn`, which can, for example,
/// set up TLS encryption or perform some other form of authentication before
/// the link is used.
pub async fn tcp_listen_wrapped<R, W, F, Fut>(
    server: Server<IoTx<W>, IoRx<R>, TcpLinkTag>, addr: SocketAddr, wrap_fn: F,
) -> Result<()>
where
    F: FnOnce(TcpStream) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = Result<(R, W)>> + Send,
    R: AsyncRead + Send + Sync + Unpin + 'static,
    W: AsyncWrite + Send + Sync + Unpin + 'static,
{
    let tcp_listener = TcpListener::bind(addr).await?;
    tracing::info!("Listening on {}", addr);

    loop {
        let (socket, mut src) = tcp_listener.accept().await?;
        let mut local_addr = socket.local_addr()?;

        // Display proper IPv4 addresses.
        if let IpAddr::V6(addr) = src.ip() {
            if let Some(addr) = addr.to_ipv4_mapped() {
                src.set_ip(addr.into());
            }
        }
        if let IpAddr::V6(addr) = local_addr.ip() {
            if let Some(addr) = addr.to_ipv4_mapped() {
                local_addr.set_ip(addr.into());
            }
        }

        // Find local interface.
        let interfaces = local_interfaces()?;
        let Some(interface) = interfaces
            .into_iter()
            .find_map(|interface| {
                interface
                    .addr
                    .map(|addr| addr.ip() == local_addr.ip())
                    .unwrap_or_default()
                    .then_some(interface.name)
            })
        else {
            tracing::warn!("Interface for incoming connection from {src} to {local_addr} not found, rejecting.");
            continue;
        };
        tracing::debug!("Accepted TCP connection from {src} on {interface}");

        let tag = TcpLinkTag { interface: interface.clone(), remote: src, direction: Direction::Incoming };
        let server = server.clone();
        let wrap_fn = wrap_fn.clone();

        tokio::spawn(async move {
            // Wrap socket.
            let (read, write) = match wrap_fn(socket).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!("Wrapping connection from {src} failed: {err}");
                    return;
                }
            };

            // Hand to Aggligator server.
            match server.add_incoming_io(read, write, tag, interface.as_bytes()).await {
                Ok(link) => {
                    tracing::info!("Added incoming link: {link:?}");
                    tokio::spawn(async move {
                        let reason = link.disconnected().await;
                        tracing::warn!("Incoming link {src} disconnected: {reason:?}");
                    });
                }
                Err(err) => {
                    tracing::warn!("Adding incoming link failed: {err}");
                }
            }
        });
    }
}
