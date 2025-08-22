#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: TCP
//!
//! #### Simple aggregation of TCP links
//! Use the [tcp_connect](simple::tcp_connect) and [tcp_server](simple::tcp_server) functions
//! from the [simple module](simple).

use aggligator::io::{IoBox, StreamBox};
use async_trait::async_trait;
use futures::{future, FutureExt};
use network_interface::Addr;
use socket2::SockRef;
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpSocket},
    sync::{mpsc, watch},
    time::sleep,
};

use aggligator::{
    control::Direction,
    transport::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, LinkTag, LinkTagBox},
    Link,
};
use util::NetworkInterface;

pub mod simple;
pub mod util;

static NAME: &str = "tcp";

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
    pub fn from_only(only_ipv4: bool, only_ipv6: bool) -> Result<Self> {
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

/// Link tag for TCP link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TcpLinkTag {
    /// Local interface name.
    pub interface: Option<Vec<u8>>,
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
        write!(
            f,
            "{:16} {dir} {}",
            String::from_utf8_lossy(self.interface.as_deref().unwrap_or_default()),
            self.remote
        )
    }
}

impl TcpLinkTag {
    /// Creates a new link tag for a TCP link.
    pub fn new(interface: Option<&[u8]>, remote: SocketAddr, direction: Direction) -> Self {
        Self { interface: interface.map(|iface| iface.to_vec()), remote, direction }
    }
}

impl LinkTag for TcpLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        self.direction
    }

    fn user_data(&self) -> Vec<u8> {
        self.interface.clone().unwrap_or_default()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn box_clone(&self) -> LinkTagBox {
        Box::new(self.clone())
    }

    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }

    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

/// TCP link filter method.
///
/// Controls which links are established between the local and remote endpoint.
#[derive(Default, Debug, Clone, Copy)]
pub enum TcpLinkFilter {
    /// No link filtering.
    None,
    /// Filter based on local interface and remote interface.
    ///
    /// One link for each pair of local interface and remote interface is established.
    #[default]
    InterfaceInterface,
    /// Filter based on local interface and remote IP address.
    ///
    /// One link for each pair of local interface and remote IP address is established.
    InterfaceIp,
}

/// TCP transport for outgoing connections.
///
/// This transport is IO-stream based.
#[derive(Clone)]
pub struct TcpConnector {
    hosts: Vec<String>,
    ip_version: IpVersion,
    resolve_interval: Duration,
    link_filter: TcpLinkFilter,
    multi_interface: bool,
    interface_filter: Arc<dyn Fn(&NetworkInterface) -> bool + Send + Sync>,
}

impl fmt::Debug for TcpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TcpConnector")
            .field("hosts", &self.hosts)
            .field("ip_version", &self.ip_version)
            .field("resolve_interval", &self.resolve_interval)
            .field("link_filter", &self.link_filter)
            .field("multi_interface", &self.multi_interface)
            .finish()
    }
}

impl fmt::Display for TcpConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.hosts.len() > 1 {
            write!(f, "[{}]", self.hosts.join(", "))
        } else {
            write!(f, "{}", &self.hosts[0])
        }
    }
}

impl TcpConnector {
    /// Create a new TCP transport for outgoing connections.
    ///
    /// `hosts` can contain IP addresses and hostnames, including port numbers.
    /// If an entry does not specify a port number, the `default_port` is used.
    ///
    /// It is checked at creation that `hosts` resolves to at least one IP address.
    ///
    /// Host name resolution is retried periodically, thus DNS updates will be taken
    /// into account without the need to recreate this transport.
    pub async fn new(hosts: impl IntoIterator<Item = String>, default_port: u16) -> Result<Self> {
        let this = Self::unresolved(hosts, default_port).await?;

        let addrs = this.resolve().await;
        if addrs.is_empty() {
            return Err(Error::new(ErrorKind::NotFound, "cannot resolve IP address of host"));
        }
        tracing::info!(%this, ?addrs, "hosts initially resolved");

        Ok(this)
    }

    /// Create a new TCP transport for outgoing connections without checking that at least one host can be resolved.
    ///
    /// `hosts` can contain IP addresses and hostnames, including port numbers.
    /// If an entry does not specify a port number, the `default_port` is used.
    ///
    /// Host name resolution is retried periodically, thus DNS updates will be taken
    /// into account without the need to recreate this transport.
    pub async fn unresolved(hosts: impl IntoIterator<Item = String>, default_port: u16) -> Result<Self> {
        let mut hosts: Vec<_> = hosts.into_iter().collect();

        if hosts.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one host is required"));
        }

        for host in &mut hosts {
            if !host.contains(':') {
                host.push_str(&format!(":{default_port}"));
            }
        }

