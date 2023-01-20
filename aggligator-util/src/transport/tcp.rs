//! TCP transport.

use async_trait::async_trait;
use futures::{future, FutureExt};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::{lookup_host, TcpListener, TcpSocket},
    sync::{mpsc, watch},
    time::sleep,
};

use super::{AccepetedIoBox, AcceptingTransport, ConnectingTransport, IoBox, LinkTag, LinkTagBox};
use aggligator::{control::Direction, Link};

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
    pub interface: Vec<u8>,
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
        write!(f, "{:16} {dir} {}", String::from_utf8_lossy(&self.interface), self.remote)
    }
}

impl TcpLinkTag {
    /// Creates a new link tag for a TCP link.
    pub fn new(interface: &[u8], remote: SocketAddr, direction: Direction) -> Self {
        Self { interface: interface.to_vec(), remote, direction }
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
        self.interface.clone()
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

/// Gets the list of local network interfaces from the operating system.
///
/// Filters out interfaces that are most likely useless.
fn local_interfaces() -> Result<Vec<NetworkInterface>> {
    Ok(NetworkInterface::show()
        .map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?
        .into_iter()
        .filter(|iface| !iface.name.starts_with("ifb"))
        .collect())
}

/// TCP transport for outgoing connections.
#[derive(Debug, Clone)]
pub struct TcpConnector {
    hosts: Vec<String>,
    ip_version: IpVersion,
    resolve_interval: Duration,
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
        let mut hosts: Vec<_> = hosts.into_iter().collect();

        if hosts.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one host is required"));
        }

        for host in &mut hosts {
            if !host.contains(':') {
                host.push_str(&format!(":{}", default_port));
            }
        }

        let this = Self { hosts, ip_version: IpVersion::Both, resolve_interval: Duration::from_secs(10) };

        let addrs = this.resolve().await?;
        if addrs.is_empty() {
            return Err(Error::new(ErrorKind::NotFound, "cannot resolve IP address of host"));
        }
        tracing::info!("{} resolves to: {:?}", &this, addrs);

        Ok(this)
    }

    /// Sets the IP version used for connecting.
    pub fn set_ip_version(&mut self, ip_version: IpVersion) {
        self.ip_version = ip_version;
    }

    /// Sets the interval for re-resolving the hostname and checking for changed network interfaces.
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }

    /// Resolve target to socket addresses.
    async fn resolve(&self) -> Result<Vec<SocketAddr>> {
        let mut all_addrs = HashSet::new();

        for host in &self.hosts {
            all_addrs.extend(lookup_host(host).await?.filter(|addr| {
                !((addr.is_ipv4() && self.ip_version.is_only_ipv6())
                    || (addr.is_ipv6() && self.ip_version.is_only_ipv4()))
            }));
        }

        let mut all_addrs: Vec<_> = all_addrs.into_iter().collect();
        all_addrs.sort();

        Ok(all_addrs)
    }

    /// Returns the interface usable for connecting to target.
    ///
    /// Filters interfaces out that either have no IP address or only support
    /// an IP protocol version that does not match the target address.
    fn interface_names_for_target(interfaces: &[NetworkInterface], target: SocketAddr) -> HashSet<Vec<u8>> {
        interfaces
            .iter()
            .cloned()
            .filter_map(|iface| match &iface.addr {
                Some(addr) if addr.ip().is_unspecified() => None,
                Some(addr) if addr.ip().is_loopback() != target.ip().is_loopback() => None,
                Some(addr) if addr.ip().is_ipv4() && target.is_ipv4() => Some(iface.name.as_bytes().to_vec()),
                Some(addr) if addr.ip().is_ipv6() && target.is_ipv6() => Some(iface.name.as_bytes().to_vec()),
                _ => None,
            })
            .collect()
    }

    /// Binds the socket the the specifed network interface.
    fn bind_socket_to_interface(socket: &TcpSocket, interface: &[u8], remote: IpAddr) -> Result<()> {
        #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
        {
            let _ = remote;
            socket.bind_device(Some(interface))
        }

        #[cfg(not(any(target_os = "android", target_os = "fuchsia", target_os = "linux")))]
        {
            for ifn in local_interfaces()? {
                if ifn.name.as_bytes() == interface {
                    let Some(addr) = ifn.addr else { continue };
                    match (addr.ip(), remote) {
                        (IpAddr::V4(_), IpAddr::V4(_)) => (),
                        (IpAddr::V6(_), IpAddr::V6(_)) => (),
                        _ => continue,
                    }

                    if addr.ip().is_loopback() != remote.is_loopback() {
                        continue;
                    }

                    tracing::debug!("binding to {addr:?} on interface {}", &ifn.name);
                    socket.bind(SocketAddr::new(addr.ip(), 0))?;
                    return Ok(());
                }
            }

            Err(Error::new(ErrorKind::NotFound, "no IP address for interface"))
        }
    }
}

