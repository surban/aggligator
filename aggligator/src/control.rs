//! Connection and link control.

use bytes::Bytes;
use futures::{Sink, Stream};
use std::{
    fmt,
    hash::Hash,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, watch, Mutex},
    time::{error::Elapsed, timeout, Instant},
};

use crate::{
    agg::link_int::LinkInt,
    cfg::Cfg,
    id::{ConnId, LinkId, ServerId},
    io::{IoRx, IoTx},
    msg::{LinkMsg, RefusedReason},
    protocol_err,
};

/// Error adding a link to a connection.
#[derive(Debug)]
pub enum AddLinkError {
    /// IO error.
    Io(io::Error),
    /// The link is connected to a different server than the other links of
    /// this connection.
    ServerIdMismatch {
        /// Expected server id.
        expected: ServerId,
        /// Server id of the remote endpoint.
        present: ServerId,
    },
    /// The server is not accepting new connections.
    NotListening,
    /// The connection was closed.
    ConnectionClosed,
    /// The connection was actively refused.
    ConnectionRefused,
    /// The link was actively refused by the link filter.
    LinkRefused,
}

impl From<io::Error> for AddLinkError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<Elapsed> for AddLinkError {
    fn from(err: Elapsed) -> Self {
        Self::Io(err.into())
    }
}

impl fmt::Display for AddLinkError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AddLinkError::Io(err) => write!(f, "IO error: {err}"),
            AddLinkError::ServerIdMismatch { expected, present } => {
                write!(f, "connected to server {expected} but link connects to server {present}")
            }
            AddLinkError::NotListening => write!(f, "not listening"),
            AddLinkError::ConnectionClosed => write!(f, "connection closed"),
            AddLinkError::ConnectionRefused => write!(f, "connection refused"),
            AddLinkError::LinkRefused => write!(f, "link refused"),
        }
    }
}

impl std::error::Error for AddLinkError {}

impl From<RefusedReason> for AddLinkError {
    fn from(reason: RefusedReason) -> Self {
        match reason {
            RefusedReason::Closed => Self::ConnectionClosed,
            RefusedReason::NotListening => Self::NotListening,
            RefusedReason::ConnectionRefused => Self::ConnectionRefused,
            RefusedReason::LinkRefused => Self::LinkRefused,
        }
    }
}

impl AddLinkError {
    /// Returns whether the connection attempt should be retried.
    pub fn should_reconnect(&self) -> bool {
        matches!(self, Self::Io(_))
    }
}

/// Direction of a connection or link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// Incoming connection or link.
    Incoming,
    /// Outgoing connection or link.
    Outgoing,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Incoming => write!(f, "incoming"),
            Self::Outgoing => write!(f, "outgoing"),
        }
    }
}

/// A handle for controlling and monitoring a connection consisting of aggregated links.
///
/// Clones of this handle refer to the same underlying connection.
/// Dropping this does not terminate the connection.
pub struct Control<TX, RX, TAG> {
    pub(crate) cfg: Arc<Cfg>,
    pub(crate) conn_id: ConnId,
    pub(crate) server_id: Option<ServerId>,
    pub(crate) remote_server_id: Arc<Mutex<Option<ServerId>>>,
    pub(crate) direction: Direction,
    pub(crate) connected: Arc<AtomicBool>,
    pub(crate) link_tx: mpsc::Sender<LinkInt<TX, RX, TAG>>,
    pub(crate) links_rx: watch::Receiver<Vec<Link<TAG>>>,
    pub(crate) stats_rx: watch::Receiver<Stats>,
}

impl<TX, RX, TAG> Clone for Control<TX, RX, TAG> {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            conn_id: self.conn_id,
            server_id: self.server_id,
            remote_server_id: self.remote_server_id.clone(),
            direction: self.direction,
            connected: self.connected.clone(),
            link_tx: self.link_tx.clone(),
            links_rx: self.links_rx.clone(),
            stats_rx: self.stats_rx.clone(),
        }
    }
}

impl<TX, RX, TAG> fmt::Debug for Control<TX, RX, TAG> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Control").field("conn_id", &self.conn_id).finish()
    }
}

impl<TX, RX, TAG> PartialEq for Control<TX, RX, TAG> {
    fn eq(&self, other: &Self) -> bool {
        self.conn_id == other.conn_id
    }
}

impl<TX, RX, TAG> Eq for Control<TX, RX, TAG> {}

