//! Tools for establishing connections consisting of aggregated TCP links.
//!
//! This module provides the simplest functions to establish outgoing or
//! accept incoming connections consisting of aggregated TCP links.
//!

use futures::Future;
use std::{
    io::{Error, ErrorKind, Result},
    net::SocketAddr,
};

use aggligator::{alc::Stream, cfg::Cfg, connect::Server};

use self::adv::{alc_connect, alc_listen, connect_links, tcp_listen, IpVersion, TargetSet};

pub mod adv;

/// Builds a connection consisting of aggregated TCP links to the target.
///
/// `cfg` is the configuration and in most cases `Cfg::default()` should be used.
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
/// use aggligator::cfg::Cfg;
/// use aggligator_util::net::tcp_connect;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let stream = tcp_connect(Cfg::default(), vec!["server".to_string()], 5900).await?;
///
///     // use the connection
///
///     Ok(())
/// }
/// ```
pub async fn tcp_connect(cfg: Cfg, target: Vec<String>, default_port: u16) -> Result<Stream> {
    let (outgoing, control) = alc_connect(cfg).await;

    let target = TargetSet::new(target, default_port, IpVersion::Both).await?;
    tokio::spawn(async move {
        if let Err(err) = connect_links(control, target).await {
            tracing::error!("connecting links failed: {err}")
        }
    });

    let ch = outgoing.connect().await.map_err(|err| Error::new(ErrorKind::TimedOut, err.to_string()))?;
    Ok(ch.into_stream())
}

/// Runs a TCP server accepting connections of aggregated links.
///
/// `cfg` is the configuration and in most cases `Cfg::default()` should be used.
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
/// use aggligator::cfg::Cfg;
/// use aggligator_util::net::tcp_server;
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     tcp_server(
///         Cfg::default(),
///         SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 5900),
///         |stream| async move {
///             // use the incoming connection
///         }
///     ).await;
///
///     Ok(())
/// }
/// ```
pub async fn tcp_server<F>(
    cfg: Cfg, addr: SocketAddr, work_fn: impl Fn(Stream) -> F + Send + 'static,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let server = Server::new(cfg);
    let listener = server.listen().await.unwrap();
    tokio::spawn(alc_listen(listener, work_fn));
    tcp_listen(server, addr).await
}
