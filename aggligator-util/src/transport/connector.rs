//! Link connector.

use async_trait::async_trait;
use futures::{
    future::{self, BoxFuture},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use std::{
    collections::HashSet,
    fmt::{self, Debug},
    future::IntoFuture,
    io::{Error, ErrorKind, Result},
    iter,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch, RwLock},
    time::sleep,
};

use super::{BoxControl, BoxLink, BoxLinkError, BoxTask, LinkTag, LinkTagBox, StreamBox, TxRxBox};
use aggligator::{connect, control::DisconnectReason, Cfg, Link, Outgoing};

/// A transport for connecting to remote endpoints.
#[async_trait]
pub trait ConnectingTransport: Send + Sync + 'static {
    /// Name of the transport.
    fn name(&self) -> &str;

    /// Discovers link tags for connecting.
    ///
    /// Each link tag returned by this function will be passed to the [`connect`](Self::connect)
    /// function to establish a link.
    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()>;

    /// Connects a link tag.
    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox>;

    /// Checks whether a new link can be added given existing links.
    async fn link_filter(&self, _new: &Link<LinkTagBox>, _existing: &[Link<LinkTagBox>]) -> bool {
        true
    }

    /// Notifies the transport of all currently connected links of the connection.
    ///
    /// This includes links by other transports as well.
    async fn connected_links(&self, _links: &[Link<LinkTagBox>]) {}
}

type ArcConnectingTransport = Arc<dyn ConnectingTransport>;

/// A wrapper for an outgoing link.
#[async_trait]
pub trait ConnectingWrapper: Send + Sync + fmt::Debug + 'static {
    /// Name of the wrapper.
    fn name(&self) -> &str;

    /// Wraps the outgoing stream.
    async fn wrap(&self, io: StreamBox) -> Result<StreamBox>;
}

type BoxConnectingWrapper = Box<dyn ConnectingWrapper>;

struct TransportPack {
    transport: ArcConnectingTransport,
    result_tx: oneshot::Sender<Result<()>>,
    remove_rx: oneshot::Receiver<()>,
}

/// Builds a customized [`Connector`].
#[derive(Debug)]
pub struct ConnectorBuilder {
    task: BoxTask,
    outgoing: Outgoing,
    control: BoxControl,
    reconnect_delay: Duration,
    wrappers: Vec<BoxConnectingWrapper>,
}

impl ConnectorBuilder {
    /// Creates a new builder.
    pub fn new(cfg: Cfg) -> Self {
        let (task, outgoing, control) = connect(cfg);
        Self { task, outgoing, control, reconnect_delay: Duration::from_secs(10), wrappers: Vec::new() }
    }

    /// Accesses the connection manager task.
    pub fn task(&mut self) -> &mut BoxTask {
        &mut self.task
    }

    /// Sets the reconnect delay for failed links.
    pub fn set_reconnect_delay(&mut self, reconnect_delay: Duration) {
        self.reconnect_delay = reconnect_delay
    }

    /// Adds a connection wrapper to the wrapper stack.
    pub fn wrap(&mut self, wrapper: impl ConnectingWrapper) {
        self.wrappers.push(Box::new(wrapper))
    }

    /// Builds the connector.
    pub fn build(self) -> Connector {
        let Self { mut task, outgoing, control, reconnect_delay, wrappers } = self;

        // Configure link filter.
        let active_transports = Arc::new(RwLock::new(Vec::<Weak<dyn ConnectingTransport>>::new()));
        let active_transports_filter = active_transports.clone();
        task.set_link_filter(move |link, others| {
            let active_transports_filter = active_transports_filter.clone();
            async move {
                let transports = active_transports_filter.read_owned().await;
                for transport in &*transports {
                    let Some(transport) = transport.upgrade() else { continue };
                    if !transport.link_filter(&link, &others).await {
                        return false;
                    }
                }
                true
            }
        });

        // Run link aggregator task for connection.
        tokio::spawn(task.run());

        // Set up channels.
        let (transport_tx, transport_rx) = mpsc::unbounded_channel();
        let (tags_tx, tags_rx) = watch::channel(HashSet::new());
        let (error_tx, error_rx) = broadcast::channel(1024);
        let (disabled_tags_tx, disabled_tags_rx) = watch::channel(HashSet::new());

        // Start connector task managing all transports.
        tokio::spawn(Connector::task(
            control.clone(),
            active_transports,
            transport_rx,
            tags_tx,
            disabled_tags_rx,
            error_tx,
            reconnect_delay,
            wrappers,
        ));

        Connector { control, outgoing: Some(outgoing), transport_tx, tags_rx, error_rx, disabled_tags_tx }
    }
}

