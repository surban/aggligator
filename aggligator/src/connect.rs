//! Establishing new incoming and outgoing connections.
//!
//! To accept **incoming connections** a [Server] and corresponding [Listener] are required.
//! Incoming links must be fed to the server, which then combines them and
//! exposes them as connections of aggregated links via the listener.
//!
//! **Outgoing connections** are built using the [connect] function or [Server::connect] method,
//! which create a new connection together with its [Control] interface.
//! The control interface is then used to add outgoing links to the connection.
//!

use bytes::Bytes;
use futures::{future, future::BoxFuture, FutureExt, Sink, Stream};
use std::{
    collections::{hash_map::Entry, HashMap},
    fmt,
    future::IntoFuture,
    io,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
    time::{error::Elapsed, timeout, Instant},
};
use x25519_dalek::{EphemeralSecret, PublicKey};

use crate::{
    agg::{link_int::LinkInt, task::Task, AggParts},
    alc::Channel,
    cfg::{Cfg, ExchangedCfg},
    control::{Control, Direction, Link},
    id::{ConnId, OwnedConnId, ServerId},
    io::{IoRx, IoTx},
    msg::{LinkMsg, RefusedReason},
    protocol_err,
};

/// Listen error.
#[derive(Debug)]
pub enum ListenError {
    /// A listener for the server already exists.
    AlreadyListening,
}

impl fmt::Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::AlreadyListening => write!(f, "already listening"),
        }
    }
}

impl std::error::Error for ListenError {}

impl From<ListenError> for io::Error {
    fn from(err: ListenError) -> Self {
        io::Error::new(io::ErrorKind::AddrInUse, err)
    }
}

/// Incoming link error.
#[derive(Debug)]
pub enum IncomingError {
    /// Sending or receiving over the link failed.
    Io(io::Error),
    /// The incoming connection was refused by the listener.
    Refused,
    /// No listener is present to handle the incoming connection.
    NotListening,
    /// The incoming link belonged to an already closed connection.
    Closed,
    /// The link aggregator server was dropped.
    ServerDropped,
}

impl fmt::Display for IncomingError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "IO error: {err}"),
            Self::Refused => write!(f, "connection refused"),
            Self::NotListening => write!(f, "not listening"),
            Self::Closed => write!(f, "connection was closed"),
            Self::ServerDropped => write!(f, "server dropped"),
        }
    }
}

impl From<io::Error> for IncomingError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<Elapsed> for IncomingError {
    fn from(err: Elapsed) -> Self {
        Self::Io(err.into())
    }
}

impl std::error::Error for IncomingError {}

impl From<IncomingError> for io::Error {
    fn from(err: IncomingError) -> Self {
        match err {
            IncomingError::Io(err) => err,
            IncomingError::Refused => io::Error::new(io::ErrorKind::ConnectionRefused, err),
            IncomingError::NotListening => io::Error::new(io::ErrorKind::ConnectionRefused, err),
            IncomingError::Closed => io::Error::new(io::ErrorKind::ConnectionAborted, err),
            IncomingError::ServerDropped => io::Error::new(io::ErrorKind::ConnectionRefused, err),
        }
    }
}

/// Outgoing connection error.
#[derive(Debug)]
pub enum ConnectError {
    /// No working link was established during the configured timeout.
    Timeout,
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConnectError::Timeout => write!(f, "connect timeout"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<ConnectError> for io::Error {
    fn from(err: ConnectError) -> Self {
        io::Error::new(io::ErrorKind::TimedOut, err)
    }
}

/// An incoming connection consisting of aggregated links.
///
/// Call [accept](Self::accept) to accept the connection and open an aggregated channel.
/// Call [refuse](Self::refuse) to refuse the connection.
pub struct Incoming<TX, RX, TAG> {
    cfg: Arc<Cfg>,
    conn_id: OwnedConnId,
    server_id: ServerId,
    remote_server_id: Option<ServerId>,
    link_tx: mpsc::Sender<LinkInt<TX, RX, TAG>>,
    link_rx: mpsc::Receiver<LinkInt<TX, RX, TAG>>,
    links: Vec<LinkInt<TX, RX, TAG>>,
}

impl<TX, RX, TAG> fmt::Debug for Incoming<TX, RX, TAG>
where
    TAG: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let link_tags: Vec<_> = self.links.iter().map(|link| link.tag()).collect();
        f.debug_struct("Incoming")
            .field("cfg", &self.cfg)
            .field("id", &self.id())
            .field("server_id", &self.server_id)
            .field("remote_server_id", &self.remote_server_id)
            .field("link_tags", &link_tags)
            .finish()
    }
}

