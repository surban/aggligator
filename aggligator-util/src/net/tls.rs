//! TLS connection functions.

use futures::Future;
use rustls::{ClientConfig, ServerConfig, ServerName};
use std::{
    io::{Error, ErrorKind, Result},
    net::SocketAddr,
    sync::Arc,
};

use aggligator::{alc::Stream, cfg::Cfg, connect::Server};

use super::adv::{alc_connect, alc_listen, tls_connect_links, tls_listen, IpVersion, TargetSet};

/// Builds a connection consisting of aggregated TCP links to the target,
/// which are encrypted and authenticated using TLS.
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
/// The identity of the server is verified using TLS against `domain`.
/// Each outgoing link is encrypted using TLS with the configuration specified
/// in `tls_client_cfg`.
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
/// use std::sync::Arc;
/// use aggligator::cfg::Cfg;
/// use aggligator_util::net::tls_connect;
/// use rustls::{ClientConfig, RootCertStore, ServerName};
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let server_name = "agl.server.net";
///     // TODO: set server_name
///
///     let mut root_store = RootCertStore::empty();
///     // TODO: add certificates to the root_store
///
///     let tls_cfg = Arc::new(
///         ClientConfig::builder()
///             .with_safe_defaults()
///             .with_root_certificates(root_store)
///             .with_no_client_auth()
///     );
///
///     let stream = tls_connect(
///         Cfg::default(),
///         vec![server_name.to_string()],
///         5900,
///         ServerName::try_from(server_name).unwrap(),
///         tls_cfg,
///     ).await?;
///
///     // use the connection
///
///     Ok(())
/// }
/// ```
pub async fn tls_connect(
    cfg: Cfg, target: Vec<String>, default_port: u16, domain: ServerName, tls_client_cfg: Arc<ClientConfig>,
) -> Result<Stream> {
    let (outgoing, control) = alc_connect(cfg).await;

    let target = TargetSet::new(target, default_port, IpVersion::Both).await?;
    tokio::spawn(async move {
        if let Err(err) = tls_connect_links(control, target, domain, tls_client_cfg).await {
            tracing::error!("connecting links failed: {err}")
        }
    });

    let ch = outgoing.connect().await.map_err(|err| Error::new(ErrorKind::TimedOut, err.to_string()))?;
    Ok(ch.into_stream())
}

/// Runs a TCP server accepting connections of aggregated links,
/// which are encrypted and authenticated using TLS.
///
/// `cfg` is the configuration and in most cases `Cfg::default()` should be used.
///
/// The TCP server listens on `addr` and accepts connections of aggregated TCP links.
/// For each new connection the work function `work_fn` is spawned onto a new
/// Tokio task.
///
/// Each incoming link is encrypted using TLS with the configuration specified
/// in `tls_server_cfg`.
///
/// # Example
/// This example listens on all interfaces on port 5900.
///
/// If the server has multiple interfaces, all IP addresses should be registered
/// in DNS so that clients can discover them and establish multiple links.
/// ```no_run
/// use std::net::{Ipv6Addr, SocketAddr};
/// use std::sync::Arc;
/// use aggligator::cfg::Cfg;
/// use aggligator_util::net::tls_server;
/// use rustls::{ServerConfig};
///
/// #[tokio::main]
/// async fn main() -> std::io::Result<()> {
///     let tls_certs = todo!("load certificate tree");
///     let tls_key = todo!("load private key");
///
///     let tls_cfg = Arc::new(
///         ServerConfig::builder()
///             .with_safe_defaults()
///             .with_no_client_auth()
///             .with_single_cert(tls_certs, tls_key)
///             .unwrap()
///     );
///
///     tls_server(
///         Cfg::default(),
///         SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 5900),
///         tls_cfg,
///         |stream| async move {
///             // use the incoming connection
///         }
///     ).await?;
///
///     Ok(())
/// }
/// ```
pub async fn tls_server<F>(
    cfg: Cfg, addr: SocketAddr, tls_server_cfg: Arc<ServerConfig>, work_fn: impl Fn(Stream) -> F + Send + 'static,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let server = Server::new(cfg);
    let listener = server.listen().await.unwrap();
    tokio::spawn(alc_listen(listener, work_fn));
    tls_listen(server, addr, tls_server_cfg).await
}