#[async_trait]
impl ConnectingTransport for TcpConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let interfaces = local_interfaces()?;

            let mut tags: HashSet<LinkTagBox> = HashSet::new();
            for addr in self.resolve().await? {
                for iface in Self::interface_names_for_target(&interfaces, addr) {
                    let tag = TcpLinkTag::new(&iface, addr, Direction::Outgoing);
                    tags.insert(Box::new(tag.clone()));
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

    async fn connect(&self, tag: &dyn LinkTag) -> Result<IoBox> {
        let tag: &TcpLinkTag = tag.as_any().downcast_ref().unwrap();

        let socket = match tag.remote.ip() {
            IpAddr::V4(_) => TcpSocket::new_v4(),
            IpAddr::V6(_) => TcpSocket::new_v6(),
        }?;

        Self::bind_socket_to_interface(&socket, &tag.interface, tag.remote.ip())?;

        let stream = socket.connect(tag.remote).await?;
        let _ = stream.set_nodelay(true);

        let (rh, wh) = stream.into_split();
        Ok(IoBox::new(rh, wh))
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
            String::from_utf8_lossy(&new_tag.interface)
        );

        match existing.iter().find(|link| {
            let Some(tag) = link.tag().as_any().downcast_ref::<TcpLinkTag>() else { return false };
            tag.interface == new_tag.interface && link.remote_user_data() == new.remote_user_data()
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
    /// Create a new TCP transport for incoming connections.
    ///
    /// It listens on the local addresses specified in `addrs`.
    pub async fn new(addrs: impl IntoIterator<Item = SocketAddr>) -> Result<Self> {
        let mut listeners = Vec::new();

        for addr in addrs {
            listeners.push(TcpListener::bind(addr).await?);
        }

        if listeners.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one listening address is required"));
        }

        Ok(Self { listeners })
    }
}

#[async_trait]
impl AcceptingTransport for TcpAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AccepetedIoBox>) -> Result<()> {
        loop {
            // Accept incoming connection.
            let (res, _, _) =
                future::select_all(self.listeners.iter().map(|listener| listener.accept().boxed())).await;
            let (socket, mut remote) = res?;
            let mut local = socket.local_addr()?;

            // Use proper IPv4 addresses.
            if let IpAddr::V6(addr) = remote.ip() {
                if let Some(addr) = addr.to_ipv4_mapped() {
                    remote.set_ip(addr.into());
                }
            }
            if let IpAddr::V6(addr) = local.ip() {
                if let Some(addr) = addr.to_ipv4_mapped() {
                    local.set_ip(addr.into());
                }
            }

            // Find local interface.
            let interfaces = local_interfaces()?;
            let Some(interface) = interfaces
                .into_iter()
                .find_map(|interface| {
                    interface
                        .addr
                        .map(|addr| addr.ip() == local.ip())
                        .unwrap_or_default()
                        .then_some(interface.name.into_bytes())
                })
            else {
                tracing::warn!("Interface for incoming connection from {remote} to {local} not found, rejecting.");
                continue;
            };

            // Build tag.
            tracing::debug!("Accepted TCP connection from {remote} on {}", String::from_utf8_lossy(&interface));
            let tag = TcpLinkTag { interface, remote, direction: Direction::Incoming };

            // Configure socket.
            let _ = socket.set_nodelay(true);
            let (rh, wh) = socket.into_split();

            let _ = tx.send(AccepetedIoBox::new(rh, wh, tag)).await;
        }
    }
}