impl<TX, RX, TAG> Incoming<TX, RX, TAG> {
    /// The connection id.
    pub fn id(&self) -> ConnId {
        self.conn_id.get()
    }

    /// The server id of the local server.
    pub fn server_id(&self) -> ServerId {
        self.server_id
    }

    /// The server id of the remote endpoint.
    ///
    /// `None` if the remote endpoint is using an outgoing-only connection.
    pub fn remote_server_id(&self) -> Option<ServerId> {
        self.remote_server_id
    }

    /// Updates the incoming links for the connection.
    fn update_links(&mut self) {
        while let Ok(link_int) = self.link_rx.try_recv() {
            self.links.push(link_int);
        }
    }

    /// The tags of the links for the incoming connection.
    pub fn link_tags(&mut self) -> Vec<&TAG> {
        self.update_links();
        self.links.iter().map(|link| link.tag()).collect()
    }

    /// The remote user datas of the links for the incoming connection.
    pub fn link_remote_user_datas(&mut self) -> Vec<&[u8]> {
        self.update_links();
        self.links.iter().map(|link| link.remote_user_data()).collect()
    }

    /// Waits until a new link has been added to the incoming connection.
    pub async fn link_added(&mut self) -> Result<(), IncomingError> {
        let link_int = self.link_rx.recv().await.ok_or(IncomingError::ServerDropped)?;
        self.links.push(link_int);
        Ok(())
    }
}

impl<TX, RX, TAG> Incoming<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    TAG: Send + Sync + 'static,
{
    /// Accepts the incoming connection.
    ///
    /// Returns the [task](Task), [channel](Channel) and [control interface](Control) for the connection.
    ///
    /// The [`Task`] manages the connection and must be executed
    /// (for example using [`tokio::spawn`]) for the connection to work.
    pub fn accept(mut self) -> (Task<TX, RX, TAG>, Channel, Control<TX, RX, TAG>) {
        self.update_links();

        let Self { cfg, conn_id, server_id, remote_server_id, link_tx, link_rx, links } = self;

        let AggParts { task, channel, control, connected_rx: _ } = AggParts::new(
            cfg,
            conn_id,
            Direction::Incoming,
            Some(server_id),
            remote_server_id,
            links,
            Some((link_tx, link_rx)),
        );

        (task, channel, control)
    }

    /// Refuses the incoming connection.
    pub async fn refuse(mut self) {
        self.link_rx.close();
        self.update_links();

        let send_refused = future::join_all(self.links.iter_mut().map(|link| async move {
            let _ = link.send_msg_and_flush(LinkMsg::Refused { reason: RefusedReason::ConnectionRefused }).await;
        }));
        let _ = timeout(self.cfg.link_non_working_timeout, send_refused).await;
    }
}

/// Server implementation.
struct ServerInner<TX, RX, TAG> {
    cfg: Arc<Cfg>,
    server_id: ServerId,
    conns: HashMap<ConnId, mpsc::Sender<LinkInt<TX, RX, TAG>>>,
    closed_conns_tx: mpsc::UnboundedSender<ConnId>,
    closed_conns_rx: mpsc::UnboundedReceiver<ConnId>,
    listen_tx: mpsc::Sender<Incoming<TX, RX, TAG>>,
}

impl<TX, RX, TAG> ServerInner<TX, RX, TAG> {
    fn new(cfg: Arc<Cfg>, server_id: ServerId) -> Self {
        let (closed_conns_tx, closed_conns_rx) = mpsc::unbounded_channel();
        let listen_tx = mpsc::channel(cfg.connect_queue.get()).0;
        Self { cfg, server_id, conns: HashMap::new(), closed_conns_tx, closed_conns_rx, listen_tx }
    }

    /// Clean up closed connections.
    fn cleanup_links(&mut self) {
        while let Ok(id) = self.closed_conns_rx.try_recv() {
            self.conns.remove(&id);
        }
    }
}

/// Handle to a link aggregator server.
///
/// This allows accepting incoming links and add them to existing connections
/// or start new connections.
///
/// Clones share the same underlying server.
pub struct Server<TX, RX, TAG> {
    server_id: ServerId,
    inner: Arc<Mutex<ServerInner<TX, RX, TAG>>>,
}

impl<TX, RX, TAG> fmt::Debug for Server<TX, RX, TAG> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Server").field("id", &self.server_id).finish()
    }
}

impl<TX, RX, TAG> Clone for Server<TX, RX, TAG> {
    fn clone(&self) -> Self {
        Self { server_id: self.server_id, inner: self.inner.clone() }
    }
}