/// Connects to remote endpoint using a variety of transports.
///
/// Dropping this does not terminate the connection or the transport
/// connection task.
pub struct Connector {
    control: BoxControl,
    outgoing: Option<Outgoing>,
    transport_tx: mpsc::UnboundedSender<TransportPack>,
    tags_rx: watch::Receiver<HashSet<LinkTagBox>>,
    disabled_tags_tx: watch::Sender<HashSet<LinkTagBox>>,
    error_rx: broadcast::Receiver<BoxLinkError>,
}

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connector").field("id", &self.control.id()).finish()
    }
}

impl Default for Connector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector {
    /// Creates a new connector using the default configuration.
    ///
    /// Use [`ConnectorBuilder`] for customization.
    pub fn new() -> Self {
        ConnectorBuilder::new(Cfg::default()).build()
    }

    /// Creates a new connector using the default configuration and a single connection wrapper.
    pub fn wrapped(wrapper: impl ConnectingWrapper) -> Self {
        let mut builder = ConnectorBuilder::new(Cfg::default());
        builder.wrap(wrapper);
        builder.build()
    }

    /// Adds a transport.
    pub fn add(&self, transport: impl ConnectingTransport) -> ConnectingTransportHandle {
        let name = transport.name().to_string();

        let (result_tx, result_rx) = oneshot::channel();
        let (remove_tx, remove_rx) = oneshot::channel();

        let pack = TransportPack { transport: Arc::new(transport), result_tx, remove_rx };
        let _ = self.transport_tx.send(pack);

        ConnectingTransportHandle { name, result_rx, remove_tx }
    }

    /// Waits for the connection to be established and obtains the aggregated link channel.
    ///
    /// If this has been called before `None` is returned.
    pub fn channel(&mut self) -> Option<Outgoing> {
        self.outgoing.take()
    }

    /// Obtains the connection control of the aggregated connection.
    pub fn control(&self) -> BoxControl {
        self.control.clone()
    }

    /// Gets the current set of available link tags.
    ///
    /// The set of available tags does not necesarilly match the set of connected tags.
    /// Use the [connection control](Self::control) to obtain and monitor
    /// the set of connected tags.
    pub fn available_tags(&self) -> HashSet<LinkTagBox> {
        self.tags_rx.borrow().clone()
    }

    /// Watches the set of available link tags.
    pub fn available_tags_watch(&self) -> watch::Receiver<HashSet<LinkTagBox>> {
        self.tags_rx.clone()
    }

    /// Sets the set of disabled link tags.
    pub fn set_disabled_tags(&self, disabled_tags: HashSet<LinkTagBox>) {
        self.disabled_tags_tx.send_replace(disabled_tags);
    }

    /// Subscribes to the stream of link errors.
    pub fn link_errors(&self) -> broadcast::Receiver<BoxLinkError> {
        self.error_rx.resubscribe()
    }

