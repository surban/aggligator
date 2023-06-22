//! Connection and link management for various transports.
//!
//! This module provides automatic link management for an Aggligator connection.
//!
//! # Establishing outgoing connections
//!
//! The following example shows how to use a [`Connector`] to establish outgoing connections
//! using the TCP transport to connect to `server` on port 5900.
//!
//! ```no_run
//! use aggligator_util::transport::Connector;
//! use aggligator_util::transport::tcp::TcpConnector;
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let mut connector = Connector::new();
//!     connector.add(TcpConnector::new(["server".to_string()], 5900).await?);
//!     let ch = connector.channel().unwrap().await?;
//!     let stream = ch.into_stream();
//!
//!     // use the connection
//!
//!     Ok(())
//! }
//! ```
//!
//! # Accepting incoming connections
//!
//! The following example shows how to use an [`Acceptor`] to listen for incoming connections
//! using the TCP transport on port 5900.
//!
//! ```no_run
//! use std::net::{Ipv6Addr, SocketAddr};
//! use aggligator_util::transport::Acceptor;
//! use aggligator_util::transport::tcp::TcpAcceptor;
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let acceptor = Acceptor::new();
//!     acceptor.add(
//!         TcpAcceptor::new([SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 5900)]).await?
//!     );
//!
//!     loop {
//!         let (ch, _control) = acceptor.accept().await?;
//!         let stream = ch.into_stream();
//!
//!         // use the connection
//!     }
//!
//!     Ok(())
//! }
//!

use bytes::Bytes;
use futures::{Sink, SinkExt, Stream, StreamExt};
use std::{
    any::Any,
    cmp::Ordering,
    error::Error,
    fmt,
    fmt::{Debug, Display},
    hash::{Hash, Hasher},
    io,
    io::Result,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use aggligator::{control::Direction, id::ConnId, Control, IoRxBox, IoTxBox, Link, Listener, Server, Task};

mod acceptor;
mod connector;

pub use acceptor::*;
pub use connector::*;

/// Link error information.
#[derive(Clone, Debug)]
pub struct LinkError<TAG> {
    /// Connection id for outgoing links.
    pub id: Option<ConnId>,
    /// Link tag.
    pub tag: TAG,
    /// Error.
    pub error: Arc<std::io::Error>,
}

impl<TAG> LinkError<TAG>
where
    TAG: Clone,
{
    /// Creates new link tag error information for outgoing links.
    pub fn outgoing(id: ConnId, tag: &TAG, error: std::io::Error) -> Self {
        Self { id: Some(id), tag: tag.clone(), error: Arc::new(error) }
    }

    /// Creates new link tag error information for incoming links.
    pub fn incoming(tag: &TAG, error: std::io::Error) -> Self {
        Self { id: None, tag: tag.clone(), error: Arc::new(error) }
    }

    /// Direction of link on which the error occured.
    pub fn direction(&self) -> Direction {
        if self.id.is_some() {
            Direction::Outgoing
        } else {
            Direction::Incoming
        }
    }
}

impl<TAG> fmt::Display for LinkError<TAG>
where
    TAG: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", &self.tag, &self.error)
    }
}

impl<TAG> Error for LinkError<TAG> where TAG: fmt::Display + fmt::Debug {}

/// A tag for a link to a remote endpoint.
pub trait LinkTag: Debug + Display + Send + Sync + 'static {
    /// The name of the transport.
    fn transport_name(&self) -> &str;

    /// The direction of the link.
    fn direction(&self) -> Direction;

    /// User data to send to the remote endpoint when connecting.
    fn user_data(&self) -> Vec<u8>;

    /// Cast this type as [`Any`].
    fn as_any(&self) -> &dyn Any;

    /// Return a clone of this type in a [`Box`].
    fn box_clone(&self) -> LinkTagBox;

    /// Compare to another link tag of the same type.
    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering;

    /// Hash this link tag.
    fn dyn_hash(&self, state: &mut dyn Hasher);
}

impl PartialEq for dyn LinkTag {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for dyn LinkTag {}

impl PartialOrd for dyn LinkTag {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(Ord::cmp(self, other))
    }
}

impl Ord for dyn LinkTag {
    fn cmp(&self, other: &Self) -> Ordering {
        let id = self.as_any().type_id();
        let other_id = other.as_any().type_id();
        self.transport_name()
            .cmp(other.transport_name())
            .then(id.cmp(&other_id).then_with(|| self.dyn_cmp(other)))
    }
}