impl<TX, RX, TAG> PartialOrd for Control<TX, RX, TAG> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.conn_id.partial_cmp(&other.conn_id)
    }
}

impl<TX, RX, TAG> Ord for Control<TX, RX, TAG> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.conn_id.cmp(&other.conn_id)
    }
}

impl<TX, RX, TAG> Hash for Control<TX, RX, TAG> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.conn_id.hash(state);
    }
}

impl<TX, RX, TAG> Control<TX, RX, TAG> {
    /// The connection id.
    pub fn id(&self) -> ConnId {
        self.conn_id
    }

    /// The server id of the local server.
    ///
    /// `None` if the connection supports only outgoing links.
    pub fn server_id(&self) -> Option<ServerId> {
        self.server_id
    }

    /// The server id of the remote server.
    ///
    /// `None` if the connection is not yet established or supports only incoming links.
    pub async fn remote_server_id(&self) -> Option<ServerId> {
        *self.remote_server_id.lock().await
    }

    /// Direction of the connection.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Returns whether the connection has been terminated.
    pub fn is_terminated(&self) -> bool {
        self.link_tx.is_closed()
    }

    /// Waits until the connection has been terminated.
    pub async fn terminated(&self) {
        self.link_tx.closed().await
    }

    /// Gets handles to all links of the connection.
    pub fn links(&self) -> Vec<Link<TAG>> {
        self.links_rx.borrow().clone()
    }

    /// Waits until the links of the connection have changed.
    pub async fn links_changed(&mut self) {
        let _ = self.links_rx.changed().await;
    }

    /// The current connection statistics.
    pub fn stats(&self) -> Stats {
        self.stats_rx.borrow().clone()
    }

    /// Waits until the connection statistics have been updated.
    pub async fn stats_updated(&mut self) {
        let _ = self.stats_rx.changed().await;
    }
}

impl<TX, RX, TAG> Control<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin,
    TX: Sink<Bytes, Error = io::Error> + Unpin,
    TAG: Send + Sync + 'static,
{
    /// Adds a new outgoing, packet-based link to the connection.
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
    pub async fn add(
        &self, mut tx: TX, mut rx: RX, tag: TAG, user_data: &[u8],
    ) -> Result<Link<TAG>, AddLinkError> {
        assert!(user_data.len() <= u16::MAX as usize, "user_data is too big");

        // Perform protocol handshake.
        let (remote_cfg, roundtrip, remote_user_data) = timeout(self.cfg.link_non_working_timeout, async {
            let LinkMsg::Welcome { extensions: _, server_id, cfg, user_data: remote_user_data } = LinkMsg::recv(&mut rx).await?
                    else { return Err::<_, AddLinkError>(protocol_err!("expected Welcome message").into()) };

            {
                let mut remote_server_id = self.remote_server_id.lock().await;
                match &*remote_server_id {
                    Some(remote_server_id) if *remote_server_id != server_id => {
                        return Err(AddLinkError::ServerIdMismatch {
                            expected: *remote_server_id,
                            present: server_id,
                        });
                    }
                    Some(_) => (),
                    None => {
                        *remote_server_id = Some(server_id);
                    }
                }
            }

            let start = Instant::now();
            LinkMsg::Connect {
                extensions: 0,
                server_id: self.server_id,
                connection_id: self.conn_id,
                existing_connection: self.connected.load(Ordering::Acquire),
                user_data: user_data.to_vec(),
                cfg: (&*self.cfg).into(),
            }
            .send(&mut tx)
            .await?;

            match LinkMsg::recv(&mut rx).await? {
                LinkMsg::Accepted => {
                    self.connected.store(true, Ordering::Release);
                    Ok((cfg, start.elapsed(), remote_user_data))
                }
                LinkMsg::Refused { reason } => Err(reason.into()),
                _ => Err(protocol_err!("expected Accepted or Refused message").into()),
            }
        })
        .await??;

        // Create link.
        let link_int = LinkInt::new(
            tag,
            self.conn_id,
            tx,
            rx,
            self.cfg.clone(),
            remote_cfg,
            Direction::Outgoing,
            roundtrip,
            remote_user_data,
        );
        let link = Link::from(&link_int);
        self.link_tx.send(link_int).await.map_err(|_| AddLinkError::ConnectionClosed)?;

        Ok(link)
    }
}

