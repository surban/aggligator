//! WebSocket transport.

use async_trait::async_trait;
use axum::{
    body::boxed,
    extract::{ConnectInfo, WebSocketUpgrade},
    http::StatusCode,
    response::Response,
    routing::get,
    Router,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt, TryStreamExt};
use std::{
    any::Any,
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    net::{IpAddr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::TcpSocket,
    sync::{mpsc, watch, Mutex},
    time::sleep,
};
use tokio_tungstenite::{client_async_tls_with_config, tungstenite::protocol::WebSocketConfig, Connector};
use url::Url;

use super::{
    tcp::{
        bind_socket_to_interface, interface_names_for_target, local_interfaces, resolve_hosts, use_proper_ipv4,
        IpVersion,
    },
    AcceptedStreamBox, AcceptingTransport, ConnectingTransport, LinkTag, LinkTagBox, StreamBox, TxRxBox,
};
use aggligator::{control::Direction, Link};

static NAME: &str = "websocket";

/// Link tag for outgoing WebSocket link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutgoingWebSocketLinkTag {
    /// Local interface name.
    pub interface: Vec<u8>,
    /// Remote socket address.
    pub remote: SocketAddr,
    /// Remote URL.
    pub url: String,
    /// Whether to use TLS for connecting.
    pub tls: bool,
}

impl fmt::Display for OutgoingWebSocketLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} -> {} ({})", String::from_utf8_lossy(&self.interface), &self.remote, &self.url)
    }
}

impl LinkTag for OutgoingWebSocketLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        Direction::Outgoing
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

/// WebSocket transport for outgoing connections.
///
/// This transport is packet-based.
#[derive(Clone)]
pub struct WebSocketConnector {
    urls: Vec<Url>,
    ip_version: IpVersion,
    resolve_interval: Duration,
    connector: Option<Connector>,
    web_socket_config: Option<WebSocketConfig>,
}

impl fmt::Debug for WebSocketConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WebSocketConnector")
            .field("urls", &self.urls)
            .field("ip_version", &self.ip_version)
            .field("resolve_interval", &self.resolve_interval)
            .field("web_socket_config", &self.web_socket_config)
            .finish()
    }
}

impl fmt::Display for WebSocketConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let urls: Vec<_> = self.urls.iter().map(|url| url.to_string()).collect();
        if self.urls.len() > 1 {
            write!(f, "[{}]", urls.join(", "))
        } else {
            write!(f, "{}", &urls[0])
        }
    }
}

impl WebSocketConnector {
    /// Create a new WebSocket transport for outgoing connections.
    ///
    /// `urls` contains one or more WebSocket URLs of the target.
    ///
    /// It is checked at creation that at least one URL can be resolved to an IP address.
    ///
    /// Host name resolution is retried periodically, thus DNS updates will be taken
    /// into account without the need to recreate this transport.
    pub async fn new(urls: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self> {
        let urls = urls
            .into_iter()
            .map(|url| url.as_ref().parse::<Url>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|err| Error::new(ErrorKind::InvalidInput, err))?;

        if urls.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "at least one URL is required"));
        }
        for url in &urls {
            if !url.has_host() {
                return Err(Error::new(ErrorKind::InvalidInput, "URL must have a host"));
            }
            if !["ws", "wss"].contains(&url.scheme()) {
                return Err(Error::new(ErrorKind::InvalidInput, "URL must have scheme ws or wss"));
            }
        }

        let this = Self {
            urls,
            ip_version: IpVersion::Both,
            resolve_interval: Duration::from_secs(10),
            connector: None,
            web_socket_config: None,
        };

        let addrs = this.resolve().await;
        if addrs.values().all(|addrs| addrs.is_empty()) {
            return Err(Error::new(ErrorKind::NotFound, "cannot resolve IP address of any URL"));
        }
        tracing::info!("URLs resolve to: {:?}", &addrs);

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

    /// Sets the WebSocket connector for establishing the connection.
    ///
    /// Allows control of TLS.
    pub fn set_connector(&mut self, connector: Option<Connector>) {
        self.connector = connector;
    }

    /// Sets the WebSocket connection configuration.
    pub fn set_web_socket_config(&mut self, web_socket_config: Option<WebSocketConfig>) {
        self.web_socket_config = web_socket_config;
    }

    /// Resolve URLs to socket addresses.
    async fn resolve(&self) -> HashMap<&Url, Vec<SocketAddr>> {
        let mut url_addrs = HashMap::new();

        for url in &self.urls {
            let host = url.host_str().unwrap();
            let port = url.port_or_known_default().unwrap();
            let addrs = resolve_hosts(&[format!("{host}:{port}")], self.ip_version).await;
            url_addrs.insert(url, addrs);
        }

        url_addrs
    }
}