impl Hash for dyn LinkTag {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let id = self.as_any().type_id();
        id.hash(state);
        self.dyn_hash(state);
    }
}

/// A boxed [`LinkTag`].
pub type LinkTagBox = Box<dyn LinkTag>;

impl Clone for LinkTagBox {
    fn clone(&self) -> Self {
        self.box_clone()
    }
}

/// A stream, either packet-based or IO-based.
pub enum StreamBox {
    /// Packet-based stream.
    TxRx(TxRxBox),
    /// IO-based stream.
    Io(IoBox),
}

impl StreamBox {
    /// Make stream packet-based.
    ///
    /// A packet-based stream is unaffacted.
    /// An IO-based stream is wrapped in the integrity codec.
    pub fn into_tx_rx(self) -> TxRxBox {
        match self {
            Self::TxRx(tx_rx) => tx_rx,
            Self::Io(IoBox { read, write }) => {
                let tx = IoTxBox::new(write);
                let rx = IoRxBox::new(read);
                TxRxBox::new(tx, rx)
            }
        }
    }
}

impl From<TxRxBox> for StreamBox {
    fn from(value: TxRxBox) -> Self {
        Self::TxRx(value)
    }
}

impl From<IoBox> for StreamBox {
    fn from(value: IoBox) -> Self {
        Self::Io(value)
    }
}

type TxBox = Pin<Box<dyn Sink<Bytes, Error = io::Error> + Send + Sync + 'static>>;
type RxBox = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + Sync + 'static>>;

/// A boxed packet-based stream.
pub struct TxRxBox {
    /// Sender.
    pub tx: TxBox,
    /// Receiver.
    pub rx: RxBox,
}

impl TxRxBox {
    /// Creates a new instance.
    pub fn new(
        tx: impl Sink<Bytes, Error = io::Error> + Send + Sync + 'static,
        rx: impl Stream<Item = Result<Bytes>> + Send + Sync + 'static,
    ) -> Self {
        Self { tx: Box::pin(tx), rx: Box::pin(rx) }
    }

    /// Splits this into boxed transmitter and receiver.
    pub fn into_split(self) -> (TxBox, RxBox) {
        let Self { tx, rx } = self;
        (tx, rx)
    }
}

impl Sink<Bytes> for TxRxBox {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        self.get_mut().tx.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<()> {
        self.get_mut().tx.start_send_unpin(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        self.get_mut().tx.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        self.get_mut().tx.poll_close_unpin(cx)
    }
}

impl Stream for TxRxBox {
    type Item = Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_next_unpin(cx)
    }
}

type ReadBox = Pin<Box<dyn AsyncRead + Send + Sync + 'static>>;
type WriteBox = Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>;

/// A boxed IO stream.
pub struct IoBox {
    /// Reader.
    pub read: ReadBox,
    /// Writer.
    pub write: WriteBox,
}

impl IoBox {
    /// Creates a new instance.
    pub fn new(
        read: impl AsyncRead + Send + Sync + 'static, write: impl AsyncWrite + Send + Sync + 'static,
    ) -> Self {
        Self { read: Box::pin(read), write: Box::pin(write) }
    }

    /// Splits this into boxed reader and writer.
    pub fn into_split(self) -> (ReadBox, WriteBox) {
        let Self { read, write } = self;
        (read, write)
    }
}

impl AsyncRead for IoBox {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf) -> Poll<Result<()>> {
        Pin::new(&mut self.get_mut().read).poll_read(cx, buf)
    }
}

impl AsyncWrite for IoBox {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_shutdown(cx)
    }
}

type BoxControl = Control<TxBox, RxBox, LinkTagBox>;
type BoxServer = Server<TxBox, RxBox, LinkTagBox>;
type BoxListener = Listener<TxBox, RxBox, LinkTagBox>;
type BoxTask = Task<TxBox, RxBox, LinkTagBox>;
type BoxLink = Link<LinkTagBox>;
type BoxLinkError = LinkError<LinkTagBox>;

#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub mod tls;

#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
pub mod tcp;

#[cfg(feature = "rfcomm")]
#[cfg_attr(docsrs, doc(cfg(feature = "rfcomm")))]
pub mod rfcomm;

#[cfg(feature = "rfcomm-profile")]
#[cfg_attr(docsrs, doc(cfg(feature = "rfcomm-profile")))]
pub mod rfcomm_profile;

#[cfg(feature = "websocket")]
#[cfg_attr(docsrs, doc(cfg(feature = "websocket")))]
pub mod websocket;