impl<R, W, TAG> Control<IoTx<W>, IoRx<R>, TAG>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    TAG: Send + Sync + 'static,
{
    /// Adds a new outgoing, stream-based link to the connection.
    ///
    /// The stream-based link is wrapped in the [length-delimited codec](tokio_util::codec::length_delimited)
    /// to make it packet-based.
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
    pub async fn add_io(&self, read: R, write: W, tag: TAG, user_data: &[u8]) -> Result<Link<TAG>, AddLinkError> {
        self.add(IoTx::new(write), IoRx::new(read), tag, user_data).await
    }
}

/// Connection statistics.
#[derive(Default, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct Stats {
    /// Time when the connection was established.
    pub established: Option<Instant>,
    /// Time since when no link of the connection is working.
    pub not_working_since: Option<Instant>,
    /// Available buffer space for sending data.
    pub send_space: usize,
    /// Size of data sent and not yet acknowledged by remote endpoint.
    pub sent_unacked: usize,
    /// Size of data that has been sent and not yet consumed by the remote endpoint.
    pub sent_unconsumed: usize,
    /// Number of packets sent and not yet consumed by the remote endpoint.
    pub sent_unconsumed_count: usize,
    /// Size of data received by remote endpoint that cannot yet be consumed,
    /// because intermediate data has not yet been received.
    pub sent_unconsumable: usize,
    /// Length of the queue for resending lost packets.
    pub resend_queue_len: usize,
    /// Size of data that has been received and not yet consumed.
    pub recved_unconsumed: usize,
    /// Number of packets received and not yet consumed.
    pub recved_unconsumed_count: usize,
}

/// A handle for controlling and monitoring a link.
///
/// Clones of this handle refer to the same underlying link.
/// Dropping this does not terminate the link.
pub struct Link<TAG> {
    pub(crate) conn_id: ConnId,
    pub(crate) link_id: LinkId,
    pub(crate) direction: Direction,
    pub(crate) tag: Arc<TAG>,
    pub(crate) cfg: Arc<Cfg>,
    pub(crate) disconnected_rx: watch::Receiver<DisconnectReason>,
    pub(crate) disconnect_tx: mpsc::Sender<()>,
    pub(crate) stats_rx: watch::Receiver<LinkStats>,
    pub(crate) remote_user_data: Arc<Vec<u8>>,
}

impl<TAG> Clone for Link<TAG> {
    fn clone(&self) -> Self {
        Self {
            conn_id: self.conn_id,
            link_id: self.link_id,
            direction: self.direction,
            tag: self.tag.clone(),
            cfg: self.cfg.clone(),
            disconnected_rx: self.disconnected_rx.clone(),
            disconnect_tx: self.disconnect_tx.clone(),
            stats_rx: self.stats_rx.clone(),
            remote_user_data: self.remote_user_data.clone(),
        }
    }
}

impl<TAG> PartialEq for Link<TAG> {
    fn eq(&self, other: &Self) -> bool {
        self.conn_id == other.conn_id && self.link_id == other.link_id
    }
}

impl<TAG> Eq for Link<TAG> {}

impl<TAG> PartialOrd for Link<TAG> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (&self.conn_id, &self.link_id).partial_cmp(&(&other.conn_id, &other.link_id))
    }
}

impl<TAG> Ord for Link<TAG> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.conn_id, &self.link_id).cmp(&(&other.conn_id, &other.link_id))
    }
}

impl<TAG> Hash for Link<TAG> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (&self.conn_id, &self.link_id).hash(state);
    }
}

impl<TAG> fmt::Debug for Link<TAG>
where
    TAG: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Link")
            .field("id", &self.link_id)
            .field("conn_id", &self.conn_id)
            .field("direction", &self.direction)
            .field("tag", &self.tag)
            .finish()
    }
}

impl<TAG> Link<TAG> {
    /// The link id.
    pub fn id(&self) -> LinkId {
        self.link_id
    }

    /// The connection id.
    pub fn conn_id(&self) -> ConnId {
        self.conn_id
    }

    /// Direction of link.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The user-defined tag of this link.
    ///
    /// This returns the tag that was supplied by the user when establishing this link.
    /// Aggligator does not process the tag.
    pub fn tag(&self) -> &TAG {
        &self.tag
    }

    /// User data provided by remote endpoint when establishing this link.
    ///
    /// This returns the user data provided at the remote endpoint when establishing the link.
    /// Aggligator does not process the user data.
    pub fn remote_user_data(&self) -> &[u8] {
        self.remote_user_data.as_ref()
    }

    /// Returns whether the link is disconnected.
    pub fn is_disconnected(&self) -> bool {
        self.disconnect_reason().is_some()
    }