        Ok(Self {
            hosts,
            ip_version: IpVersion::Both,
            resolve_interval: Duration::from_secs(10),
            link_filter: TcpLinkFilter::default(),
            multi_interface: !cfg!(target_os = "android"),
            interface_filter: Arc::new(|_| true),
        })
    }

    /// Sets the IP version used for connecting.
    pub fn set_ip_version(&mut self, ip_version: IpVersion) {
        self.ip_version = ip_version;
    }

    /// Sets the interval for re-resolving the hostname and checking for changed network interfaces.
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }

    /// Sets the link filter method.
    pub fn set_link_filter(&mut self, link_filter: TcpLinkFilter) {
        self.link_filter = link_filter;
    }

    /// Sets whether all available local interfaces should be used for connecting.
    ///
    /// If this is true (default for non-Android platforms), a separate link is
    /// established for each pair of server IP and local interface. Each outgoing socket
    /// is explicitly bound to a local interface.
    ///
    /// If this is false (default for Android platform), one link is established for
    /// each server IP. The operating system automatically assigns a local interface
    /// for the outgoing socket.
    pub fn set_multi_interface(&mut self, multi_interface: bool) {
        self.multi_interface = multi_interface;
    }

    /// Sets the local interface filter.
    ///
    /// It is only used when multi interface is enabled.
    ///
    /// The provided function is called for each discoved local interface and should
    /// return whether the interface should be used for establishing links.
    ///
    /// By default all local interfaces are used.
    pub fn set_interface_filter(
        &mut self, interface_filter: impl Fn(&NetworkInterface) -> bool + Send + Sync + 'static,
    ) {
        self.interface_filter = Arc::new(interface_filter);
    }

    /// Resolve target to socket addresses.
    async fn resolve(&self) -> Vec<SocketAddr> {
        util::resolve_hosts(&self.hosts, self.ip_version).await
    }
}

#[async_trait]
impl ConnectingTransport for TcpConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let interfaces: Option<Vec<NetworkInterface>> = match self.multi_interface {
                true => Some(
                    util::local_interfaces()?
                        .into_iter()
                        .filter(|iface| (self.interface_filter)(&iface))
                        .collect(),
                ),
                false => None,
            };

            let mut tags: HashSet<LinkTagBox> = HashSet::new();
            for addr in self.resolve().await {
                match &interfaces {
                    Some(interfaces) => {
                        for iface in util::interface_names_for_target(interfaces, addr) {
                            let tag = TcpLinkTag::new(Some(&iface), addr, Direction::Outgoing);
                            tags.insert(Box::new(tag));
                        }
                    }
                    None => {
                        let tag = TcpLinkTag::new(None, addr, Direction::Outgoing);
                        tags.insert(Box::new(tag));
                    }
                }
            }

            tx.send_if_modified(|v| {
                if *v != tags {
                    *v = tags;
                    true
                } else {
                    false
                }
            });

            sleep(self.resolve_interval).await;
        }
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &TcpLinkTag = tag.as_any().downcast_ref().unwrap();

        let socket = match tag.remote.ip() {
            IpAddr::V4(_) => TcpSocket::new_v4(),
            IpAddr::V6(_) => TcpSocket::new_v6(),
        }?;

        if let Some(interface) = &tag.interface {
            util::bind_socket_to_interface(&socket, interface, tag.remote.ip())?;
        }

        let stream = socket.connect(tag.remote).await?;
        let _ = stream.set_nodelay(true);

        let (rh, wh) = stream.into_split();
        Ok(IoBox::new(rh, wh).into())
    }

    async fn link_filter(&self, new: &Link<LinkTagBox>, existing: &[Link<LinkTagBox>]) -> bool {
        let Some(new_tag) = new.tag().as_any().downcast_ref::<TcpLinkTag>() else { return true };

        let intro = format!(
            "Judging {} TCP link {} {} ({}) on {}",
            new.direction(),
            match new.direction() {
                Direction::Incoming => "from",
                Direction::Outgoing => "to",
            },
            new_tag.remote,
            String::from_utf8_lossy(new.remote_user_data()),
            String::from_utf8_lossy(new_tag.interface.as_deref().unwrap_or(b"any interface"))
        );

        match existing.iter().find(|link| {
            let Some(tag) = link.tag().as_any().downcast_ref::<TcpLinkTag>() else { return false };
            match self.link_filter {
                TcpLinkFilter::None => false,
                TcpLinkFilter::InterfaceInterface => {
                    tag.interface == new_tag.interface && link.remote_user_data() == new.remote_user_data()
                }
                TcpLinkFilter::InterfaceIp => {
                    tag.interface == new_tag.interface && tag.remote.ip() == new_tag.remote.ip()
                }
            }
        }) {
            Some(other) => {
                let other_tag = other.tag().as_any().downcast_ref::<TcpLinkTag>().unwrap();
                tracing::debug!("{intro} => link {} is redundant, rejecting.", other_tag.remote);
                false
            }
            None => {
                tracing::debug!("{intro} => accepted.");
                true
            }
        }
    }
}