impl<TX, RX, TAG> Server<TX, RX, TAG>
where
    TAG: Send + Sync + 'static,
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
{
    /// Creates a new link aggregator server.
    pub fn new(cfg: Cfg) -> Self {
        let server_id = ServerId::generate();
        Self { server_id, inner: Arc::new(Mutex::new(ServerInner::new(Arc::new(cfg), server_id))) }
    }

    /// The server id.
    pub fn id(&self) -> ServerId {
        self.server_id
    }

    /// Starts building a new outgoing connection.
    ///
    /// Incoming links can be added to this connection.
    ///
    /// The [`Task`] manages the connection and must be executed
    /// (for example using [`tokio::spawn`]) for the connection to work.
    /// Add links to the connection using [`Control::add`] or [`Control::add_io`]
    /// and then call [`Outgoing::connect`] to establish the connection.
    pub fn connect(&self) -> (Task<TX, RX, TAG>, Outgoing, Control<TX, RX, TAG>) {
        let mut inner = self.inner.lock().unwrap();

        let conn_id = ConnId::generate();
        let (link_tx, link_rx) = mpsc::channel(inner.cfg.connect_queue.get());

        let AggParts { task, channel, control, connected_rx } = AggParts::new(
            inner.cfg.clone(),
            OwnedConnId::new(conn_id, inner.closed_conns_tx.clone()),
            Direction::Outgoing,
            Some(self.server_id),
            None,
            Vec::new(),
            Some((link_tx.clone(), link_rx)),
        );

        inner.conns.insert(conn_id, link_tx);

        (task, Outgoing { channel, connected_rx }, control)
    }

    /// Starts accepting *new* incoming connections.
    ///
    /// Only one [`Listener`] may be present at a time.
    /// If one already exists, an error is returned.
    ///
    /// Incoming links can be added to *existing* connections without listening.
    pub fn listen(&self) -> Result<Listener<TX, RX, TAG>, ListenError> {
        let mut inner = self.inner.lock().unwrap();

        if !inner.listen_tx.is_closed() {
            return Err(ListenError::AlreadyListening);
        }

        let (listen_tx, listen_rx) = mpsc::channel(inner.cfg.connect_queue.get());
        inner.listen_tx = listen_tx;
        Ok(Listener { server_id: inner.server_id, listen_rx })
    }

    /// Adds an incoming, packet-based link.
    ///
    /// If the incoming link belongs to an existing connection, it is added to that connection.
    /// If not, but a [`Listener`] is present, a new incoming connection is created,
    /// that can be obtained by calling [`Listener::accept`].
    /// Otherwise the incoming link is refused.
    ///
    /// The `tag` consists of user-defined data that will be attached to the link.
    /// On existing links it can be queried using [`Link::tag`] and be used to identify the link.
    /// Aggligator does not process the tag data.
    ///
    /// The `user_data` is transferred to the remote endpoint when establishing the link.
    /// Its size must not exceed 64 kB.
    /// It can be used to transfer link-specific information and queried using [`Link::remote_user_data`].
    /// Aggligator does not process the user data.
    ///
    /// Returns a handle to the link.
    ///
    /// # Panics
    /// Panics when the size of `user_data` exceeds [`u16::MAX`].
    pub async fn add_incoming(
        &self, mut tx: TX, mut rx: RX, tag: TAG, user_data: &[u8],
    ) -> Result<Link<TAG>, IncomingError> {
        assert!(user_data.len() <= u16::MAX as usize, "user_data is too big");

        let server_id;
        let cfg;
        let closed_conns_tx;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.cleanup_links();
            server_id = inner.server_id;
            cfg = inner.cfg.clone();
            closed_conns_tx = inner.closed_conns_tx.clone();
        }

        // Perform protocol handshake.
        let (remote_server_id, conn_id, existing, remote_cfg, roundtrip, remote_user_data) =
            timeout(cfg.link_ping_timeout, async {
                let server_secret = EphemeralSecret::new(rand_core::OsRng);
                let server_public_key = PublicKey::from(&server_secret);

                let start = Instant::now();
                LinkMsg::Welcome {
                    extensions: 0,
                    public_key: server_public_key,
                    server_id,
                    user_data: user_data.to_vec(),
                    cfg: (&*cfg).into(),
                }
                .send(&mut tx)
                .await?;

                let LinkMsg::Connect {
                    extensions: _,
                    public_key: client_public_key,
                    server_id,
                    connection_id: encrypted_conn_id,
                    existing_connection,
                    user_data: remote_user_data,
                    cfg,
                } = LinkMsg::recv(&mut rx).await?
                else {
                    return Err::<_, IncomingError>(protocol_err!("expected Connect message").into());
                };

                let shared_secret = server_secret.diffie_hellman(&client_public_key);
                let conn_id = encrypted_conn_id.decrypt(&shared_secret);

                Ok((server_id, conn_id, existing_connection, cfg, start.elapsed(), remote_user_data))
            })
            .await??;

        tracing::debug!(%server_id, %conn_id, %existing, "handling incoming link");

        enum Connection<TX, RX, TAG> {
            Existing {
                link_tx: mpsc::Sender<LinkInt<TX, RX, TAG>>,
            },
            New {
                link_tx: mpsc::Sender<LinkInt<TX, RX, TAG>>,
                link_rx: mpsc::Receiver<LinkInt<TX, RX, TAG>>,
                listen_tx_permit: mpsc::OwnedPermit<Incoming<TX, RX, TAG>>,
            },
            Refuse {
                reason: RefusedReason,
                err: IncomingError,
            },
        }

        let mut need_listen_tx_permit = false;
        let connection = loop {
            // Obtain listen queue permit if required.
            let listen_tx_permit = if need_listen_tx_permit {
                let listen_tx = self.inner.lock().unwrap().listen_tx.clone();
                Some(listen_tx.reserve_owned().await)
            } else {
                None
            };

            // Check if link belongs to existing connection.
            let mut inner = self.inner.lock().unwrap();
            match inner.conns.entry(conn_id) {
                // Link joins existing connection.
                Entry::Occupied(ocu) => break Connection::Existing { link_tx: ocu.get().clone() },

                // Link belongs to new, incoming connection.
                Entry::Vacant(vac) if !existing => match listen_tx_permit {
                    Some(Ok(listen_tx_permit)) => {
                        let (link_tx, link_rx) = mpsc::channel(cfg.connect_queue.get());
                        vac.insert(link_tx.clone());
                        break Connection::New { link_tx, link_rx, listen_tx_permit };
                    }
                    Some(Err(_)) => {
                        break Connection::Refuse {
                            reason: RefusedReason::NotListening,
                            err: IncomingError::NotListening,
                        }
                    }
                    None => need_listen_tx_permit = true,
                },

                // Link belongs to unknown connection, but existing connection was expected.
                Entry::Vacant(_) => {
                    break Connection::Refuse { reason: RefusedReason::Closed, err: IncomingError::Closed }
                }
            }
        };

        match connection {
            // Link joins existing connection.
            Connection::Existing { link_tx } => match link_tx.reserve_owned().await {
                Ok(link_tx_permit) => {
                    let link_int = LinkInt::new(
                        tag,
                        conn_id,
                        tx,
                        rx,
                        cfg,
                        remote_cfg,
                        Direction::Incoming,
                        roundtrip,
                        remote_user_data,
                    );
                    let link = Link::from(&link_int);
                    link_tx_permit.send(link_int);

                    tracing::debug!("link joins existing connection {conn_id}");
                    Ok(link)
                }
                Err(_) => {
                    tracing::debug!("refusing link that belongs to closed connection");
                    timeout(
                        cfg.link_ping_timeout,
                        LinkMsg::Refused { reason: RefusedReason::Closed }.send(&mut tx),
                    )
                    .await??;
                    Err(IncomingError::Closed)
                }
            },

            // Link belongs to new, incoming connection.
            Connection::New { link_tx, link_rx, listen_tx_permit } => {
                let link_int = LinkInt::new(
                    tag,
                    conn_id,
                    tx,
                    rx,
                    cfg.clone(),
                    remote_cfg,
                    Direction::Incoming,
                    roundtrip,
                    remote_user_data,
                );
                let link = Link::from(&link_int);
                link_tx.try_send(link_int).unwrap();

                listen_tx_permit.send(Incoming {
                    cfg,
                    conn_id: OwnedConnId::new(conn_id, closed_conns_tx),
                    server_id: self.server_id,
                    remote_server_id,
                    link_tx,
                    link_rx,
                    links: Vec::new(),
                });

                tracing::debug!("link starts new connection {conn_id}");
                Ok(link)
            }

            // Link cannot be accepted.
            Connection::Refuse { reason, err } => {
                tracing::debug!("refusing link with reason {reason:?}: {err}");
                timeout(cfg.link_ping_timeout, LinkMsg::Refused { reason }.send(&mut tx)).await??;
                Err(err)
            }
        }
    }
}