#[async_trait]
impl ConnectingTransport for WebSocketConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        loop {
            let interfaces = local_interfaces()?;

            let mut tags: HashSet<LinkTagBox> = HashSet::new();
            for (url, addrs) in self.resolve().await {
                for addr in addrs {
                    for interface in interface_names_for_target(&interfaces, addr) {
                        let tag = OutgoingWebSocketLinkTag {
                            interface,
                            remote: addr,
                            url: url.to_string(),
                            tls: url.scheme() == "wss",
                        };
                        tags.insert(Box::new(tag));
                    }
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

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        let tag: &OutgoingWebSocketLinkTag = tag.as_any().downcast_ref().unwrap();

        // Establish TCP connection to server.
        let socket = match tag.remote.ip() {
            IpAddr::V4(_) => TcpSocket::new_v4(),
            IpAddr::V6(_) => TcpSocket::new_v6(),
        }?;
        bind_socket_to_interface(&socket, &tag.interface, tag.remote.ip())?;
        let stream = socket.connect(tag.remote).await?;
        let _ = stream.set_nodelay(true);

        // Convert into WebSocket.
        let connector = if tag.tls { self.connector.clone() } else { Some(Connector::Plain) };
        let (web_socket, _rsp) =
            client_async_tls_with_config(&tag.url, stream, self.web_socket_config, connector)
                .await
                .map_err(|err| Error::new(ErrorKind::ConnectionRefused, err))?;

        // Adapt WebSocket IO.
        let (ws_tx, ws_rx) = web_socket.split();
        let ws_tx = Box::pin(
            ws_tx
                .with(|data: Bytes| async move {
                    Ok::<_, tungstenite::Error>(tungstenite::Message::Binary(data.into()))
                })
                .sink_map_err(|err| Error::new(ErrorKind::Other, err)),
        );
        let ws_rx = Box::pin(
            ws_rx
                .try_filter_map(|msg: tungstenite::Message| async move {
                    if let tungstenite::Message::Binary(data) = msg {
                        Ok(Some(data.into()))
                    } else {
                        Ok(None)
                    }
                })
                .map_err(|err| Error::new(ErrorKind::Other, err)),
        );

        Ok(TxRxBox::new(ws_tx, ws_rx).into())
    }

    async fn link_filter(&self, new: &Link<LinkTagBox>, existing: &[Link<LinkTagBox>]) -> bool {
        let Some(new_tag) = new.tag().as_any().downcast_ref::<OutgoingWebSocketLinkTag>() else { return true };

        let intro = format!(
            "Judging {} WebSocket link {} {} ({}) on {}",
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
            let Some(tag) = link.tag().as_any().downcast_ref::<OutgoingWebSocketLinkTag>() else { return false };
            tag.interface == new_tag.interface && link.remote_user_data() == new.remote_user_data()
        }) {
            Some(other) => {
                let other_tag = other.tag().as_any().downcast_ref::<OutgoingWebSocketLinkTag>().unwrap();
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

/// Link tag for incoming WebSocket link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IncomingWebSocketLinkTag {
    /// Local socket address.
    pub local: SocketAddr,
    /// Remote socket address.
    pub remote: SocketAddr,
    /// WebSocket sub-protocol.
    pub protocol: Option<String>,
}

impl fmt::Display for IncomingWebSocketLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} <- {}{}",
            &self.local,
            &self.remote,
            match &self.protocol {
                Some(protocol) => format!(" ({protocol})"),
                None => String::new(),
            }
        )
    }
}

impl LinkTag for IncomingWebSocketLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        Direction::Incoming
    }

    fn user_data(&self) -> Vec<u8> {
        match self.local.ip() {
            IpAddr::V4(ip) => ip.octets().into(),
            IpAddr::V6(ip) => ip.octets().into(),
        }
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

struct IncomingWebSocket {
    local: SocketAddr,
    remote: SocketAddr,
    web_socket: axum::extract::ws::WebSocket,
}

/// Builds a [WebSocket transport listener](WebSocketAcceptor).
pub struct WebSocketAcceptorBuilder {
    tx: mpsc::Sender<IncomingWebSocket>,
    rx: mpsc::Receiver<IncomingWebSocket>,
}

impl fmt::Debug for WebSocketAcceptorBuilder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WebSocketAcceptorBuilder").finish()
    }
}

