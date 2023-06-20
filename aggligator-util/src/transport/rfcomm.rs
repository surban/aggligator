//! Bluetooth RFCOMM transport.

use async_trait::async_trait;
use bluer::rfcomm::{Listener, Socket, SocketAddr};
use futures::future;
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::Result,
};
use tokio::sync::{mpsc, watch};

use super::{AcceptedStreamBox, AcceptingTransport, ConnectingTransport, IoBox, LinkTag, LinkTagBox, StreamBox};
use aggligator::control::Direction;

static NAME: &str = "rfcomm";

/// Link tag for Bluetooth RFCOMM link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RfcommLinkTag {
    /// Local RFCOMM address.
    pub local: SocketAddr,
    /// Remote RFCOMM address.
    pub remote: SocketAddr,
    /// Link direction.
    pub direction: Direction,
}

impl fmt::Display for RfcommLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dir = match self.direction {
            Direction::Incoming => "<-",
            Direction::Outgoing => "->",
        };
        write!(f, "{} {dir} {}", self.local, self.remote)
    }
}

impl RfcommLinkTag {
    /// Creates a new link tag for a Bluetooth RFCOMM link.
    pub fn new(local: SocketAddr, remote: SocketAddr, direction: Direction) -> Self {
        Self { local, remote, direction }
    }
}

impl LinkTag for RfcommLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        self.direction
    }

    fn user_data(&self) -> Vec<u8> {
        Vec::new()
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

/// Bluetooth RFCOMM transport for outgoing connections.
///
/// This transport is IO-stream based.
#[derive(Debug, Clone)]
pub struct RfcommConnector {
    local: SocketAddr,
    remote: SocketAddr,
}

impl RfcommConnector {
    /// Creates a new Bluetooth RFCOMM transport for RFCOMM connections.
    ///
    /// The transport establishes one connection to the specified RFCOMM socket address.
    pub fn new(remote: SocketAddr) -> Self {
        Self { local: SocketAddr::any(), remote }
    }

    /// Binds the outgoing socket to the given local Bluetooth address.
    pub fn bind(&mut self, local: SocketAddr) {
        self.local = local;
    }
}

#[async_trait]
impl ConnectingTransport for RfcommConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        let tag = RfcommLinkTag::new(self.local, self.remote, Direction::Outgoing);
        tx.send_replace([Box::new(tag) as Box<dyn LinkTag>].into_iter().collect());
        future::pending().await
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &RfcommLinkTag = tag.as_any().downcast_ref().unwrap();

        let socket = Socket::new()?;
        socket.bind(tag.local)?;
        let stream = socket.connect(tag.remote).await?;

        let (rh, wh) = stream.into_split();
        Ok(IoBox::new(rh, wh).into())
    }
}

/// Bluetooth RFCOMM transport for incoming connection.
///
/// This transport is IO-stream based.
#[derive(Debug)]
pub struct RfcommAcceptor {
    listener: Listener,
}

impl RfcommAcceptor {
    /// Creates a new Bluetooth RFCOMM transport for incoming connections.
    ///
    /// It listens on the specified RFCOMM socket address.
    pub async fn new(addr: SocketAddr) -> Result<Self> {
        let listener = Listener::bind(addr).await?;
        Ok(Self { listener })
    }

    /// The local RFCOMM socket address used for listening.
    pub fn address(&self) -> Result<SocketAddr> {
        self.listener.as_ref().local_addr()
    }
}

#[async_trait]
impl AcceptingTransport for RfcommAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        loop {
            let (socket, remote) = self.listener.accept().await?;
            let local = socket.as_ref().local_addr()?;

            tracing::debug!("Accepted RFCOMM connection from {remote} on {local}");
            let tag = RfcommLinkTag::new(local, remote, Direction::Incoming);

            let (rh, wh) = socket.into_split();
            let _ = tx.send(AcceptedStreamBox::new(IoBox::new(rh, wh).into(), tag)).await;
        }
    }
}