impl<R, W, TAG> Server<IoTx<W>, IoRx<R>, TAG>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
    TAG: Send + Sync + 'static,
{
    /// Adds an incoming, stream-based link.
    ///
    /// The stream-based link is wrapped in the [integrity codec](crate::io::IntegrityCodec)
    /// to make it packet-based.
    ///
    /// If the incoming link belongs to an existing connection, it is added to that connection.
    /// If not, but a [`Listener`] is present, a new incoming connection is created,
    /// that can be obtained by calling [`Listener::accept`].
    /// Otherwise the incoming link is refused.
    ///
    /// The `tag` consists of user-defined data that will be attached to the link.
    /// On existing links it can be queried using [`Link::tag`] and be used to identify the link.
    /// Aggligator does not process the tag data.
    ///
    /// The `user_data` is transferred to the remote endpoint when establishing the link.
    /// Its size must not exceed 64 kB.
    /// It can be used to transfer link-specific information and queried using [`Link::remote_user_data`].
    /// Aggligator does not process the user data.
    ///
    /// Returns a handle to the link.
    ///
    /// # Panics
    /// Panics when the size of `user_data` exceeds [`u16::MAX`].
    pub async fn add_incoming_io(
        &self, read: R, write: W, tag: TAG, user_data: &[u8],
    ) -> Result<Link<TAG>, IncomingError> {
        self.add_incoming(IoTx::new(write), IoRx::new(read), tag, user_data).await
    }
}