impl WebSocketAcceptorBuilder {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel(16);
        Self { tx, rx }
    }
}

impl WebSocketAcceptorBuilder {
    /// Creates a Axum router that accepts a WebSocket connection at the specified `path`.
    ///
    /// The router must be converted into a service with connection info,
    /// see [`axum::Router::into_make_service_with_connect_info`] with
    /// connection info type [`SocketAddr`].
    pub fn router(&self, path: &str) -> Router {
        let protocols: [String; 0] = [];
        self.custom_router(path, SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0), protocols)
    }

    /// Creates a Axum router that accepts a WebSocket connection at the specified `path` with custom options.
    ///
    /// `local_addr` specifies to local address the axum server is listening on.
    /// This is used for link filtering if the server is listening on multiple IP addresses.
    ///
    /// `protocols` specifies the known WebSocket protocols to advertise to a connecting client.
    ///
    /// The router must be converted into a service with connection info,
    /// see [`axum::Router::into_make_service_with_connect_info`] with
    /// connection info type [`SocketAddr`].
    pub fn custom_router(
        &self, path: &str, local_addr: SocketAddr, protocols: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Router {
        let protocols: Vec<_> = protocols.into_iter().map(|p| p.as_ref().to_string()).collect();
        let tx = self.tx.clone();

        Router::new().route(
            path,
            get(move |ws: WebSocketUpgrade, ConnectInfo(remote): ConnectInfo<SocketAddr>| async move {
                match tx.reserve_owned().await {
                    Ok(permit) => ws.protocols(protocols.clone()).on_upgrade(move |web_socket| async move {
                        permit.send(IncomingWebSocket { local: local_addr, remote, web_socket });
                    }),
                    Err(_) => Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .body(boxed("WebSocketAcceptor was dropped".to_string()))
                        .unwrap(),
                }
            }),
        )
    }

    /// Builds the [WebSocket transport listener](WebSocketAcceptor).
    pub fn build(self) -> WebSocketAcceptor {
        WebSocketAcceptor { rx: Mutex::new(self.rx) }
    }
}

/// WebSocket transport for incoming connections.
///
/// This transport is packet-based.
#[derive(Debug)]
pub struct WebSocketAcceptor {
    rx: Mutex<mpsc::Receiver<IncomingWebSocket>>,
}

impl fmt::Display for WebSocketAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WebSocketAcceptor").finish()
    }
}

impl WebSocketAcceptor {
    /// Create a new WebSocket transport listening for incoming connections at the specified `path`.
    pub fn new(path: &str) -> (Self, Router) {
        let wsab = WebSocketAcceptorBuilder::new();
        let router = wsab.router(path);
        (wsab.build(), router)
    }

    /// Starts building a WebSocket transport listener.
    pub fn builder() -> WebSocketAcceptorBuilder {
        WebSocketAcceptorBuilder::new()
    }
}

#[async_trait]
impl AcceptingTransport for WebSocketAcceptor {
    fn name(&self) -> &str {
        NAME
    }

    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
        let mut rx = self.rx.try_lock().unwrap();

        while let Some(IncomingWebSocket { local, mut remote, web_socket }) = rx.recv().await {
            let protocol = web_socket.protocol().and_then(|hv| hv.to_str().ok()).map(|s| s.to_string());
            use_proper_ipv4(&mut remote);

            // Adapt WebSocket IO.
            let (ws_tx, ws_rx) = web_socket.split();
            let ws_tx = Box::pin(
                ws_tx
                    .with(|data: Bytes| async move {
                        Ok::<_, axum::Error>(axum::extract::ws::Message::Binary(data.into()))
                    })
                    .sink_map_err(|err| Error::new(ErrorKind::Other, err)),
            );
            let ws_rx = Box::pin(
                ws_rx
                    .try_filter_map(|msg: axum::extract::ws::Message| async move {
                        if let axum::extract::ws::Message::Binary(data) = msg {
                            Ok(Some(data.into()))
                        } else {
                            Ok(None)
                        }
                    })
                    .map_err(|err| Error::new(ErrorKind::Other, err)),
            );

            // Build tag.
            tracing::debug!("Accepted WebSocket connection from {remote}");
            let tag = IncomingWebSocketLinkTag { local, remote, protocol };

            let _ = tx.send(AcceptedStreamBox::new(TxRxBox::new(ws_tx, ws_rx).into(), tag)).await;
        }

        Err(Error::new(ErrorKind::ConnectionReset, "router was dropped"))
    }
}
