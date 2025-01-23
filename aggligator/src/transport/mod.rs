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

use std::{
    any::Any,
    cmp::Ordering,
    error::Error,
    fmt,
    fmt::{Debug, Display},
    hash::{Hash, Hasher},
    sync::Arc,
};

use crate::{
    control::Direction,
    id::ConnId,
    io::{RxBox, TxBox},
    Control, Link, Listener, Server, Task,
};

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

type BoxControl = Control<TxBox, RxBox, LinkTagBox>;
type BoxServer = Server<TxBox, RxBox, LinkTagBox>;
type BoxListener = Listener<TxBox, RxBox, LinkTagBox>;
type BoxTask = Task<TxBox, RxBox, LinkTagBox>;
type BoxLink = Link<LinkTagBox>;
type BoxLinkError = LinkError<LinkTagBox>;