    /// The reason for why this link has been disconnected.
    ///
    /// `None` if the link is connected.
    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.disconnect_tx.is_closed().then(|| self.disconnected_rx.borrow().clone())
    }

    /// Waits until this link has been disconnected.
    pub async fn disconnected(&self) -> DisconnectReason {
        self.disconnect_tx.closed().await;
        self.disconnected_rx.borrow().clone()
    }

    /// Gracefully disconnects this link.
    ///
    /// Returns when the link has been disconnected.
    pub async fn disconnect(&self) {
        self.start_disconnect();
        self.disconnected().await;
    }

    /// Starts graceful disconnection of this link.
    ///
    /// Returns immediately.
    pub fn start_disconnect(&self) {
        let _ = self.disconnect_tx.try_send(());
    }

    /// The current link statistics.
    pub fn stats(&self) -> LinkStats {
        self.stats_rx.borrow().clone()
    }

    /// Waits until the link statistics have been updated.
    pub async fn stats_updated(&mut self) {
        let _ = self.stats_rx.changed().await;
    }
}

/// Link statistics over a time interval.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct LinkIntervalStats {
    /// Duration of interval.
    pub interval: Duration,
    /// Start time of interval.
    pub start: Instant,
    /// Bytes sent within time interval.
    pub sent: u64,
    /// Bytes received within time interval.
    pub recved: u64,
    /// Whether sending was used to capacity within time interval.
    pub busy: bool,
}

impl LinkIntervalStats {
    pub(crate) fn new(interval: Duration) -> Self {
        Self { interval, start: Instant::now(), sent: 0, recved: 0, busy: true }
    }

    /// Send speed in bytes per second.
    pub fn send_speed(&self) -> f64 {
        self.sent as f64 / self.interval.as_secs_f64()
    }

    /// Receive speed in bytes per second.
    pub fn recv_speed(&self) -> f64 {
        self.recved as f64 / self.interval.as_secs_f64()
    }
}

/// Link statistics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct LinkStats {
    /// Time when link was established.
    pub established: Instant,
    /// Link is working properly and being used.
    pub working: bool,
    /// Total data sent in bytes.
    pub total_sent: u64,
    /// Total data received in bytes.
    pub total_recved: u64,
    /// Current data sent but not yet acknowledged by remote endpoint in bytes.
    pub sent_unacked: u64,
    /// Current limit of [`sent_unacked`](Self::sent_unacked).
    pub unacked_limit: u64,
    /// Round trip duration, i.e. ping.
    pub roundtrip: Duration,
    /// Statistics over time intervals specified in the [configuration](crate::cfg::Cfg::stats_intervals).
    pub time_stats: Vec<LinkIntervalStats>,
}

/// The reason for the disconnection of a link.
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    /// Sending over the link took too long.
    SendTimeout,
    /// Ping reply was not received in time.
    PingTimeout,
    /// The link was unconfirmed for too long.
    UnconfirmedTimeout,
    /// All links were unconfirmed for too long at the same time.
    AllUnconfirmedTimeout,
    /// An IO error occurred on the link.
    IoError(Arc<io::Error>),
    /// Locally requested by calling the [Link::disconnect] method.
    LocallyRequested,
    /// Remotely requested by calling the [Link::disconnect] method.
    RemotelyRequested,
    /// The connection was closed.
    ConnectionClosed,
    /// The link was rejected by the local link filter.
    LinkFilter,
    /// The connection task was terminated.
    TaskTerminated,
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SendTimeout => write!(f, "send timeout"),
            Self::PingTimeout => write!(f, "ping timeout"),
            Self::UnconfirmedTimeout => write!(f, "unconfirmed timeout"),
            Self::AllUnconfirmedTimeout => write!(f, "all links unconfirmed timeout"),
            Self::IoError(err) => write!(f, "IO error: {err}"),
            Self::LocallyRequested => write!(f, "locally requested"),
            Self::RemotelyRequested => write!(f, "remotely requested"),
            Self::ConnectionClosed => write!(f, "connection closed"),
            Self::LinkFilter => write!(f, "link filter"),
            Self::TaskTerminated => write!(f, "task terminated"),
        }
    }
}

impl DisconnectReason {
    /// Returns whether a reconnection should be attempted.
    pub fn should_reconnect(&self) -> bool {
        matches!(self, Self::SendTimeout | Self::PingTimeout | Self::UnconfirmedTimeout | Self::IoError(_))
    }
}