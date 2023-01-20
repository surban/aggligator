//! Tools for establishing connections consisting of aggregated TCP links,
//! optionally encrypted and authenticated using TLS.
//!
//! This module provides the simplest functions to establish outgoing or
//! accept incoming connections consisting of aggregated TCP links.
//!

#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
mod tls;
#[cfg(feature = "tls")]
#[cfg_attr(docsrs, doc(cfg(feature = "tls")))]
pub use tls::*;

use futures::Future;
use std::{io::Result, net::SocketAddr};

use crate::transport::{
    tcp::{TcpAcceptor, TcpConnector},
    Acceptor, Connector,
};
use aggligator::alc::Stream;

/// Builds a connection consisting of aggregated TCP links to the target.
///
/// `target` specifies a set of IP addresses or hostnames of the target host.
/// If a hostname resolves to multiple IP addresses this is taken into account
/// automatically.
/// If an entry in target specifies no port number, `default_port` is used.
///
/// Links are established automatically from all available local network interfaces
/// to all IP addresses of the target. If a link fails, it is reconnected
/// automatically.
///
/// Returns the connection stream.
///
/// # Example
/// This example connects to the host `server` on port 5900.
///
/// Multiple links will be used if the local machine has multiple interfaces
/// that can all connect to `server`, or `server` has multiple interfaces
/// that are registered with their IP addresses in DNS.
/// ```no_run
/// use aggligator_util::net::tcp_connect;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let stream = tcp_connect(["server".to_string()], 5900).await?;
///
///     // use the connection
///
///     Ok(())
/// }
/// ```
pub async fn tcp_connect(target: impl IntoIterator<Item = String>, default_port: u16) -> Result<Stream> {
    let mut connector = Connector::new();
    connector.add(TcpConnector::new(target, default_port).await?);
    let ch = connector.channel().unwrap().await?;
    Ok(ch.into_stream())
}

/// Runs a TCP server accepting connections of aggregated links.
///
/// The TCP server listens on `addr` and accepts connections of aggregated TCP links.
/// For each new connection the work function `work_fn` is spawned onto a new
/// Tokio task.
///
/// # Example
/// This example listens on all interfaces on port 5900.
///
/// If the server has multiple interfaces, all IP addresses should be registered
/// in DNS so that clients can discover them and establish multiple links.
/// ```no_run
/// use std::net::{Ipv6Addr, SocketAddr};
/// use aggligator_util::net::tcp_server;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     tcp_server(
///         SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 5900),
///         |stream| async move {
///             // use the incoming connection
///         }
///     ).await?;
///
///     Ok(())
/// }
/// ```
pub async fn tcp_server<F>(addr: SocketAddr, work_fn: impl Fn(Stream) -> F + Send + 'static) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let acceptor = Acceptor::new();
    acceptor.add(TcpAcceptor::new([addr]).await?);

    loop {
        let (ch, _control) = acceptor.accept().await?;
        tokio::spawn(work_fn(ch.into_stream()));
    }
}
