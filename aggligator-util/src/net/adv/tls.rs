//! TLS connection functions.

use rustls::{ClientConfig, ServerConfig, ServerName};
use std::{collections::HashSet, io::Result, net::SocketAddr, pin::Pin, sync::Arc};
use tokio::{
    io::{split, AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::{mpsc, watch},
};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use aggligator::{connect::Server, control::Control};

use super::{
    tcp_connect_links_and_monitor_wrapped, tcp_connect_links_wrapped, tcp_listen_wrapped, IoRxBox, IoTxBox,
    TargetSet, TcpLinkTag,
};
use crate::TagError;

/// Boxes a TLS stream so that it can be used with IoTxBox and IoRxBox.
#[allow(clippy::type_complexity)]
fn box_tls_stream(
    stream: TlsStream<TcpStream>,
) -> (Pin<Box<dyn AsyncRead + Send + Sync + 'static>>, Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>) {
    let (r, w) = split(stream);
    (Box::pin(r), Box::pin(w))
}

/// Connects links of the aggregated connection to the target via all possible local interfaces
/// and target's IP addresses using TLS over TCP.
///
/// This function monitors the locally available network interfaces and IP addresses that
/// `target` resolves to and tries to establish a link from every local interface to every
/// IP address of the target. If the link is successfully established, it is added to the
/// connection specified via `control`.
///
/// Links that cannot be established or that fail are retried periodically.
///
/// The identity of the server is verified using TLS against `domain`.
/// Each outgoing link is encrypted using TLS with the configuration specified
/// in `tls_client_cfg`.
///
/// The function returns when the connection is terminated.
pub async fn tls_connect_links(
    control: Control<IoTxBox, IoRxBox, TcpLinkTag>, target: TargetSet, domain: ServerName,
    tls_client_cfg: Arc<ClientConfig>,
) -> Result<()> {
    let connector = TlsConnector::from(tls_client_cfg);
    let wrap_fn = |stream| async move {
        let tls = connector.connect(domain, stream).await?;
        Ok(box_tls_stream(TlsStream::from(tls)))
    };

    tcp_connect_links_wrapped(control, target, wrap_fn).await
}

/// Connects links of the aggregated connection to the target via all possible local interfaces
/// and target's IP addresses using TLS over TCP, providing an additional interface for the interactive monitor.
///
/// This function monitors the locally available network interfaces and IP addresses that
/// `target` resolves to and tries to establish a link from every local interface to every
/// IP address of the target. If the link is successfully established, it is added to the
/// connection specified via `control`.
///
/// Links that cannot be established or that fail are retried periodically.
///
/// The identity of the server is verified using TLS against `domain`.
/// Each outgoing link is encrypted using TLS with the configuration specified
/// in `tls_client_cfg`.
///
/// Additionally the following arguments are useful when using the [interactive monitor](crate::monitor):
///   * potential links are sent via the channel `tags_tx`,
///   * errors that occur on individual links are sent via the channel `tag_error_tx`,
///   * the channel `disabled_tags_rx` is used to receive a set of links that should
///     not be used.
///
/// The function returns when the connection is terminated.
pub async fn tls_connect_links_and_monitor(
    control: Control<IoTxBox, IoRxBox, TcpLinkTag>, target: TargetSet, domain: ServerName,
    tls_client_cfg: Arc<ClientConfig>, tags_tx: watch::Sender<Vec<TcpLinkTag>>,
    tag_err_tx: mpsc::Sender<TagError<TcpLinkTag>>, disabled_tags_rx: watch::Receiver<HashSet<TcpLinkTag>>,
) -> Result<()> {
    let connector = TlsConnector::from(tls_client_cfg);
    let wrap_fn = |stream| async move {
        let tls = connector.connect(domain, stream).await?;
        Ok(box_tls_stream(TlsStream::from(tls)))
    };

    tcp_connect_links_and_monitor_wrapped(control, target, wrap_fn, tags_tx, tag_err_tx, disabled_tags_rx).await
}

/// TLS over TCP listener for link aggregator.
///
/// Listens on `addr` and accepts incoming TCP connections, which are then
/// forwarded to the specified link aggregator server.
///
/// Each incoming link is encrypted using TLS with the configuration specified
/// in `tls_server_cfg`.
pub async fn tls_listen(
    server: Server<IoTxBox, IoRxBox, TcpLinkTag>, addr: SocketAddr, tls_server_cfg: Arc<ServerConfig>,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(tls_server_cfg);
    let wrap_fn = |stream| async move {
        let tls = acceptor.accept(stream).await?;
        Ok(box_tls_stream(TlsStream::from(tls)))
    };

    tcp_listen_wrapped(server, addr, wrap_fn).await
}
