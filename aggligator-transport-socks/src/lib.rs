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
    fmt, future,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::SocketAddr,
};
use tokio::sync::watch;
use tokio_socks::{IntoTargetAddr, TargetAddr, tcp::Socks5Stream};

static NAME: &str = "socks5";

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

/// SOCKS5 transport for outgoing connections.
///
/// This transport establishes one IO-stream-based link through each configured proxy.
#[derive(Debug, Clone)]
pub struct Socks5Connector {
    proxies: Vec<SocketAddr>,
    target: Socks5Target,
}

impl fmt::Display for Socks5Connector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let proxies: Vec<_> = self.proxies.iter().map(ToString::to_string).collect();
        if proxies.len() > 1 {
            write!(f, "[{}] -> {}", proxies.join(", "), self.target)
        } else {
            write!(f, "{} -> {}", proxies[0], self.target)
        }
    }
}

impl Socks5Connector {
    /// Creates a SOCKS5 transport for a target through one or more proxies.
    ///
    /// The target hostname is resolved by each proxy.
    /// Proxies must be provided as socket addresses.
    ///
    /// Only a single link per proxy is being used, even when the local and/or remote
    /// endpoints provide more than one network interface.
    pub fn new<'a>(
        proxies: impl IntoIterator<Item = SocketAddr>, target: impl IntoTargetAddr<'a>,
    ) -> Result<Self> {
        let proxies: Vec<_> = proxies.into_iter().collect();
        if proxies.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one proxy is required"));
        }

        let target = target.into_target_addr().map_err(|err| Error::new(ErrorKind::InvalidInput, err))?.into();

        Ok(Self { proxies, target })
    }
}

#[async_trait]
impl ConnectingTransport for Socks5Connector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        let tags = self
            .proxies
            .iter()
            .map(|proxy| Box::new(Socks5LinkTag::new(*proxy, self.target.clone())) as LinkTagBox)
            .collect();
        tx.send_replace(tags);
        future::pending().await
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
