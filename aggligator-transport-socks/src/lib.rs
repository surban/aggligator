#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport through SOCKS5 proxies.

use aggligator::{
    control::Direction,
    io::{IoBox, StreamBox},
    transport::{ConnectingTransport, LinkTag, LinkTagBox},
};
use async_trait::async_trait;
use std::{
    any::Any,
    borrow::Cow,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{net::lookup_host, sync::watch, time::sleep};
use tokio_socks::{IntoTargetAddr, TargetAddr, tcp::Socks5Stream};

static NAME: &str = "socks5";

/// Port used when a proxy is specified without a port number.
pub const DEFAULT_PROXY_PORT: u16 = 1080;

/// Target for a SOCKS5 connection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Socks5Target {
    /// Target IP address.
    Ip(SocketAddr),
    /// Target domain name and port.
    Domain(String, u16),
}

impl fmt::Display for Socks5Target {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Ip(addr) => write!(f, "{addr}"),
            Self::Domain(domain, port) => write!(f, "{domain}:{port}"),
        }
    }
}

impl From<TargetAddr<'_>> for Socks5Target {
    fn from(target: TargetAddr<'_>) -> Self {
        match target {
            TargetAddr::Ip(addr) => Self::Ip(addr),
            TargetAddr::Domain(domain, port) => Self::Domain(domain.into_owned(), port),
        }
    }
}

impl IntoTargetAddr<'static> for Socks5Target {
    fn into_target_addr(self) -> tokio_socks::Result<TargetAddr<'static>> {
        Ok(match self {
            Self::Ip(addr) => TargetAddr::Ip(addr),
            Self::Domain(domain, port) => TargetAddr::Domain(Cow::Owned(domain), port),
        })
    }
}

/// Link tag for an outgoing SOCKS5 link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Socks5LinkTag {
    /// SOCKS5 proxy address.
    pub proxy: SocketAddr,
    /// Target address reached through the proxy.
    pub target: Socks5Target,
}

impl Socks5LinkTag {
    /// Creates a link tag for a SOCKS5 proxy and target.
    pub fn new(proxy: SocketAddr, target: Socks5Target) -> Self {
        Self { proxy, target }
    }
}

impl fmt::Display for Socks5LinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} -> {}", self.proxy, self.target)
    }
}

impl LinkTag for Socks5LinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        Direction::Outgoing
    }

    fn user_data(&self) -> Vec<u8> {
        self.proxy.to_string().into_bytes()
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

/// Adds the default proxy port to a proxy specification that does not contain a port number.
fn with_default_port(proxy: &str) -> String {
    if proxy.parse::<SocketAddr>().is_ok() {
        return proxy.to_string();
    }

    if let Ok(ip) = proxy.parse::<IpAddr>() {
        return SocketAddr::new(ip, DEFAULT_PROXY_PORT).to_string();
    }

    if proxy.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok()) {
        return proxy.to_string();
    }

    format!("{proxy}:{DEFAULT_PROXY_PORT}")
}

/// SOCKS5 transport for outgoing connections.
///
/// This transport establishes one IO-stream-based link through each configured proxy.
#[derive(Debug, Clone)]
pub struct Socks5Connector {
    proxies: Vec<String>,
    target: Socks5Target,
    resolve_interval: Duration,
}

impl fmt::Display for Socks5Connector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.proxies.len() > 1 {
            write!(f, "[{}] -> {}", self.proxies.join(", "), self.target)
        } else {
            write!(f, "{} -> {}", self.proxies[0], self.target)
        }
    }
}

impl Socks5Connector {
    /// Creates a SOCKS5 transport for a target through one or more proxies.
    ///
    /// The target hostname is resolved by each proxy.
    ///
    /// `proxies` can contain IP addresses and hostnames, including port numbers.
    /// If an entry does not specify a port number, [`DEFAULT_PROXY_PORT`] is used.
    /// Proxy hostnames are resolved locally and one link is established for each
    /// resolved IP address. Resolution is retried periodically, thus DNS updates
    /// will be taken into account without the need to recreate this transport.
    ///
    /// It is *not* checked at creation that the proxies can be resolved.
    ///
    /// Only a single link per proxy IP address is being used, even when the local and/or
    /// remote endpoints provide more than one network interface.
    pub fn new<'a>(
        proxies: impl IntoIterator<Item = impl AsRef<str>>, target: impl IntoTargetAddr<'a>,
    ) -> Result<Self> {
        let proxies: Vec<_> = proxies.into_iter().map(|proxy| with_default_port(proxy.as_ref())).collect();
        if proxies.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one proxy is required"));
        }

        let target = target.into_target_addr().map_err(|err| Error::new(ErrorKind::InvalidInput, err))?.into();

        Ok(Self { proxies, target, resolve_interval: Duration::from_secs(10) })
    }

    /// Sets the interval for re-resolving the proxy hostnames.
    pub fn set_resolve_interval(&mut self, resolve_interval: Duration) {
        self.resolve_interval = resolve_interval;
    }

    /// Resolves the proxies to socket addresses.
    async fn resolve(&self) -> Vec<SocketAddr> {
        let mut all_addrs = HashSet::new();

        for proxy in &self.proxies {
            match lookup_host(proxy).await {
                Ok(addrs) => all_addrs.extend(addrs),
                Err(err) => tracing::warn!(%proxy, %err, "cannot resolve SOCKS5 proxy"),
            }
        }

        let mut all_addrs: Vec<_> = all_addrs.into_iter().collect();
        all_addrs.sort();
        all_addrs
    }
}

#[async_trait]
impl ConnectingTransport for Socks5Connector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let tags = self
                .resolve()
                .await
                .into_iter()
                .map(|proxy| Box::new(Socks5LinkTag::new(proxy, self.target.clone())) as LinkTagBox)
                .collect();
            tx.send_replace(tags);

            sleep(self.resolve_interval).await;
        }
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &Socks5LinkTag = tag.as_any().downcast_ref().unwrap();
        let stream =
            Socks5Stream::connect(tag.proxy, tag.target.clone()).await.map_err(Error::other)?.into_inner();
        let _ = stream.set_nodelay(true);
        let (rh, wh) = stream.into_split();
        Ok(IoBox::new(rh, wh).into())
    }
}

#[cfg(test)]
mod tests;
