//! Link acceptor.

use async_trait::async_trait;
use futures::{future, future::BoxFuture, pin_mut, stream::FuturesUnordered, FutureExt, StreamExt};
use std::{
    fmt,
    future::IntoFuture,
    io::{Error, ErrorKind, Result},
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch, Mutex, OwnedSemaphorePermit, RwLock, Semaphore},
    time::{sleep_until, Instant},
};

use super::{
    BoxControl, BoxLink, BoxLinkError, BoxListener, BoxServer, BoxTask, LinkError, LinkTag, LinkTagBox,
    StreamBox, TxRxBox,
};
use aggligator::{alc::Channel, Cfg, Server};

/// An accepted incoming stream.
pub struct AcceptedStreamBox {
    /// Stream.
    pub stream: StreamBox,
    /// Link tag.
    pub tag: LinkTagBox,
}

impl AcceptedStreamBox {
    /// Creates a new instance.
    pub fn new(stream: StreamBox, tag: impl LinkTag) -> Self {
        Self { stream, tag: Box::new(tag) }
    }
}

/// A transport for accepting connections from remote endpoints.
#[async_trait]
pub trait AcceptingTransport: Send + Sync + 'static {
    /// Name of the transport.
    fn name(&self) -> &str;

    /// Accepts incoming connections.
    ///
    /// This functions listens for incoming connections, accepts them and
    /// sends the read stream, write stream and link tag over the provided channel.
    async fn listen(&self, tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()>;

    /// Checks whether a new link can be added given existing links.
    async fn link_filter(&self, _new: &BoxLink, _existing: &[BoxLink]) -> bool {
        true
    }
}

type ArcAcceptingTransport = Arc<dyn AcceptingTransport>;

/// Function configuring the connection task of each incoming connection.
type TaskCfgFn = Box<dyn Fn(&mut BoxTask) + Send + Sync + 'static>;

/// A wrapper for an incoming link.
#[async_trait]
pub trait AcceptingWrapper: Send + Sync + fmt::Debug + 'static {
    /// Name of the wrapper.
    fn name(&self) -> &str;

    /// Wraps the incoming stream.
    async fn wrap(&self, io: StreamBox) -> Result<StreamBox>;
}

type BoxAcceptingWrapper = Box<dyn AcceptingWrapper>;

struct AcceptingTransportPack {
    transport: ArcAcceptingTransport,
    result_tx: oneshot::Sender<Result<()>>,
    remove_rx: oneshot::Receiver<()>,
    _permit: OwnedSemaphorePermit,
}

/// Builds a customized [`Acceptor`].
pub struct AcceptorBuilder {
    server: BoxServer,
    task_cfg: TaskCfgFn,
    wrappers: Vec<BoxAcceptingWrapper>,
    no_transport_timeout: Duration,
}

impl AcceptorBuilder {
    /// Creates a new builder.
    pub fn new(cfg: Cfg) -> Self {
        let server = Server::new(cfg);
        let task_cfg: TaskCfgFn = Box::new(|_| ());
        Self { server, task_cfg, wrappers: Vec::new(), no_transport_timeout: Duration::from_secs(30) }
    }

    /// Sets the function configuring the connection task of each incoming connection.
    pub fn set_task_cfg(&mut self, task_cfg: impl Fn(&mut BoxTask) + Send + Sync + 'static) {
        self.task_cfg = Box::new(task_cfg);
    }

    /// Sets the timeout for waiting for a connection when no transports are currently present.
    pub fn set_no_transport_timeout(&mut self, no_transport_timeout: Duration) {
        self.no_transport_timeout = no_transport_timeout;
    }

    /// Adds a connection wrapper to the wrapper stack.
    pub fn wrap(&mut self, wrapper: impl AcceptingWrapper) {
        self.wrappers.push(Box::new(wrapper))
    }

    /// Builds the acceptor.
    pub fn build(self) -> Acceptor {
        let Self { server, task_cfg, wrappers, no_transport_timeout } = self;

        let active_transports = Arc::new(RwLock::new(Vec::<Weak<dyn AcceptingTransport>>::new()));
        let (transport_tx, transport_rx) = mpsc::unbounded_channel();
        let (transports_present_tx, transports_present_rx) = watch::channel(true);
        let (error_tx, error_rx) = broadcast::channel(1024);
        let listener = Mutex::new(server.listen().unwrap());

        tokio::spawn(Acceptor::task(
            server.clone(),
            active_transports.clone(),
            transport_rx,
            error_tx,
            transports_present_tx,
            wrappers,
        ));

        Acceptor {
            server,
            listener,
            task_cfg,
            transport_tx,
            transports_present_rx,
            transports_being_added: Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
            error_rx,
            active_transports,
            no_transport_timeout,
        }
    }
}