    /// Task for handling all transports.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(level="debug", skip_all, fields(id=%control.id()))]
    async fn task(
        control: BoxControl, active_transports: Arc<RwLock<Vec<Weak<dyn ConnectingTransport>>>>,
        mut transport_rx: mpsc::UnboundedReceiver<TransportPack>, tags_tx: watch::Sender<HashSet<LinkTagBox>>,
        disabled_tags_rx: watch::Receiver<HashSet<LinkTagBox>>, link_error_tx: broadcast::Sender<BoxLinkError>,
        reconnect_delay: Duration, wrappers: Vec<BoxConnectingWrapper>,
    ) {
        let wrappers = Arc::new(wrappers);
        let mut transport_tasks = FuturesUnordered::new();
        let mut transport_tags: Vec<watch::Receiver<HashSet<LinkTagBox>>> = Vec::new();

        loop {
            // Remove channels from terminated transports.
            transport_tags.retain(|tt| tt.has_changed().is_ok());

            // Collect and publish tags from all transports.
            let mut all_tags = HashSet::new();
            for tt in &mut transport_tags {
                let tags = tt.borrow_and_update();
                for tag in &*tags {
                    all_tags.insert(tag.clone());
                }
            }
            tags_tx.send_if_modified(|tags| {
                if *tags == all_tags {
                    false
                } else {
                    *tags = all_tags;
                    true
                }
            });

            // Monitor all transport tags for changes.
            let tags_changed = future::select_all(
                transport_tags
                    .iter_mut()
                    .map(|tt| tt.changed().boxed())
                    .chain(iter::once(future::pending().boxed())),
            );

            enum ConnectorEvent {
                TransportAdded(TransportPack),
                TagsChanged,
                TransportTerminated,
            }

            // Wait for event.
            let event = tokio::select! {
                Some(transport_pack) = transport_rx.recv() => ConnectorEvent::TransportAdded(transport_pack),
                _ = tags_changed => ConnectorEvent::TagsChanged,
                Some(()) = transport_tasks.next() => ConnectorEvent::TransportTerminated,
                _ = control.terminated() => {
                    tracing::debug!("connection was terminated");
                    break;
                }
            };

            // Handle event.
            match event {
                ConnectorEvent::TransportAdded(transport_pack) => {
                    // Add transport to list of active transports.
                    let mut active_transports = active_transports.write().await;
                    active_transports.retain(|at| at.strong_count() > 0);
                    active_transports.push(Arc::downgrade(&transport_pack.transport));

                    // Start transport task.
                    let (transport_tags_tx, transport_tags_rx) = watch::channel(HashSet::new());
                    transport_tags.push(transport_tags_rx);
                    transport_tasks.push(Self::transport_task(
                        transport_pack,
                        control.clone(),
                        transport_tags_tx,
                        disabled_tags_rx.clone(),
                        link_error_tx.clone(),
                        reconnect_delay,
                        wrappers.clone(),
                    ));
                }
                ConnectorEvent::TagsChanged => (),
                ConnectorEvent::TransportTerminated => (),
            }
        }
    }