/// Listens for new connections consisting of aggregated links.
///
/// Drop to stop listening.
pub struct Listener<TX, RX, TAG> {
    server_id: ServerId,
    listen_rx: mpsc::Receiver<Incoming<TX, RX, TAG>>,
}

impl<N, R, W> fmt::Debug for Listener<N, R, W> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Listener").field("server_id", &self.server_id).finish()
    }
}

impl<TX, RX, TAG> Listener<TX, RX, TAG>
where
    TAG: Send + Sync + 'static,
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
{
    /// The server id.
    pub fn id(&self) -> ServerId {
        self.server_id
    }

    /// Gets the next incoming connection.
    ///
    /// The incoming connection can be inspected before making the
    /// decision to accept or refuse it.
    ///
    /// This function is cancel-safe.
    pub async fn next(&mut self) -> Result<Incoming<TX, RX, TAG>, IncomingError> {
        self.listen_rx.recv().await.ok_or(IncomingError::ServerDropped)
    }

    /// Accepts the next incoming connection.
    ///
    /// This function is cancel-safe.
    pub async fn accept(&mut self) -> Result<(Task<TX, RX, TAG>, Channel, Control<TX, RX, TAG>), IncomingError> {
        let ic = self.next().await?;
        Ok(ic.accept())
    }
}

/// Establishes a new outgoing connection consisting of aggregated links.
///
/// Add one or more links to the connection using its [controller](Control).
/// Then call [connect](Self::connect) to establish the connection.
pub struct Outgoing {
    channel: Channel,
    connected_rx: oneshot::Receiver<Arc<ExchangedCfg>>,
}

impl fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Outgoing").field("id", &self.id()).finish()
    }
}

impl Outgoing {
    /// The connection id.
    pub fn id(&self) -> ConnId {
        self.channel.id()
    }

    /// Establishes the connection over the link(s) added via the controller.
    ///
    /// If the connection cannot be established over any link
    /// after the connection timeout has passed, an error is returned.
    pub async fn connect(self) -> Result<Channel, ConnectError> {
        let Self { mut channel, connected_rx } = self;

        let remote_cfg = connected_rx.await.map_err(|_| ConnectError::Timeout)?;
        channel.set_remote_cfg(remote_cfg);

        Ok(channel)
    }
}

impl IntoFuture for Outgoing {
    type Output = Result<Channel, ConnectError>;
    type IntoFuture = BoxFuture<'static, Result<Channel, ConnectError>>;

    fn into_future(self) -> Self::IntoFuture {
        self.connect().boxed()
    }
}

/// Starts building a new connection consisting of outgoing links only.
///
/// Incoming links cannot be added to this connection.
///
/// The [`Task`] manages the connection and must be executed
/// (for example using [`tokio::spawn`]) for the connection to work.
/// Add links to the connection using [`Control::add`] or [`Control::add_io`]
/// and then call [`Outgoing::connect`] to establish the connection.
pub fn connect<TX, RX, TAG>(cfg: Cfg) -> (Task<TX, RX, TAG>, Outgoing, Control<TX, RX, TAG>)
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    TAG: Send + Sync + 'static,
{
    let AggParts { task, channel, control, connected_rx } = AggParts::new(
        Arc::new(cfg),
        OwnedConnId::untracked(ConnId::generate()),
        Direction::Outgoing,
        None,
        None,
        Vec::new(),
        None,
    );

    (task, Outgoing { channel, connected_rx }, control)
}