/// Accepts incoming connections from remote endpoints using a variety of transports.
///
/// Dropping this stops listening and accepting incoming connections.
pub struct Acceptor {
    server: BoxServer,
    listener: Mutex<BoxListener>,
    task_cfg: TaskCfgFn,
    transport_tx: mpsc::UnboundedSender<AcceptingTransportPack>,
    transports_present_rx: watch::Receiver<bool>,
    transports_being_added: Arc<Semaphore>,
    active_transports: Arc<RwLock<Vec<Weak<dyn AcceptingTransport>>>>,
    error_rx: broadcast::Receiver<BoxLinkError>,
    no_transport_timeout: Duration,
}

impl fmt::Debug for Acceptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Acceptor").field("id", &self.server.id()).finish()
    }
}

impl Default for Acceptor {
    fn default() -> Self {
        Self::new()
    }
}

impl Acceptor {
    /// Creates a new acceptor using the default configuration.
    ///
    /// Use [`AcceptorBuilder`] for customization.
    pub fn new() -> Self {
        AcceptorBuilder::new(Cfg::default()).build()
    }

    /// Creates a new acceptor using the default configuration and a single connection wrapper.
    pub fn wrapped(wrapper: impl AcceptingWrapper) -> Self {
        let mut builder = AcceptorBuilder::new(Cfg::default());
        builder.wrap(wrapper);
        builder.build()
    }

    /// Adds a new transport.
    pub fn add(&self, transport: impl AcceptingTransport) -> AcceptingTransportHandle {
        let name = transport.name().to_string();

        let (result_tx, result_rx) = oneshot::channel();
        let (remove_tx, remove_rx) = oneshot::channel();

        let pack = AcceptingTransportPack {
            transport: Arc::new(transport),
            result_tx,
            remove_rx,
            _permit: self.transports_being_added.clone().try_acquire_owned().unwrap(),
        };
        let _ = self.transport_tx.send(pack);

        AcceptingTransportHandle { name, result_rx, remove_tx }
    }

    /// Returns whether no transports are present.
    pub fn is_empty(&self) -> bool {
        !*self.transports_present_rx.borrow()
            && self.transports_being_added.available_permits() == Semaphore::MAX_PERMITS
    }

    /// Waits for an incoming connection and accepts it.
    ///
    /// Returns the aggregated link channel and control handle.
    ///
    /// This function is cancel-safe.
    pub async fn accept(&self) -> Result<(Channel, BoxControl)> {
        // Set up timeout for no available transports.
        let mut transports_present_rx = self.transports_present_rx.clone();
        let no_transport_timeout = self.no_transport_timeout;
        let timeout = async move {
            let mut until = None;
            loop {
                if *transports_present_rx.borrow_and_update() {
                    until = None;
                } else if until.is_none() {
                    until = Instant::now().checked_add(no_transport_timeout);
                }

                let sleep_task = async {
                    match until {
                        Some(until) => sleep_until(until).await,
                        None => future::pending().await,
                    }
                };

                tokio::select! {
                    () = sleep_task => return Error::new(ErrorKind::BrokenPipe, "no listening transports available"),
                    res = transports_present_rx.changed() => {
                        if res.is_err() {
                            return Error::new(ErrorKind::BrokenPipe, "listener was terminated");
                        }
                    }
                }
            }
        };
        pin_mut!(timeout);

        // Accept incoming connection.
        let mut listener = self.listener.lock().await;
        let (mut task, channel, control) = tokio::select! {
            res = listener.accept() => res?,
            err = &mut timeout => return Err(err),
        };

        // Configure connection task.
        (self.task_cfg)(&mut task);

        // Configure link filter.
        let active_transports = self.active_transports.clone();
        task.set_link_filter(move |link, others| {
            let active_transports = active_transports.clone();
            async move {
                let transports = active_transports.read_owned().await;
                for transport in &*transports {
                    let Some(transport) = transport.upgrade() else { continue };
                    if !transport.link_filter(&link, &others).await {
                        return false;
                    }
                }
                true
            }
        });

        // Run server task.
        tokio::spawn(task.run());

        tracing::debug!("accepted incoming connected {}", control.id());
        Ok((channel, control))
    }

    /// Subscribes to the stream of link errors.
    pub fn link_errors(&self) -> broadcast::Receiver<BoxLinkError> {
        self.error_rx.resubscribe()
    }

    /// Task managing all listening transports.
    async fn task(
        server: BoxServer, active_transports: Arc<RwLock<Vec<Weak<dyn AcceptingTransport>>>>,
        mut transport_rx: mpsc::UnboundedReceiver<AcceptingTransportPack>,
        link_error_tx: broadcast::Sender<BoxLinkError>, transports_present_tx: watch::Sender<bool>,
        wrappers: Vec<BoxAcceptingWrapper>,
    ) {
        let wrappers = Arc::new(wrappers);
        let mut transport_tasks = FuturesUnordered::new();

        loop {
            // Notify of transport availability.
            transports_present_tx.send_replace(!transport_tasks.is_empty());

            enum ListenerEvent {
                TransportAdded(AcceptingTransportPack),
                TaskEnded,
            }

            // Wait for event.
            let event = tokio::select! {
                res = transport_rx.recv() => {
                    match res {
                        Some(transport_pack) => ListenerEvent::TransportAdded(transport_pack),
                        None => break,
                    }
                }
                Some(()) = transport_tasks.next() => ListenerEvent::TaskEnded,
            };

            // Handle event.
            match event {
                ListenerEvent::TransportAdded(transport_pack) => {
                    // Add transport to list of active transports.
                    let mut active_transports = active_transports.write().await;
                    active_transports.retain(|at| at.strong_count() > 0);
                    active_transports.push(Arc::downgrade(&transport_pack.transport));

                    // Notify of transport availability.
                    transports_present_tx.send_replace(true);

                    // Start transport task.
                    transport_tasks.push(Self::transport_task(
                        server.clone(),
                        transport_pack,
                        link_error_tx.clone(),
                        wrappers.clone(),
                    ));
                }
                ListenerEvent::TaskEnded => (),
            }
        }
    }