    /// Task for handling a transport.
    #[tracing::instrument(level="debug", skip_all, fields(id=%control.id(), transport=transport_pack.transport.name()))]
    async fn transport_task(
        transport_pack: TransportPack, control: BoxControl, tags_fw_tx: watch::Sender<HashSet<LinkTagBox>>,
        mut disabled_tags_rx: watch::Receiver<HashSet<LinkTagBox>>,
        link_error_tx: broadcast::Sender<BoxLinkError>, reconnect_delay: Duration,
        wrappers: Arc<Vec<BoxConnectingWrapper>>,
    ) {
        let TransportPack { transport, result_tx, mut remove_rx } = transport_pack;
        let conn_id = control.id();
        let mut changed_control = control.clone();

        // Set up channel for getting tags.
        let (tags_tx, mut tags_rx) = watch::channel(HashSet::new());
        let mut tags_task = transport.link_tags(tags_tx);
        let mut tags_changed = true;

        let mut connecting_tags = HashSet::new();
        let mut connecting_tasks = FuturesUnordered::new();
        let mut link_filter_rejected_tags = HashSet::new();

        let res = 'outer: loop {
            {
                // Notify transport of connected links.
                let links = control.links();
                transport.connected_links(&links).await;

                // Get disabled tags and disconnect them.
                let disabled_tags = disabled_tags_rx.borrow_and_update();
                for link in &links {
                    if disabled_tags.contains(link.tag()) {
                        link.start_disconnect();
                    }
                }

                // Get and forward available tags from transport.
                let tags = tags_rx.borrow_and_update().clone();
                if tags_changed {
                    tracing::debug!(
                        "available tags: {}",
                        tags.iter().map(|tag| tag.to_string()).collect::<Vec<_>>().join(", ")
                    );
                    tags_fw_tx.send_replace(tags.clone());
                    tags_changed = false;
                }

                // Connect available but unconnected tags.
                for tag in tags {
                    if tag.transport_name() != transport.name() {
                        break 'outer Err(Error::new(
                            ErrorKind::Other,
                            "link tag transport name mismatch".to_string(),
                        ));
                    }

                    if connecting_tags.contains(&tag)
                        || disabled_tags.contains(&tag)
                        || link_filter_rejected_tags.contains(&tag)
                        || links.iter().any(|link| link.tag() == &tag)
                    {
                        continue;
                    }

                    tracing::debug!("connecting tag: {tag}");
                    connecting_tags.insert(tag.clone());

                    let connect_task = async {
                        // Establish transport connection.
                        tracing::debug!("establishing transport connection for tag {tag}");
                        let mut stream_box = match transport.connect(&*tag).await {
                            Ok(stream_box) => stream_box,
                            Err(err) => {
                                tracing::debug!("connecting transport for tag {tag} failed: {err}");
                                let _ = link_error_tx.send(BoxLinkError::outgoing(conn_id, &tag, err));
                                sleep(reconnect_delay).await;
                                return (tag, None);
                            }
                        };

                        // Apply wrappers to IO stream.
                        for wrapper in &*wrappers {
                            let name = wrapper.name();
                            tracing::debug!("wrapping tag {tag} in {name}");

                            match wrapper.wrap(stream_box).await {
                                Ok(wrapped) => stream_box = wrapped,
                                Err(err) => {
                                    tracing::debug!("wrapping tag {tag} in {name} failed: {err}");
                                    let _ = link_error_tx.send(BoxLinkError::outgoing(conn_id, &tag, err));
                                    sleep(reconnect_delay).await;
                                    return (tag, None);
                                }
                            }
                        }

                        // Add link to aggregated connection.
                        tracing::debug!("adding link for tag {tag} to connection");
                        let TxRxBox { tx, rx } = stream_box.into_tx_rx();
                        let link = match control.add(tx, rx, tag.clone(), &tag.user_data()).await {
                            Ok(link) => link,
                            Err(err) => {
                                tracing::debug!("adding link for tag {tag} to connection failed: {err}");
                                let _ = link_error_tx.send(BoxLinkError::outgoing(conn_id, &tag, err.into()));
                                sleep(reconnect_delay).await;
                                return (tag, None);
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
                        let sleep_until = sleep(reconnect_delay);
                        let reason = link.disconnected().await;
                        tracing::debug!("link for tag {tag} disconnected: {reason}");
                        let _ = link_error_tx.send(BoxLinkError::outgoing(conn_id, &tag, reason.clone().into()));
                        sleep_until.await;

                        (tag, Some(reason))
                    };
                    connecting_tasks.push(connect_task);
                }
            }

            // Handle events.
            tokio::select! {
                res = &mut tags_task => break res,
                Ok(()) = &mut remove_rx => break Ok(()),
                Ok(()) = disabled_tags_rx.changed() => (),
                Ok(()) = tags_rx.changed() => tags_changed = true,
                () = changed_control.links_changed() => (),
                _ = control.terminated() => break Ok(()),
                Some((tag, reason)) = connecting_tasks.next() => {
                    connecting_tags.remove(&tag);
                    match reason {
                        Some(DisconnectReason::LinkFilter) => {
                            tracing::debug!("blocking tag {tag}");
                            link_filter_rejected_tags.insert(tag);
                        }
                        Some(_) => {
                            tracing::debug!("clearing tag block list");
                            link_filter_rejected_tags.clear();
                        }
                        None => (),
                    }
                },
            }
        };

        // Publish result.
        match &res {
            Ok(()) => tracing::debug!("transport terminated"),
            Err(err) => tracing::debug!("transport failed: {err}"),
        }
        let _ = result_tx.send(res);
    }
}

/// A handle to a transport.
///
/// Await this future to be notified when the transport fails.
///
/// Dropping this will not remove the transport from the connectors.
pub struct ConnectingTransportHandle {
    name: String,
    result_rx: oneshot::Receiver<Result<()>>,
    remove_tx: oneshot::Sender<()>,
}

impl fmt::Debug for ConnectingTransportHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ConnectingTransportHandle").field("name", &self.name).finish()
    }
}

impl ConnectingTransportHandle {
    /// Name of the transport.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Removes the transport from the connector.
    pub fn remove(self) {
        let Self { remove_tx, .. } = self;
        let _ = remove_tx.send(());
    }
}

impl IntoFuture for ConnectingTransportHandle {
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