/// TCP transport for incoming connections.
///
/// This transport is IO-stream based.
#[derive(Debug)]
pub struct TcpAcceptor {
    listeners: Vec<TcpListener>,
}

impl fmt::Display for TcpAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let addrs: Vec<_> = self
            .listeners
            .iter()
            .filter_map(|listener| listener.local_addr().ok().map(|addr| addr.to_string()))
            .collect();
        if addrs.len() > 1 {
            write!(f, "[{}]", addrs.join(", "))
        } else {
            write!(f, "{}", addrs[0])
        }
    }
}

impl TcpAcceptor {
    /// Create a new TCP transport listening for incoming connections.
    ///
    /// It listens on the local addresses specified in `addrs`.
    pub async fn new(addrs: impl IntoIterator<Item = SocketAddr>) -> Result<Self> {
        let mut listeners = Vec::new();

        for addr in addrs {
            let socket = match addr {
                SocketAddr::V4(_) => TcpSocket::new_v4()?,
                SocketAddr::V6(_) => {
                    let socket = TcpSocket::new_v6()?;
                    let _ = SockRef::from(&socket).set_only_v6(false);
                    socket
                }
            };
            socket.bind(addr)?;
            listeners.push(socket.listen(16)?);
        }

        Self::from_listeners(listeners)
    }

    /// Create a new TCP transport for incoming connections using the specified TCP listeners.
    pub fn from_listeners(listeners: impl IntoIterator<Item = TcpListener>) -> Result<Self> {
        let listeners: Vec<_> = listeners.into_iter().collect();

        if listeners.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one listener is required"));
        }

        Ok(Self { listeners: listeners.into_iter().collect() })
    }

    /// Create a new TCP transport for incoming connections, listening individually on all interfaces.
    ///
    /// On Linux each listener is specifically bound to a network interface. This may
    /// be necessary for the operating system to correctly enforce interface-specific
    /// traffic limits.
    ///
    /// In general the use of this function is not necessary.
    /// Prefer [`new`](Self::new) instead.
    pub async fn all_interfaces(port: u16) -> Result<Self> {
        let mut listeners = Vec::new();

        for iface in util::local_interfaces()? {
            for version in [IpVersion::IPv6, IpVersion::IPv4] {
                match Self::listen(&iface, port, version) {
                    Ok(listener) => listeners.push(listener),
                    Err(err) => {
                        tracing::warn!(interface =% iface.name, ip_version =? version, %err, "cannot listen");
                    }
                }
            }
        }

        Self::from_listeners(listeners)
    }

    fn listen(interface: &util::NetworkInterface, port: u16, ip_version: IpVersion) -> Result<TcpListener> {
        let ip = interface
            .addr
            .iter()
            .find_map(|addr| match addr {
                Addr::V4(ip) if ip_version != IpVersion::IPv6 => Some(IpAddr::V4(ip.ip)),
                Addr::V6(ip) if ip_version != IpVersion::IPv4 => Some(IpAddr::V6(ip.ip)),
                _ => None,
            })
            .ok_or(ErrorKind::NotFound)?;
        let addr = SocketAddr::new(ip, port);

        let socket = match addr.ip() {
            IpAddr::V4(_) => TcpSocket::new_v4()?,
            IpAddr::V6(_) => TcpSocket::new_v6()?,
        };

        if addr.is_ipv6() {
            let _ = SockRef::from(&socket).set_only_v6(false);
        }

        socket.bind(addr)?;

        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        socket.bind_device(Some(interface.name.as_bytes()))?;

        tracing::debug!(%addr, interface =% interface.name, "listening");

        socket.listen(8)
    }
}

#[async_trait]
impl AcceptingTransport for TcpAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        loop {
            // Accept incoming connection.
            let (res, _, _) =
                future::select_all(self.listeners.iter().map(|listener| listener.accept().boxed())).await;
            let (socket, mut remote) = res?;
            let mut local = socket.local_addr()?;

            // Use proper IPv4 addresses.
            util::use_proper_ipv4(&mut remote);
            util::use_proper_ipv4(&mut local);

            // Find local interface.
            let Some(interface) = util::local_interface_for_ip(local.ip())? else {
                tracing::warn!(
                    "Interface for incoming connection from {remote} to {local} not found, rejecting."
                );
                continue;
            };

            // Build tag.
            tracing::debug!(%remote, interface =% String::from_utf8_lossy(&interface), "Accepted TCP connection");
            let tag = TcpLinkTag::new(Some(&interface), remote, Direction::Incoming);

            // Configure socket.
            let _ = socket.set_nodelay(true);
            let (rh, wh) = socket.into_split();

            let _ = tx.send(AcceptedStreamBox::new(IoBox::new(rh, wh).into(), tag)).await;
        }
    }
}