    /// Task managing a listening transport.
    #[tracing::instrument(level="debug", skip_all, fields(id=%server.id(), transport=transport.transport.name()))]
    async fn transport_task(
        server: BoxServer, transport: AcceptingTransportPack, link_error_tx: broadcast::Sender<BoxLinkError>,
        wrappers: Arc<Vec<BoxAcceptingWrapper>>,
    ) {
        let AcceptingTransportPack { transport, result_tx, mut remove_rx, _permit: _ } = transport;

        let (tx, mut rx) = mpsc::channel(128);
        let mut listener = transport.listen(tx);

        let mut accepting_tasks = FuturesUnordered::new();

        let res = loop {
            // Accept incoming transport connection.
            let AcceptedStreamBox { stream: mut stream_box, tag } = tokio::select! {
                Some(accepted) = rx.recv() => accepted,
                Some(()) = accepting_tasks.next() => continue,
                res = &mut listener => break res,
                Ok(()) = &mut remove_rx => break Ok(()),
            };

            tracing::debug!("accepted transport connection for tag {tag}");
            if tag.transport_name() != transport.name() {
                break Err(Error::new(ErrorKind::Other, "link tag transport name mismatch".to_string()));
            }

            // Handle incoming connection in separate task.
            let wrappers = &*wrappers;
            let server = &server;
            let link_error_tx = &link_error_tx;
            let task = async move {
                // Apply wrappers to IO stream.
                for wrapper in wrappers {
                    let name = wrapper.name();
                    tracing::debug!("wrapping tag {tag} in {name}");

                    match wrapper.wrap(stream_box).await {
                        Ok(wrapped) => stream_box = wrapped,
                        Err(err) => {
                            tracing::debug!("wrapping tag {tag} in {name} failed: {err}");
                            let _ = link_error_tx.send(BoxLinkError::incoming(&tag, err));
                            return;
                        }
                    }
                }

                // Add link to aggregated connection.
                tracing::debug!("adding link for tag {tag} to connection");
                let user_data = tag.user_data();
                let TxRxBox { tx, rx } = stream_box.into_tx_rx();
                let link = match server.add_incoming(tx, rx, tag.clone(), &user_data).await {
                    Ok(link) => link,
                    Err(err) => {
                        tracing::debug!("adding link for tag {tag} to connection failed: {err}");
                        let _ = link_error_tx.send(LinkError::incoming(&tag, err.into()));
                        return;
                    }
                };
                tracing::debug!("link for tag {tag} connected");

                // Disconnect link when transport is removed.
                struct DisconnectLink<'a>(&'a BoxLink);
                impl<'a> Drop for DisconnectLink<'a> {
                    fn drop(&mut self) {
                        self.0.start_disconnect();
                    }
                }
                let _disconnect_link = DisconnectLink(&link);

                // Wait for disconnection and publish reason.
                let reason = link.disconnected().await;
                tracing::debug!("link for tag {tag} disconnected: {reason}");
                let _ = link_error_tx.send(BoxLinkError::incoming(&tag, reason.into()));
            };
            accepting_tasks.push(task);
        };

        let _ = result_tx.send(res);
    }
}

/// A handle to a listening transport.
///
/// Await this future to be notified when the transport fails.
///
/// Dropping this will not remove the transport from the listener.
pub struct AcceptingTransportHandle {
    name: String,
    result_rx: oneshot::Receiver<Result<()>>,
    remove_tx: oneshot::Sender<()>,
}

impl fmt::Debug for AcceptingTransportHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("AcceptingTransportHandle").field("name", &self.name).finish()
    }
}

impl AcceptingTransportHandle {
    /// Name of the transport.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Removes the transport from the listener.
    pub fn remove(self) {
        let Self { remove_tx, .. } = self;
        let _ = remove_tx.send(());
    }
}

impl IntoFuture for AcceptingTransportHandle {
    type Output = Result<()>;
    type IntoFuture = BoxFuture<'static, Result<()>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { result_rx, .. } = self;
        async move {
            match result_rx.await {
                Ok(res) => res,
                Err(_) => Ok(()),
            }
        }
        .boxed()
    }
}
