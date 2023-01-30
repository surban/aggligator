//! TLS wrapper.

use async_trait::async_trait;
use rustls::{ClientConfig, ServerConfig, ServerName};
use std::{io::Result, sync::Arc};
use tokio::io::split;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use super::{AcceptingWrapper, ConnectingWrapper, IoBox};

static NAME: &str = "tls";

/// TLS outgoing connection wrapper.
#[derive(Debug)]
#[must_use = "you must pass this wrapper to the connector"]
pub struct TlsClient {
    server_name: ServerName,
    client_cfg: Arc<ClientConfig>,
}

impl TlsClient {
    /// Creates a new TLS outgoing connection wrapper.
    ///
    /// The identity of the server is verified using TLS against `server_name`.
    /// The outgoing link is encrypted using TLS with the configuration specified
    /// in `client_cfg`.
    pub fn new(client_cfg: Arc<ClientConfig>, server_name: ServerName) -> Self {
        Self { server_name, client_cfg }
    }
}

#[async_trait]
impl ConnectingWrapper for TlsClient {
    fn name(&self) -> &str {
        NAME
    }

    async fn wrap(&self, io: IoBox) -> Result<IoBox> {
        let connector = TlsConnector::from(self.client_cfg.clone());
        let tls = connector.connect(self.server_name.clone(), io).await?;
        let (rh, wh) = split(tls);
        Ok(IoBox::new(rh, wh))
    }
}

/// TLS incoming connection wrapper.
#[derive(Debug)]
#[must_use = "you must pass this wrapper to the acceptor"]
pub struct TlsServer {
    server_cfg: Arc<ServerConfig>,
}

impl TlsServer {
    /// Creates a new TLS incoming connection wrapper.
    ///
    /// Incoming links are encrypted using TLS with the configuration specified
    /// in `server_cfg`.
    pub fn new(server_cfg: Arc<ServerConfig>) -> Self {
        Self { server_cfg }
    }
}

#[async_trait]
impl AcceptingWrapper for TlsServer {
    fn name(&self) -> &str {
        NAME
    }

    async fn wrap(&self, io: IoBox) -> Result<IoBox> {
        let acceptor = TlsAcceptor::from(self.server_cfg.clone());
        let tls = acceptor.accept(io).await?;
        let (rh, wh) = split(tls);
        Ok(IoBox::new(rh, wh))
    }
}