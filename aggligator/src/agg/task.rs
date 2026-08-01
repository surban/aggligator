//! Link aggregator task.

use atomic_refcell::AtomicRefCell;
use bytes::Bytes;
use futures::{
    Future, FutureExt, Sink, Stream, StreamExt, future, future::BoxFuture, stream, stream::FuturesUnordered,
};
use rand::{prelude::*, rngs::SmallRng};
use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    fmt,
    future::IntoFuture,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    select,
    sync::{mpsc, oneshot, watch},
};

use crate::{
    agg::link_int::{DisconnectInitiator, LinkInt, LinkIntEvent, LinkTest},
    alc::{RecvError, SendError},
    cfg::{Cfg, ExchangedCfg, LinkCfg, LinkPing},
    control::{Direction, DisconnectReason, Link, NotWorkingReason, Stats},
    exec::time::{Instant, interval_stream, sleep_until, timeout},
    id::{ConnId, LinkId, OwnedConnId},
    msg::{LinkMsg, RefusedReason, ReliableMsg},
    peekable_mpsc::{PeekableReceiver, RecvIfError},
    protocol_err,
    seq::Seq,
};

/// Number of roundtrip estimates to treat as reliable.
const RELIABLE_ROUNDTRIP_ESTIMATES: usize = 10;

/// Error indicating why a connection of aggregated links failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    /// All links were unconfirmed for too long at the same time.
    AllUnconfirmedTimeout,
    /// No links were available for too long.
    NoLinksTimeout,
    /// A protocol error occured on a link.
    ProtocolError {
        /// Link on which the error occured.
        link_id: LinkId,
        /// Protocol error description.
        error: String,
    },
    /// A link connected to another server than the other links.
    ///
    /// This will occur when the server is restarted while a client is connected.
    ServerIdMismatch,
    /// The connection was forcefully terminated.
    Terminated,
    /// The server aborted the connection while no link was working.
    AbortedByServer,
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::AllUnconfirmedTimeout => write!(f, "all links unconfirmed timeout"),
            Self::NoLinksTimeout => write!(f, "no links available timeout"),
            Self::ProtocolError { link_id, error } => write!(f, "protocol error on link {link_id}: {error}"),
            Self::ServerIdMismatch => write!(f, "a new link connected to another server"),
            Self::Terminated => write!(f, "connection forcefully terminated"),
            Self::AbortedByServer => write!(f, "connection aborted by server"),
        }
    }
}

impl Error for TaskError {}

impl From<TaskError> for std::io::Error {
    fn from(err: TaskError) -> Self {
        io::Error::new(io::ErrorKind::ConnectionAborted, err)
    }
}

/// Fatal error during connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FatalConnectError {
    /// A link connected to another server than the other links.
    ServerIdMismatch,
    /// The remote endpoint indicated that the connection is already closed.
    Closed,
}

/// A send request to the link aggregator task.
#[derive(Debug)]
pub(crate) enum SendReq {
    /// Send data.
    Send(Bytes),
    /// Flush.
    Flush(oneshot::Sender<()>),
}

/// Send overrun handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendOverrun {
    /// Send overrun handling is armed.
    Armed,
    /// Soft handling has occurred.
    Soft,
    /// Hard handling has occurred.
    Hard,
}

/// A sent reliable packet.
#[derive(Clone)]
pub(super) struct SentReliable {
    /// Sequence number.
    pub seq: Seq,
    /// Status.
    pub status: AtomicRefCell<SentReliableStatus>,
}

impl fmt::Debug for SentReliable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SentReliable")
            .field("seq", &self.seq)
            .field("status", &self.status.try_borrow().map(|b| (*b).clone()))
            .finish()
    }
}

/// Status of a sent reliable packet.
#[derive(Debug, Clone)]
pub(super) enum SentReliableStatus {
    /// Message was sent, but its reception is not yet confirmed.
    Sent {
        /// Time packet was sent.
        sent: Instant,
        /// Time packet was flushed.
        flushed: Option<Instant>,
        /// Index of link used to send the packet.
        link_id: usize,
        /// Id of link used to send the packet.
        link: LinkId,
        /// Sent message.
        msg: ReliableMsg,
        /// Whether packet has been resent.
        resent: bool,
    },
    /// Message was received by remote endpoint.
    Received {
        /// Size of data.
        size: usize,
    },
    /// Message has been queued for resending.
    ResendQueued {
        /// Message for resending.
        msg: ReliableMsg,
        /// Id of original link used to send the packet.
        sent_link: LinkId,
    },
}

/// Received reliable message.
#[derive(Debug, Clone)]
struct ReceivedReliableMsg {
    /// Sequence number.
    seq: Seq,
    /// Message.
    msg: ReliableMsg,
}

/// Link aggregator task event.
enum TaskEvent<TX, RX, TAG> {
    /// Immediate termination.
    Terminate,
    /// A new link has been established.
    NewLink(Box<LinkInt<TX, RX, TAG>>),
    /// No new links will be established.
    NoNewLinks,
    /// A link event occurred.
    LinkEvent { id: usize, event: LinkIntEvent },
    /// Data to send over an idle link has been received.
    WriteRx { id: usize, data: Bytes },
    /// No more data to send will be received.
    WriteEnd,
    /// Flush.
    Flush(oneshot::Sender<()>),
    /// Confirmation of sent packet over specified link timed out.
    ConfirmTimedOut(usize),
    /// Resend packet over an idle link.
    Resend(Arc<SentReliable>),
    /// Data consumer was dropped.
    ReadDropped,
    /// Data consumer was closed.
    ReadClosed,
    /// Received data has been consumed.
    ConsumeReceived { received: ReceivedReliableMsg, permit: Option<mpsc::OwnedPermit<Bytes>> },
    /// Space for sending a queued ack has become available.
    SendConsumed,
    /// Ping a link.
    PingLink(usize),
    /// Link was unconfirmed for too long.
    LinkUnconfirmedTimeout(usize),
    /// Sending over link timed out.
    LinkSendTimeout(usize),
    /// Timeout waiting for ping reply over link.
    LinkPingTimeout(usize),
    /// A link requires testing.
    LinkTesting,
    /// No working links within timeout.
    NoLinksTimeout,
    /// Publish link statistics.
    PublishLinkStats,
    /// A refused link task completed.
    RefusedLinkTask,
    /// A fatal connect error occurred.
    FatalConnectError(FatalConnectError),
}

/// Forceful connection termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendTerminate {
    /// No termination.
    None,
    /// Initiate termination.
    Initiate,
    /// Reply to received termination request.
    Reply,
}

/// Link filter function type.
type LinkFilterFn<TAG> = Box<dyn FnMut(Link<TAG>, Vec<Link<TAG>>) -> BoxFuture<'static, bool> + Send>;

/// Task managing a connection of aggregated links.
///
/// This manages a connection of aggregated links and must be executed
/// (for example using [`tokio::spawn`]) for the connection to work.
///
/// It returns when the connection has been terminated.
/// Dropping this causes immediate termination of the connection.
#[must_use = "the link aggregator task must be run for the connection to work"]
pub struct Task<TX, RX, TAG> {
    /// Local configuration.
    cfg: Arc<Cfg>,
    /// Configuration of remote endpoint.
    /// `None` if not connected yet.
    remote_cfg: Option<Arc<ExchangedCfg>>,
    /// Connection identifier.
    conn_id: OwnedConnId,
    /// Connection direction.
    direction: Direction,
    /// Channel for receiving an immediate termination request.
    terminate_rx: mpsc::Receiver<()>,
    /// Established links.
    links: Vec<Option<LinkInt<TX, RX, TAG>>>,
    /// Channel for receiving newly established links.
    link_rx: Option<mpsc::Receiver<LinkInt<TX, RX, TAG>>>,
    /// Channel for publishing current set of links.
    links_tx: watch::Sender<Vec<Link<TAG>>>,
    /// Since when no link is working.
    links_not_working_since: Option<Instant>,
    /// Channel for notifying that a connection has been established.
    connected_tx: Option<oneshot::Sender<Arc<ExchangedCfg>>>,
    /// Channel for sending received message to user.
    read_tx: Option<mpsc::Sender<Bytes>>,
    /// Channel to receive message from user that receive channel should be closed.
    read_closed_rx: Option<mpsc::Receiver<()>>,
    /// ReceiveClose message has been sent.
    receive_close_sent: bool,
    /// ReceiveFinish message has been sent.
    receive_finish_sent: bool,
    /// Channel for receiving messages to send from user.
    write_rx: Option<PeekableReceiver<SendReq>>,
    /// Whether remote endpoint closed its receiver.
    write_closed: Arc<AtomicBool>,
    /// SendFinish message has been sent.
    send_finish_sent: bool,
    /// Error for reading.
    read_error_tx: watch::Sender<Option<RecvError>>,
    /// Error for writing.
    write_error_tx: watch::Sender<SendError>,
    /// Next data sequence number for sending.
    tx_seq: Seq,
    /// Send overrun handling.
    tx_overrun: SendOverrun,
    /// Since when send overrun condition is active.
    tx_overrun_since: Option<Instant>,
    /// Packets that have been sent but not yet become consumable by the remote endpoint.
    txed_packets: VecDeque<Arc<SentReliable>>,
    /// Size of data sent and not yet acknowledged by remote endpoint.
    txed_unacked: usize,
    /// Size of data that has been sent and not yet consumed by the remote endpoint.
    txed_unconsumed: usize,
    /// Size of data received by remote endpoint that cannot yet be consumed.
    txed_unconsumable: usize,
    /// Sequence number of last packet consumed by the remote endpoint.
    txed_last_consumed: Seq,
    /// Queue of packets that have been declared lost and must be send again.
    resend_queue: VecDeque<Arc<SentReliable>>,
    /// Ids of links that are ready to send data.
    idle_links: Vec<usize>,
    /// Next data sequence number for handing out.
    rx_seq: Seq,
    /// Received message parts, with sequence numbers starting at `rx_seq`.
    rxed_reliable: VecDeque<Option<ReceivedReliableMsg>>,
    /// Received data message parts, ready for consumption.
    rxed_reliable_consumable: VecDeque<ReceivedReliableMsg>,
    /// Sum of size of all buffers in `rxed_reliable` and `rxed_reliable_consumable`.
    rxed_reliable_size: usize,
    /// Size of that that has been consumed since last acknowledgement.
    rxed_reliable_consumed_since_last_ack: usize,
    /// Forces acking consumed data.
    rxed_reliable_consumed_force_ack: bool,
    /// Ids of links that are currently being flushed by user request.
    unflushed_links: HashSet<usize>,
    /// Channel for sending notification when flushing completed.
    flushed_tx: Option<oneshot::Sender<()>>,
    /// Time when task was started.
    start_time: Instant,
    /// Time when both read_tx and write_rx became None.
    read_write_closed: Option<Instant>,
    /// Time when connection was established.
    established: Option<Instant>,
    /// Channel for sending connection statistics.
    stats_tx: watch::Sender<Stats>,
    /// Time when connection statistics were last sent.
    stats_last_sent: Instant,
    /// Filter function for new links.
    link_filter: LinkFilterFn<TAG>,
    /// Links provided at creation of this task.
    init_links: VecDeque<LinkInt<TX, RX, TAG>>,
    /// Tasks handling refused links.
    refused_links_tasks: FuturesUnordered<BoxFuture<'static, ()>>,
    /// Fatal error notification.
    fatal_connect_error_rx: mpsc::Receiver<FatalConnectError>,
    /// Result of task sender.
    result_tx: watch::Sender<Result<(), TaskError>>,
    /// Channel for sending analysis data.
    #[cfg(feature = "dump")]
    dump_tx: Option<mpsc::Sender<super::dump::ConnDump>>,
}

impl<TX, RX, TAG> fmt::Debug for Task<TX, RX, TAG> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Task({}{:?})", self.direction.arrow(), self.conn_id)
    }
}

impl<TX, RX, TAG> Task<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    TAG: fmt::Display + Send + Sync + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cfg: Arc<Cfg>, remote_cfg: Option<Arc<ExchangedCfg>>, conn_id: OwnedConnId, direction: Direction,
        terminate_rx: mpsc::Receiver<()>, links_tx: watch::Sender<Vec<Link<TAG>>>,
        link_rx: mpsc::Receiver<LinkInt<TX, RX, TAG>>, connected_tx: oneshot::Sender<Arc<ExchangedCfg>>,
        read_tx: mpsc::Sender<Bytes>, read_closed_rx: mpsc::Receiver<()>, write_rx: mpsc::Receiver<SendReq>,
        read_error_tx: watch::Sender<Option<RecvError>>, write_error_tx: watch::Sender<SendError>,
        stats_tx: watch::Sender<Stats>, fatal_connect_error_rx: mpsc::Receiver<FatalConnectError>,
        result_tx: watch::Sender<Result<(), TaskError>>, links: Vec<LinkInt<TX, RX, TAG>>,
    ) -> Self {
        Self {
            cfg,
            remote_cfg,
            conn_id,
            direction,
            terminate_rx,
            links: Vec::new(),
            link_rx: Some(link_rx),
            links_tx,
            links_not_working_since: None,
            connected_tx: Some(connected_tx),
            read_tx: Some(read_tx),
            read_closed_rx: Some(read_closed_rx),
            receive_close_sent: false,
            receive_finish_sent: false,
            write_rx: Some(write_rx.into()),
            write_closed: Arc::new(AtomicBool::new(false)),
            send_finish_sent: false,
            read_error_tx,
            write_error_tx,
            tx_seq: Seq::ZERO,
            tx_overrun: SendOverrun::Armed,
            tx_overrun_since: None,
            txed_packets: VecDeque::new(),
            txed_unacked: 0,
            resend_queue: VecDeque::new(),
            idle_links: Vec::new(),
            rx_seq: Seq::ZERO,
            rxed_reliable: VecDeque::new(),
            rxed_reliable_consumable: VecDeque::new(),
            rxed_reliable_consumed_since_last_ack: 0,
            txed_unconsumed: 0,
            txed_unconsumable: 0,
            txed_last_consumed: Seq::MINUS_ONE,
            rxed_reliable_size: 0,
            rxed_reliable_consumed_force_ack: false,
            unflushed_links: HashSet::new(),
            flushed_tx: None,
            start_time: Instant::now(),
            read_write_closed: None,
            established: None,
            stats_tx,
            stats_last_sent: Instant::now(),
            link_filter: Box::new(|_, _| async { true }.boxed()),
            init_links: links.into(),
            refused_links_tasks: FuturesUnordered::new(),
            fatal_connect_error_rx,
            result_tx,
            #[cfg(feature = "dump")]
            dump_tx: None,
        }
    }

    /// Runs the task that manages the connection of aggregated links.
    ///
    /// This returns when the connection has been terminated.
    /// Cancelling the returned future leads to immediate termination of the connection.
    #[tracing::instrument(name = "aggligator::connection", level = "info", skip_all, 
                          fields(conn_id =? self.conn_id, dir =% self.direction), ret)]
    pub async fn run(mut self) -> Result<(), TaskError> {
        tracing::debug!(cfg =? self.cfg, "link aggregator task starting");
        self.start_time = Instant::now();

        let mut stat_timers = stream::select_all(self.cfg.stats_intervals.iter().map(|t| interval_stream(*t)));

        let mut fast_rng = SmallRng::seed_from_u64(1);

        // Termination reasons when exiting main loop.
        let read_term;
        let write_term;
        let link_term;
        let mut send_terminate = SendTerminate::None;
        let result;

        // Main loop.
        loop {
            let is_consume_ack_required = self.is_consume_ack_required();
            let tx_seq_avail = self.tx_seq_avail();
            let tx_space = self.tx_space();
            let resending = !self.resend_queue.is_empty();
            let links_idling = !self.idle_links.is_empty();
            let links_available = self.links.iter().any(Option::is_some);

            // Send statistics and dump.
            self.send_stats();
            #[cfg(feature = "dump")]
            self.send_dump();

            if !tx_seq_avail {
                tracing::debug!("no sequence number available for sending");
            }

            // Check for graceful disconnection because sender and receiver have both been dropped,
            // either locally or remotely.
            if self.read_tx.is_none() && self.write_rx.is_none() {
                let since = self.read_write_closed.get_or_insert_with(Instant::now);

                if (self.txed_packets.is_empty()
                    && self.txed_unconsumed == 0
                    && self.rxed_reliable_size == 0
                    && self.rxed_reliable_consumed_since_last_ack == 0
                    && self.send_finish_sent
                    && self.receive_finish_sent)
                    || !links_available
                    || since.elapsed() >= self.cfg.termination_timeout
                {
                    tracing::info!("disconnecting because sender and receiver were dropped");
                    result = Ok(());
                    read_term = None;
                    write_term = SendError::Closed;
                    link_term = DisconnectReason::ConnectionClosed;
                    break;
                }
            }

            // Check for forceful disconnection because no links are available anymore and no
            // new links can be established.
            if !links_available && self.link_rx.is_none() {
                tracing::warn!("disconnecting because no links available and none can be added");
                result = Err(TaskError::AllUnconfirmedTimeout);
                read_term = Some(RecvError::AllLinksFailed);
                write_term = SendError::AllLinksFailed;
                link_term = DisconnectReason::AllUnconfirmedTimeout;
                break;
            }

            // Notify that connection has been established.
            if links_available && let Some(connected_tx) = self.connected_tx.take() {
                tracing::debug!("sending connection established notification");
                let _ = connected_tx.send(self.remote_cfg.clone().unwrap());
                self.established = Some(Instant::now());
            }

            // Notify that flushing has completed.
            if self.unflushed_links.is_empty()
                && let Some(tx) = self.flushed_tx.take()
            {
                tracing::trace!("flush request completed");
                let _ = tx.send(());
            }

            // Check link limits and unconfirm if exceeded.
            self.check_link_limits();

            // Adjust link transmit buffer limits.
            self.adjust_link_tx_limits();

            // Timeout for no working links.
            let no_link_since = self.links_not_working_since();
            let no_link_timeout = self.cfg.no_link_timeout;
            let links_timeout = async move {
                match no_link_since {
                    Some(since) => sleep_until(since + no_link_timeout).await,
                    None => future::pending().await,
                }
            };

            // Timeout for sending next ping.
            let next_link_ping = self.next_link_ping();
            let next_ping_timeout = async move {
                match next_link_ping {
                    Some((link_id, timeout)) => {
                        sleep_until(timeout).await;
                        link_id
                    }
                    None => future::pending().await,
                }
            };

            // Timeout for expecting ping reply.
            let next_pong_timeout = self
                .earliest_link_specific_timeout(|link_cfg| link_cfg.ping_timeout, |link| link.current_ping_sent);

            // Timeout for removing an unconfirmed link.
            let next_unconfirmed_timeout = self.earliest_link_specific_timeout(
                |link_cfg| link_cfg.non_working_timeout,
                |link| {
                    link.unconfirmed().as_ref().and_then(|(since, reason)| {
                        (*reason != NotWorkingReason::MaxPingExceeded).then_some(*since)
                    })
                },
            );

            // Timeout for removing a link that takes too long to send data.
            let next_send_timeout =
                self.earliest_link_specific_timeout(|link_cfg| link_cfg.ping_timeout, |link| link.tx_polling());

            // Timeout for next link testing step.
            let next_link_testing = (0..self.links.len()).filter_map(|id| self.link_testing_step(id)).min();
            let link_testing_timeout = async move {
                match next_link_testing {
                    Some(timeout) => sleep_until(timeout).await,
                    None => future::pending().await,
                }
            };

            // Timeout for receiving acknowledgement for sent packet.
            let earliest_confirm_timeout = self.earliest_confirm_timeout();
            let recv_confirm_timeout = async move {
                match earliest_confirm_timeout {
                    Some((link_id, timeout, flushed)) => {
                        sleep_until(timeout).await;
                        if flushed {
                            TaskEvent::ConfirmTimedOut(link_id)
                        } else {
                            TaskEvent::LinkEvent { id: link_id, event: LinkIntEvent::FlushRequired }
                        }
                    }
                    None => future::pending().await,
                }
            };

            // Task waiting for termination request.
            let terminate_task = async {
                match self.terminate_rx.recv().await {
                    Some(()) => TaskEvent::Terminate,
                    None => future::pending().await,
                }
            };

            // Task for receiving a new link.
            let new_link_task = async {
                match &mut self.link_rx {
                    _ if !self.init_links.is_empty() => {
                        TaskEvent::NewLink(Box::new(self.init_links.pop_front().unwrap()))
                    }
                    Some(link_rx) => match link_rx.recv().await {
                        Some(link) => TaskEvent::NewLink(Box::new(link)),
                        None => TaskEvent::NoNewLinks,
                    },
                    None => future::pending().await,
                }
            };

            // Determine available links for (re-)sending.
            let mut sendable_idle_link_id = None;
            let mut resendable_idle_link_id = None;
            let non_resendable_link = self.resend_queue.front().map(|sent| {
                let SentReliableStatus::ResendQueued { sent_link, .. } = &*sent.status.borrow() else {
                    unreachable!("message in wrong state in resend queue")
                };
                *sent_link
            });
            for &id in self.idle_links.iter().rev() {
                if sendable_idle_link_id.is_some()
                    && (resendable_idle_link_id.is_some() || non_resendable_link.is_none())
                {
                    break;
                }

                let link = self.links[id].as_ref().unwrap();
                if link.is_sendable() {
                    if sendable_idle_link_id.is_none() {
                        sendable_idle_link_id = Some(id);
                    }

                    if resendable_idle_link_id.is_none() && non_resendable_link != Some(link.link_id()) {
                        resendable_idle_link_id = Some(id);
                    }
                }
            }

            // Task for receiving requests from sender.
            let write_rx_task = async {
                if links_idling && is_consume_ack_required {
                    TaskEvent::SendConsumed
                } else {
                    match &mut self.write_rx {
                        Some(write_rx) if tx_seq_avail && !resending => {
                            match write_rx
                                .recv_if(|msg| match msg {
                                    SendReq::Send(data) => {
                                        data.len() <= tx_space && sendable_idle_link_id.is_some()
                                    }
                                    SendReq::Flush(_) => true,
                                })
                                .await
                            {
                                Ok(SendReq::Send(data)) => {
                                    TaskEvent::WriteRx { id: sendable_idle_link_id.unwrap(), data }
                                }
                                Ok(SendReq::Flush(flushed_tx)) => TaskEvent::Flush(flushed_tx),
                                Err(RecvIfError::NoMatch) => future::pending().await,
                                Err(RecvIfError::Disconnected) => TaskEvent::WriteEnd,
                            }
                        }
                        _ => future::pending().await,
                    }
                }
            };

            // Task for receiving link events.
            let link_task = async {
                if self.links.is_empty() {
                    future::pending().await
                } else {
                    let mut tasks: Vec<_> = self
                        .links
                        .iter_mut()
                        .enumerate()
                        .filter_map(|(id, link_opt)| {
                            link_opt.as_mut().map(|link| async move { (id, link.event().await) }.boxed())
                        })
                        .collect();
                    tasks.shuffle(&mut fast_rng);
                    future::select_all(tasks).await
                }
            };

            // Task for notification when receiver is closed.
            let read_closed_task = async {
                match &mut self.read_closed_rx {
                    Some(read_closed_tx) => match read_closed_tx.recv().await {
                        Some(_) => TaskEvent::ReadClosed,
                        None => future::pending().await,
                    },
                    None => future::pending().await,
                }
            };

            // Task for resending unacknowledged messages.
            let resend_task = async {
                if resending && resendable_idle_link_id.is_some() {
                    self.resend_queue.pop_front().unwrap()
                } else {
                    future::pending().await
                }
            };

            // Task for forwarding received data to receiver.
            let consume_task = async {
                if !self.rxed_reliable_consumable.is_empty() {
                    match self.read_tx.as_ref() {
                        Some(read_tx) => match read_tx.clone().reserve_owned().await {
                            Ok(permit) => TaskEvent::ConsumeReceived {
                                received: self.rxed_reliable_consumable.pop_front().unwrap(),
                                permit: Some(permit),
                            },
                            Err(_) => TaskEvent::ReadDropped,
                        },
                        None => TaskEvent::ConsumeReceived {
                            received: self.rxed_reliable_consumable.pop_front().unwrap(),
                            permit: None,
                        },
                    }
                } else {
                    future::pending().await
                }
            };

            // Wait for next event.
            let event = select! {
                terminate_event = terminate_task => terminate_event,
                new_link_event = new_link_task => new_link_event,
                ((id, event), _, _) = link_task => TaskEvent::LinkEvent { id, event },
                write_event = write_rx_task => write_event,
                recv_confirm_wimeout_event = recv_confirm_timeout => recv_confirm_wimeout_event,
                link_id = next_ping_timeout => TaskEvent::PingLink(link_id),
                link_id = next_pong_timeout => TaskEvent::LinkPingTimeout(link_id),
                link_id = next_unconfirmed_timeout => TaskEvent::LinkUnconfirmedTimeout(link_id),
                link_id = next_send_timeout => TaskEvent::LinkSendTimeout(link_id),
                packet = resend_task => TaskEvent::Resend (packet),
                consume_event = consume_task => consume_event,
                event = read_closed_task => event,
                () = link_testing_timeout => TaskEvent::LinkTesting,
                () = links_timeout => TaskEvent::NoLinksTimeout,
                Some(_) = stat_timers.next() => TaskEvent::PublishLinkStats,
                Some(()) = self.refused_links_tasks.next(), if !self.refused_links_tasks.is_empty()
                    => TaskEvent::RefusedLinkTask,
                Some(err) = self.fatal_connect_error_rx.recv() => TaskEvent::FatalConnectError(err),
            };

            // Handle event.
            match event {
                TaskEvent::Terminate => {
                    tracing::info!("forceful connection termination by local request");
                    result = Err(TaskError::Terminated);
                    read_term = Some(RecvError::TaskTerminated);
                    write_term = SendError::TaskTerminated;
                    link_term = DisconnectReason::TaskTerminated;
                    send_terminate = SendTerminate::Initiate;
                    break;
                }

                TaskEvent::NewLink(mut link) => {
                    let link_id = link.link_id();
                    let tag = link.tag();
                    if self.remote_cfg.is_none() {
                        let remote_cfg = link.remote_cfg();
                        tracing::debug!(?remote_cfg, "obtained remote configuration");
                        self.remote_cfg = Some(remote_cfg);
                    }
                    let others =
                        self.links.iter().filter_map(|link_opt| link_opt.as_ref().map(Link::from)).collect();
                    if (self.link_filter)(Link::from(&*link), others).await {
                        tracing::info!(?link_id, %tag, "adding new link");
                        self.add_link(*link);
                    } else {
                        tracing::debug!(?link_id, %tag, "link was refused by link filter");
                        let link_non_working_timeout = link.link_cfg().non_working_timeout;
                        if link.needs_tx_accepted {
                            self.refused_links_tasks.push(
                                async move {
                                    let _ = timeout(
                                        link_non_working_timeout,
                                        link.send_msg_and_flush(LinkMsg::Refused {
                                            reason: RefusedReason::LinkRefused,
                                        }),
                                    )
                                    .await;
                                    link.notify_disconnected(DisconnectReason::LinkFilter);
                                }
                                .boxed(),
                            );
                        } else {
                            link.notify_disconnected(DisconnectReason::LinkFilter);
                        }
                    }
                }

                TaskEvent::NoNewLinks => {
                    tracing::debug!("no new links can be added");
                    self.link_rx = None;
                }

                TaskEvent::LinkEvent { id, event } => {
                    let link = self.links[id].as_ref().unwrap();
                    let link_id = link.link_id();
                    match event {
                        LinkIntEvent::TxReady => {
                            // Link is ready to send more data.
                            let link = self.links[id].as_mut().unwrap();
                            let link_blocked = link.blocked.load(Ordering::Relaxed);
                            if link.needs_tx_accepted {
                                tracing::debug!(?link_id, tag =% link.tag(), "sending Accepted over link");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Accepted, None);
                                link.needs_tx_accepted = false;
                            } else if link.send_pong {
                                tracing::trace!(?link_id, tag =% link.tag(), "sending Pong over link");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Pong, None);
                                link.send_pong = false;
                            } else if let Some(initiator) = link.disconnecting {
                                if !link.goodbye_sent {
                                    tracing::debug!(?link_id, tag =% link.tag(), "sending GoodBye over link");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    link.start_send_msg(LinkMsg::Goodbye, None);
                                    link.goodbye_sent = true;
                                } else if initiator == DisconnectInitiator::Remote {
                                    // All outstanding messages and Goodbye have been sent and flushed,
                                    // thus we can now disconnect the link.
                                    tracing::info!(?link_id, tag =% link.tag(), "removing link by remote request");
                                    self.remove_link(id, DisconnectReason::RemotelyRequested);
                                }
                            } else if link.send_ping {
                                tracing::trace!(?link_id, tag =% link.tag(), "sending Ping over link");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Ping, None);
                                link.current_ping_sent = Some(Instant::now());
                                link.send_ping = false;
                            } else if link_blocked != link.blocked_sent {
                                tracing::debug!(?link_id, tag =% link.tag(), %link_blocked, "local block status of link changed");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::SetBlock { blocked: link_blocked }, None);
                                link.blocked_sent = link_blocked;
                            } else if let Some(recved_seq) = link.tx_ack_queue.pop_front() {
                                tracing::trace!(?link_id, tag =% link.tag(), "acking sequence {recved_seq} over non-idle link");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Ack { received: recved_seq }, None);
                            } else if link.unconfirmed().is_none() && !link.is_blocked() {
                                // This is a link that is believed to be working, so we can submit
                                // reliable messages over it. Do so by priority.
                                if is_consume_ack_required {
                                    let consumed = self.rxed_reliable_consumed_since_last_ack as u32;
                                    tracing::trace!(
                                        ?link_id, tag =% link.tag(),
                                        "acking {consumed} consumed bytes over non-idle link"
                                    );
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::Consumed(consumed));
                                    self.rxed_reliable_consumed_since_last_ack = 0;
                                    self.rxed_reliable_consumed_force_ack = false;
                                } else if resending
                                    && link.is_sendable()
                                    && Some(link.link_id()) != non_resendable_link
                                {
                                    let packet = self.resend_queue.pop_front().unwrap();
                                    tracing::trace!(
                                        ?link_id, tag =% link.tag(),
                                        "resending packet {} over non-idle link",
                                        packet.seq
                                    );
                                    self.idle_links.retain(|idle_id| *idle_id != id);
                                    self.resend_reliable_over_link(id, packet);
                                } else if self.read_closed_rx.is_none() && !self.receive_close_sent {
                                    tracing::trace!(?link_id, tag =% link.tag(), "sending ReceiveClose over non-idle link");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::ReceiveClose);
                                    self.receive_close_sent = true;
                                } else if self.read_tx.is_none() && !self.receive_finish_sent {
                                    tracing::trace!(?link_id, tag =% link.tag(), "sending ReceiveFinish over non-idle link");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::ReceiveFinish);
                                    self.receive_finish_sent = true;
                                } else if self.write_rx.is_none() && !self.send_finish_sent {
                                    tracing::trace!(?link_id, tag =% link.tag(), "sending SendFinish over non-idle link");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::SendFinish);
                                    self.send_finish_sent = true;
                                } else if let Some(SendReq::Send(data)) = self
                                    .write_rx
                                    .as_mut()
                                    .filter(|_| tx_seq_avail && link.is_sendable())
                                    .and_then(|rx| {
                                        rx.try_recv_if(
                                            |msg| matches!(msg, SendReq::Send(data) if data.len() <= tx_space),
                                        )
                                        .ok()
                                    })
                                {
                                    tracing::trace!(
                                        ?link_id, tag =% link.tag(),
                                        "sending data of size {} over non-idle link",
                                        data.len()
                                    );
                                    self.idle_links.retain(|idle_id| *idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::Data(data));
                                } else if link.needs_flush() && !link.is_sendable() {
                                    tracing::trace!(?link_id, tag =% link.tag(), "flushing link because it is not sendable");
                                    self.flush_link(id);
                                } else if !self.idle_links.contains(&id) {
                                    // Store link in idle list.
                                    tracing::trace!(?link_id, tag =% link.tag(), "link has become idle");
                                    link.mark_idle();
                                    self.idle_links.push(id);
                                }
                            } else {
                                // Link is unconfirmed, make sure it is flushed.
                                if link.needs_flush() || link.need_ack_flush() {
                                    tracing::trace!(?link_id, tag =% link.tag(), "flushing link because it is now unconfirmed");
                                    self.flush_link(id);
                                }
                            }
                        }

                        LinkIntEvent::TxFlushed => {
                            // Link has completed flushing.
                            self.unflushed_links.remove(&id);
                        }

                        LinkIntEvent::Rx { msg, data } => {
                            // Link has received a message.
                            match self.handle_received_msg(id, msg, data) {
                                Ok(false) => (),
                                Ok(true) => {
                                    tracing::info!("forceful connection termination by remote endpoint");
                                    result = Err(TaskError::Terminated);
                                    read_term = Some(RecvError::TaskTerminated);
                                    write_term = SendError::TaskTerminated;
                                    link_term = DisconnectReason::TaskTerminated;
                                    send_terminate = SendTerminate::Reply;
                                    break;
                                }
                                Err(err) => {
                                    let link = self.links[id].as_ref().unwrap();
                                    tracing::warn!(
                                        link_id =? link.link_id(), tag =% link.tag(),
                                        %err, "link caused protocol error"
                                    );
                                    result = Err(TaskError::ProtocolError { link_id, error: err.to_string() });
                                    read_term = Some(RecvError::ProtocolError);
                                    write_term = SendError::ProtocolError;
                                    link_term = DisconnectReason::ProtocolError(err.to_string());
                                    break;
                                }
                            }
                        }

                        LinkIntEvent::FlushRequired => {
                            // Link requires send buffer flushing.
                            tracing::trace!(?link_id, tag =% link.tag(), "flushing link");
                            self.flush_link(id);
                        }

                        LinkIntEvent::TxError(err) | LinkIntEvent::RxError(err) => {
                            // Link has failed.
                            tracing::warn!(?link_id, tag =% link.tag(), %err, "disconnecting link due to IO error");
                            let reason = if self.read_tx.is_none() && self.write_rx.is_none() {
                                DisconnectReason::ConnectionClosed
                            } else {
                                DisconnectReason::IoError(Arc::new(err))
                            };
                            self.remove_link(id, reason);
                        }

                        LinkIntEvent::BlockedChanged => {
                            // Local link blocking has changed.
                            let link = self.links[id].as_mut().unwrap();
                            self.idle_links.retain(|&idle_id| idle_id != id);
                            link.report_ready();
                            link.blocked_changed_out_tx.send_replace(());
                        }

                        LinkIntEvent::LinkCfgChanged => {
                            // Link configuration was changed.
                            let link = self.links[id].as_mut().unwrap();
                            link.update_link_cfg();
                        }

                        LinkIntEvent::Disconnect => {
                            // Local request to disconnect link.
                            let link = self.links[id].as_mut().unwrap();
                            if link.disconnecting.is_none() {
                                tracing::info!(?link_id, tag =% link.tag(), "starting disconnection of link by local request");
                                link.disconnecting = Some(DisconnectInitiator::Local);
                                self.unconfirm_link(id, NotWorkingReason::Disconnecting);
                            }
                        }
                    }
                }

                TaskEvent::WriteRx { id, data } => {
                    let link = self.links[id].as_ref().unwrap();
                    tracing::trace!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "sending data of size {} bytes over idle link", data.len()
                    );
                    self.idle_links.retain(|&idle_id| idle_id != id);
                    self.send_reliable_over_link(id, ReliableMsg::Data(data));
                }

                TaskEvent::SendConsumed => {
                    let id = self.idle_links.pop().unwrap();
                    let link = self.links[id].as_ref().unwrap();
                    let consumed = self.rxed_reliable_consumed_since_last_ack as u32;
                    tracing::trace!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "acking {consumed} consumed bytes over idle link"
                    );
                    self.send_reliable_over_link(id, ReliableMsg::Consumed(consumed));
                    self.rxed_reliable_consumed_since_last_ack = 0;
                    self.rxed_reliable_consumed_force_ack = false;
                }

                TaskEvent::WriteEnd => {
                    tracing::debug!("sender was dropped");
                    self.write_rx = None;
                    if let Some(id) = self.idle_links.pop() {
                        let link = self.links[id].as_ref().unwrap();
                        tracing::debug!(
                            link_id =? link.link_id(), tag =% link.tag(),
                            "sending SendFinish over idle link"
                        );
                        self.send_reliable_over_link(id, ReliableMsg::SendFinish);
                        self.send_finish_sent = true;
                    } else {
                        tracing::debug!("queueing sending of SendFinish");
                    }
                }

                TaskEvent::Flush(tx) => {
                    tracing::trace!("starting flush of all links");
                    self.unflushed_links = self
                        .links
                        .iter_mut()
                        .enumerate()
                        .filter_map(|(id, link_opt)| {
                            link_opt.as_mut().and_then(|link| {
                                if link.unconfirmed().is_none() {
                                    link.start_flush();
                                    Some(id)
                                } else {
                                    None
                                }
                            })
                        })
                        .collect();
                    self.idle_links.retain(|idle_id| !self.unflushed_links.contains(idle_id));
                    self.flushed_tx = Some(tx);
                }

                TaskEvent::ConfirmTimedOut(id) => {
                    let link = self.links[id].as_ref().unwrap();
                    tracing::debug!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "acknowledgement timeout on link with ping {} ms",
                        link.roundtrip.as_millis()
                    );
                    self.unconfirm_link(id, NotWorkingReason::AckTimeout);
                }

                TaskEvent::Resend(packet) => {
                    let id = resendable_idle_link_id.unwrap();
                    let link = self.links[id].as_ref().unwrap();
                    self.idle_links.retain(|&idle_id| idle_id != id);
                    tracing::trace!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "resending message {} over idle link", packet.seq
                    );
                    self.resend_reliable_over_link(id, packet);
                }

                TaskEvent::ReadDropped => {
                    tracing::debug!("receiver was dropped");
                    self.read_tx = None;
                    self.read_closed_rx = None;
                    if let Some(id) = self.idle_links.pop() {
                        let link = self.links[id].as_ref().unwrap();
                        tracing::debug!(
                            link_id =? link.link_id(), tag =% link.tag(),
                            "sending ReceiveFinish over idle link"
                        );
                        self.send_reliable_over_link(id, ReliableMsg::ReceiveFinish);
                        self.receive_finish_sent = true;
                    } else {
                        tracing::debug!("queueing sending of ReceiveFinish");
                    }
                }

                TaskEvent::ReadClosed => {
                    tracing::debug!("receiver was closed");
                    self.read_closed_rx = None;
                    if let Some(id) = self.idle_links.pop() {
                        self.send_reliable_over_link(id, ReliableMsg::ReceiveClose);
                        self.receive_close_sent = true;
                    }
                }

                TaskEvent::ConsumeReceived { received, permit } => {
                    tracing::trace!("consuming received data message {:?}", &received.msg);
                    match received.msg {
                        ReliableMsg::Data(data) => {
                            self.rxed_reliable_size -= data.len();
                            self.rxed_reliable_consumed_since_last_ack += data.len();
                            if let Some(permit) = permit {
                                permit.send(data);
                            }
                        }
                        ReliableMsg::SendFinish => {
                            self.read_error_tx.send_replace(None);
                            self.read_tx = None;
                            self.receive_finish_sent = true;
                            self.rxed_reliable_consumed_force_ack = true;
                        }
                        // Handled in handle_received_reliable_msg.
                        ReliableMsg::ReceiveClose | ReliableMsg::ReceiveFinish | ReliableMsg::Consumed(_) => {
                            unreachable!()
                        }
                    }
                }

                TaskEvent::PingLink(id) => {
                    let link = self.links[id].as_mut().unwrap();
                    tracing::trace!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "requesting ping of link"
                    );
                    link.send_ping = true;
                    self.flush_link(id);
                }

                TaskEvent::LinkPingTimeout(id) => {
                    let link = self.links[id].as_ref().unwrap();
                    tracing::warn!(
                        link_id =? link.link_id(), tag =% link.tag(),
                         "removing link due to ping timeout"
                    );
                    self.remove_link(id, DisconnectReason::PingTimeout);
                }

                TaskEvent::LinkUnconfirmedTimeout(id) => {
                    let link = self.links[id].as_ref().unwrap();
                    tracing::warn!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "removing link due to unconfirmed timeout"
                    );
                    self.remove_link(id, DisconnectReason::UnconfirmedTimeout);
                }

                TaskEvent::LinkSendTimeout(id) => {
                    let link = self.links[id].as_ref().unwrap();
                    tracing::warn!(link_id =? link.link_id(), tag =% link.tag(),
                        "removing link due to send timeout"
                    );
                    self.remove_link(id, DisconnectReason::SendTimeout);
                }

                TaskEvent::LinkTesting => (),

                TaskEvent::NoLinksTimeout => {
                    tracing::warn!("disconnecting because no links are available for too long");
                    result = Err(TaskError::NoLinksTimeout);
                    read_term = Some(RecvError::AllLinksFailed);
                    write_term = SendError::AllLinksFailed;
                    link_term = DisconnectReason::AllUnconfirmedTimeout;
                    break;
                }

                TaskEvent::PublishLinkStats => {
                    for link_opt in &mut self.links {
                        if let Some(link) = link_opt.as_mut() {
                            link.publish_stats();
                        }
                    }
                }

                TaskEvent::RefusedLinkTask => (),

                TaskEvent::FatalConnectError(FatalConnectError::ServerIdMismatch) => {
                    tracing::warn!("disconnecting because server id changed");
                    result = Err(TaskError::ServerIdMismatch);
                    read_term = Some(RecvError::ServerIdMismatch);
                    write_term = SendError::ServerIdMismatch;
                    link_term = DisconnectReason::ServerIdMismatch;
                    break;
                }

                TaskEvent::FatalConnectError(FatalConnectError::Closed) => {
                    tracing::warn!("disconnecting because server closed connection");
                    result = Err(TaskError::AbortedByServer);
                    read_term = Some(RecvError::AbortedByServer);
                    write_term = SendError::AbortedByServer;
                    link_term = DisconnectReason::AbortedByServer;
                    break;
                }
            }
        }

        // Terminate aggregated links channel.
        if *self.read_error_tx.borrow() == Some(RecvError::TaskTerminated) {
            self.read_error_tx.send_replace(read_term);
        }
        if *self.write_error_tx.borrow() == SendError::TaskTerminated {
            self.write_error_tx.send_replace(write_term);
        }
        self.read_tx = None;
        self.write_rx = None;

        // Exchange forceful terminatation over all links, if requested.
        if send_terminate != SendTerminate::None {
            let mut term_tasks = FuturesUnordered::new();
            for link in &mut self.links {
                let Some(link) = link.as_mut() else { continue };
                term_tasks.push(link.terminate_connection(send_terminate == SendTerminate::Initiate));
            }

            let res =
                timeout(self.cfg.termination_timeout, async move { while term_tasks.next().await.is_some() {} })
                    .await;
            if res.is_err() {
                tracing::warn!("forceful connection termination timed out");
            }
        }

        // Disconnect all links.
        for link in self.links.drain(..) {
            let Some(link) = link else { continue };
            link.notify_disconnected(link_term.clone());
        }

        // Publish task termination reason.
        let _ = self.result_tx.send_replace(result.clone());
        #[allow(unused_assignments)]
        {
            // For drop order.
            self.link_rx = None;
        }

        result
    }

    /// Adds a newly established link and returns its id.
    fn add_link(&mut self, mut link: LinkInt<TX, RX, TAG>) -> usize {
        link.report_ready();
        link.set_unconfirmed(Some((Instant::now(), NotWorkingReason::New)));

        for (id, link_opt) in self.links.iter_mut().enumerate() {
            if link_opt.is_none() {
                *link_opt = Some(link);
                self.publish_links();
                return id;
            }
        }

        self.links.push(Some(link));
        self.publish_links();

        self.links.len() - 1
    }

    /// Removes the link with the specified index.
    fn remove_link(&mut self, id: usize, reason: DisconnectReason) {
        let link = self.links[id].as_ref().unwrap();
        tracing::debug!(
            link_id =? link.link_id(), tag =% link.tag(), ?reason,
            "removing link"
        );

        // Queue unconfirmed packets for resending.
        self.unconfirm_link(id, NotWorkingReason::Disconnecting);

        // Send disconnect reason.
        let link = self.links[id].take().unwrap();
        link.notify_disconnected(reason);

        // Cleanup and publish links.
        while let Some(None) = self.links.last() {
            self.links.pop();
        }
        self.publish_links();
    }

    /// Publishes the currently connected links.
    fn publish_links(&self) {
        let links = self.links.iter().filter_map(|link_opt| link_opt.as_ref().map(Link::from)).collect();
        self.links_tx.send_replace(links);
    }

    /// Returns since when no link is working.
    fn links_not_working_since(&mut self) -> Option<Instant> {
        let links_working = self
            .links
            .iter()
            .any(|link_opt| link_opt.as_ref().map(|link| link.unconfirmed().is_none()).unwrap_or_default());

        match (links_working, &self.links_not_working_since) {
            (true, Some(_)) => self.links_not_working_since = None,
            (false, None) => self.links_not_working_since = Some(Instant::now()),
            _ => (),
        }

        self.links_not_working_since
    }

    /// Receive buffer size of the remote endpoint.
    fn remote_recv_buffer(&self) -> Option<usize> {
        self.remote_cfg.as_ref().map(|cfg| cfg.recv_buffer.get() as usize)
    }

    /// Space available in buffers necessary for sending data.
    fn tx_space(&self) -> usize {
        let tx_local_space = (self.cfg.send_buffer.get() as usize).saturating_sub(self.txed_unacked);
        let tx_remote_space = self.remote_recv_buffer().unwrap_or_default().saturating_sub(self.txed_unconsumed);
        tx_local_space.min(tx_remote_space)
    }

    /// Returns whether a sequence number is available for sending.
    fn tx_seq_avail(&self) -> bool {
        self.txed_packets.front().map(|p| self.tx_seq - p.seq <= Seq::USABLE_INTERVAL).unwrap_or(true)
    }

    /// Returns the limit ping and good ping as given by the link ping spread limits, if configured.
    fn ping_spread_limits(&self) -> Option<(Duration, Duration)> {
        let min_ping = self
            .links
            .iter()
            .filter_map(|link_opt| link_opt.as_ref())
            .filter(|link| {
                link.unconfirmed().is_none()
                    && !link.is_blocked()
                    && link.roundtrip_estimates.is_some_and(|n| n >= RELIABLE_ROUNDTRIP_ESTIMATES)
            })
            .map(|link| link.roundtrip)
            .min()?;

        let limit_ping = min_ping * self.cfg.link_max_ping_spread?.get();
        let good_ping = (min_ping + 3 * limit_ping) / 4;

        Some((limit_ping, good_ping))
    }

    /// Checks that links behave within limits and unconfirm links that do not.
    fn check_link_limits(&mut self) {
        let mut to_unconfirm = Vec::new();
        let confirmed_links: Vec<_> = self
            .links
            .iter()
            .enumerate()
            .filter_map(|(id, link)| link.as_ref().map(|link| (id, link)))
            .filter(|(_id, link)| link.unconfirmed().is_none())
            .collect();

        // Check for link ping exceeding configured limit.
        let all_links_slow = confirmed_links.iter().all(|(_id, link)| {
            link.is_blocked() || link.link_cfg().max_ping.is_none_or(|max_ping| link.roundtrip > max_ping)
        });
        if !all_links_slow {
            for (id, link) in &confirmed_links {
                if let Some(max_ping) = link.link_cfg().max_ping
                    && link.roundtrip > max_ping
                {
                    tracing::debug!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "unconfirming link due to slow ping of {} ms",
                        link.roundtrip.as_millis(),
                    );
                    to_unconfirm.push((*id, NotWorkingReason::MaxPingExceeded));
                }
            }
        }

        // Check for link ping exceeds ping spread limit.
        if let Some((limit_ping, _good_ping)) = self.ping_spread_limits() {
            for (id, link) in &confirmed_links {
                if link.roundtrip > 2 * limit_ping
                    && link.roundtrip_estimates.is_some_and(|n| n >= RELIABLE_ROUNDTRIP_ESTIMATES)
                {
                    tracing::debug!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "unconfirming link due to ping of {} ms twice above ping spread limit of {} ms",
                        link.roundtrip.as_millis(), limit_ping.as_millis(),
                    );
                    to_unconfirm.push((*id, NotWorkingReason::MaxPingExceeded));
                }
            }
        }

        for (id, reason) in to_unconfirm {
            self.unconfirm_link(id, reason);
        }
    }

    /// Adjusts the link transmission buffer limits to ensure that no link stalls the channel.
    fn adjust_link_tx_limits(&mut self) {
        let Some(remote_recv_buffer) = self.remote_recv_buffer() else { return };
        let coming_seq = match self.resend_queue.front() {
            Some(packet) => packet.seq,
            None => self.tx_seq,
        };

        // Check for unconsumable data approaching its limits.
        let unconsumable_limit = (self.cfg.send_buffer.get() as usize).min(remote_recv_buffer);
        let low_level = self.txed_unconsumable < unconsumable_limit / 4;
        let soft_overrun = self.txed_unconsumable > unconsumable_limit / 3;
        let hard_overrun = self.txed_unconsumable > unconsumable_limit * 3 / 4;

        // If too much data is unconsumable, decrease unacked data limit of guilty link,
        // which is most probably the link used to send the oldest still unconfirmed data.
        if (soft_overrun && self.tx_overrun == SendOverrun::Armed)
            || (hard_overrun && self.tx_overrun != SendOverrun::Hard)
        {
            if let Some(id) = self.txed_packets.iter().find_map(|p| {
                if let SentReliableStatus::Sent { link_id, .. } = &*p.status.borrow() {
                    Some(*link_id)
                } else {
                    None
                }
            }) {
                let link = self.links[id].as_mut().unwrap();

                // Decrease limit.
                let current = link.txed_unacked_data.min(link.txed_unacked_data_limit);
                if hard_overrun {
                    link.txed_unacked_data_limit = current / 2;
                    self.tx_overrun = SendOverrun::Hard;
                } else if soft_overrun {
                    link.txed_unacked_data_limit = current * 95 / 100;
                    self.tx_overrun = SendOverrun::Soft;
                }
                self.tx_overrun_since = Some(Instant::now());
                tracing::trace!(
                    link_id =? link.link_id(), tag =% link.tag(),
                    "decreasing unacked limit of link to {} bytes",
                    link.txed_unacked_data_limit
                );

                // Block link from increasing its send data limit.
                link.txed_unacked_data_limit_increased = Some(coming_seq);
                link.txed_unacked_data_limit_increased_consecutively = 0;
            }
        } else if self.tx_overrun != SendOverrun::Armed && !soft_overrun && !hard_overrun {
            tracing::trace!("re-arming send overrun handling");
            self.tx_overrun = SendOverrun::Armed;
            self.tx_overrun_since = None;
        }

        // Rearm send overrun handling if it is blocked for too long.
        if let Some(since) = self.tx_overrun_since
            && since.elapsed() >= Duration::from_secs(1)
        {
            tracing::trace!("re-arming send overrun handling due to timeout");
            self.tx_overrun = SendOverrun::Armed;
            self.tx_overrun_since = None
        }

        // Decrease data limits of links that approach maximum ping.
        let all_links_slow = self.links.iter().all(|link_opt| {
            link_opt.as_ref().is_none_or(|link| {
                link.unconfirmed().is_some()
                    || link.is_blocked()
                    || link.link_cfg().max_ping.is_none_or(|max_ping| link.roundtrip > max_ping / 2)
            })
        });
        if !all_links_slow {
            for link_opt in &mut self.links {
                if let Some(link) = link_opt
                    && let Some(max_ping) = link.link_cfg().max_ping
                    && link.unconfirmed().is_none()
                    && link.txed_unacked_data_limit_increased.is_none()
                    && link.roundtrip > max_ping * 3 / 4
                {
                    // Decrease limit.
                    let current = link.txed_unacked_data.min(link.txed_unacked_data_limit);
                    link.txed_unacked_data_limit = current * 95 / 100;
                    tracing::trace!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "decreasing unacked limit of link to {} bytes due to ping",
                        link.txed_unacked_data_limit
                    );

                    // Block link from increasing its send data limit.
                    link.txed_unacked_data_limit_increased = Some(coming_seq);
                    link.txed_unacked_data_limit_increased_consecutively = 0;
                }
            }
        }

        // Determine minimum ping and calculate allowable ping spread.
        let (limit_ping, good_ping) = self.ping_spread_limits().unzip();

        // Decrease limit of links with ping above allowable ping spread.
        if let Some(limit_ping) = limit_ping {
            for link_opt in &mut self.links {
                if let Some(link) = link_opt
                    && link.unconfirmed().is_none()
                    && !link.is_blocked()
                    && link.roundtrip > limit_ping
                    && link.roundtrip_estimates.is_some_and(|n| n >= RELIABLE_ROUNDTRIP_ESTIMATES)
                {
                    // Decrease limit.
                    let current = link.txed_unacked_data.min(link.txed_unacked_data_limit);
                    link.txed_unacked_data_limit = current * 95 / 100;
                    tracing::trace!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "decreasing unacked limit of link to {} bytes due to ping spread limit ({} ms, limit={} ms)",
                        link.txed_unacked_data_limit,
                        link.roundtrip.as_millis(),
                        limit_ping.as_millis(),
                    );

                    // Block link from increasing its send data limit.
                    link.txed_unacked_data_limit_increased = Some(coming_seq);
                    link.txed_unacked_data_limit_increased_consecutively = 0;
                    link.roundtrip_estimates = None;
                }
            }
        }

        // Count number of working links.
        let working_link_count = self
            .links
            .iter()
            .filter(|link_opt| {
                link_opt.as_ref().is_some_and(|link| link.unconfirmed().is_none() && !link.is_blocked())
            })
            .count();

        // Check if data is available for sending but no link is available.
        let send_data_avail = self.write_rx.as_mut().map(|rx| rx.try_peek().is_ok()).unwrap_or_default()
            || !self.resend_queue.is_empty();
        let sendable_link_avail = self.links.iter().any(|link_opt| {
            link_opt.as_ref().is_some_and(|link| {
                !link.tx_pending
                    && link.unconfirmed().is_none()
                    && !link.is_blocked()
                    && link.txed_unacked_data < link.txed_unacked_data_limit
            })
        });

        // Increase the unacked data limits of links that are currently blocked by it.
        if send_data_avail && !sendable_link_avail {
            for link_opt in &mut self.links {
                if let Some(link) = link_opt
                    && !link.tx_pending
                    && link.unconfirmed().is_none()
                    && !link.is_blocked()
                    && link.txed_unacked_data >= link.txed_unacked_data_limit
                    && link.txed_unacked_data_limit_increased.is_none()
                    && link.txed_unacked_data_limit < link.link_cfg().unacked_limit.get()
                    && link
                        .link_cfg()
                        .max_ping
                        .is_none_or(|max_ping| link.roundtrip <= max_ping / 2 || all_links_slow)
                    && good_ping.is_none_or(|good_ping| {
                        link.roundtrip <= good_ping
                            && link.roundtrip_estimates.is_some_and(|n| n >= RELIABLE_ROUNDTRIP_ESTIMATES)
                    })
                {
                    // Increase limit, faster if done many times consecutively.
                    link.txed_unacked_data_limit = if working_link_count == 1 {
                        link.txed_unacked_data_limit * 2
                    } else if link.txed_unacked_data_limit_increased_consecutively >= 100 {
                        link.txed_unacked_data_limit * 120 / 100
                    } else if link.txed_unacked_data_limit_increased_consecutively >= 50 {
                        link.txed_unacked_data_limit * 110 / 100
                    } else if link.txed_unacked_data_limit_increased_consecutively >= 25 {
                        link.txed_unacked_data_limit * 105 / 100
                    } else if link.txed_unacked_data_limit_increased_consecutively >= 10 {
                        link.txed_unacked_data_limit * 102 / 100
                    } else {
                        link.txed_unacked_data_limit * 101 / 100
                    }
                    .max(100);

                    tracing::trace!(
                        link_id =? link.link_id(), tag =% link.tag(),
                        "increasing unacked limit of link to {} bytes (done {} times without overrun)",
                        link.txed_unacked_data_limit,
                        link.txed_unacked_data_limit_increased_consecutively
                    );

                    // Block link from increasing limit again until newly sent data is received.
                    link.txed_unacked_data_limit_increased = Some(coming_seq);
                    link.txed_unacked_data_limit_increased_consecutively =
                        link.txed_unacked_data_limit_increased_consecutively.saturating_add(1);
                    link.roundtrip_estimates = None;
                }
            }
        }

        // Reset consecutive increase count.
        if !low_level {
            for link in self.links.iter_mut().flatten() {
                link.txed_unacked_data_limit_increased_consecutively = 0;
            }
        }
    }

    /// Computes the earliest link-specific timeout.
    fn earliest_link_specific_timeout<T, S>(
        &self, timeout_fn: T, since_fn: S,
    ) -> impl Future<Output = usize> + use<TX, RX, TAG, T, S>
    where
        T: Fn(&LinkCfg) -> Duration,
        S: Fn(&LinkInt<TX, RX, TAG>) -> Option<Instant>,
    {
        let earliest_timeout = self
            .links
            .iter()
            .enumerate()
            .filter_map(|(id, link_opt)| {
                if let Some(link) = &link_opt
                    && let Some(since) = since_fn(link)
                {
                    Some((id, since + timeout_fn(link.link_cfg())))
                } else {
                    None
                }
            })
            .min_by_key(|(_id, t)| *t);

        async move {
            match earliest_timeout {
                Some((link_id, timeout)) => {
                    sleep_until(timeout).await;
                    link_id
                }
                None => future::pending().await,
            }
        }
    }

    /// Time when the earliest sent packet times out confirmation.
    ///
    /// Returns link id and instant of timeout.
    fn earliest_confirm_timeout(&self) -> Option<(usize, Instant, bool)> {
        for p in &self.txed_packets {
            if let SentReliableStatus::Sent { link_id, sent, flushed, resent, .. } = &*p.status.borrow() {
                let link = self.links[*link_id].as_ref().unwrap();
                let definitely_sent = flushed.unwrap_or(*sent);
                let mut dur_factor = if *resent { 3 } else { 1 };
                if link.roundtrip_estimates.unwrap_or_default() < RELIABLE_ROUNDTRIP_ESTIMATES {
                    dur_factor *= 3;
                }
                let dur = (link.roundtrip * link.link_cfg().ack_timeout_roundtrip_factor.get() * dur_factor)
                    .clamp(link.link_cfg().ack_timeout_min, link.link_cfg().ack_timeout_max);
                return Some((*link_id, definitely_sent + dur, flushed.is_some()));
            }
        }

        None
    }

    /// Time when next link must be pinged.
    fn next_link_ping(&self) -> Option<(usize, Instant)> {
        self.links
            .iter()
            .enumerate()
            .filter_map(|(id, link_opt)| match &link_opt {
                Some(link)
                    if link.current_ping_sent.is_none() && !link.send_ping && link.unconfirmed().is_none() =>
                {
                    match link.link_cfg().ping {
                        LinkPing::Periodic(interval) => {
                            Some((id, link.last_ping.map(|last| last + interval).unwrap_or_else(Instant::now)))
                        }
                        LinkPing::WhenIdle(timeout) => {
                            let msg_timeout =
                                link.tx_last_msg.map(|last| last + timeout).unwrap_or_else(Instant::now);
                            let ping_timeout =
                                link.last_ping.map(|last| last + timeout).unwrap_or_else(Instant::now);
                            Some((id, msg_timeout.max(ping_timeout)))
                        }
                        LinkPing::WhenTimedOut => None,
                    }
                }
                _ => None,
            })
            .min_by_key(|(_id, next_ping)| *next_ping)
    }

    /// Sends a sequenced reliable message over the specified link.
    fn send_reliable_over_link(&mut self, id: usize, reliable_msg: ReliableMsg) -> Seq {
        let seq = self.next_tx_seq();
        let link = self.links[id].as_mut().unwrap();

        // Send message.
        tracing::trace!(
            link_id =? link.link_id(), tag =% link.tag(),
            "sending reliable message {seq} over link: {reliable_msg:?}"
        );
        let (msg, data) = reliable_msg.to_link_msg(seq);
        link.start_send_msg(msg, data);

        // Update statistics.
        if let ReliableMsg::Data(data) = &reliable_msg {
            self.txed_unacked += data.len();
            self.txed_unconsumed += data.len();
            link.txed_unacked_data += data.len();
        }

        // Store sent message until confirmation to be able to resend it should the link fail.
        let packet = SentReliable {
            seq,
            status: AtomicRefCell::new(SentReliableStatus::Sent {
                sent: Instant::now(),
                flushed: None,
                link_id: id,
                link: link.link_id(),
                msg: reliable_msg,
                resent: false,
            }),
        };
        let packet = Arc::new(packet);
        link.txed_packets.push_back(Arc::downgrade(&packet));
        self.txed_packets.push_back(packet);

        seq
    }

    /// Resends a packet over the specified link.
    fn resend_reliable_over_link(&mut self, id: usize, packet: Arc<SentReliable>) {
        let link = self.links[id].as_mut().unwrap();

        // Extract message and link used for sending.
        let mut status = packet.status.borrow_mut();
        let SentReliableStatus::ResendQueued { msg: reliable_msg, sent_link } = &*status else {
            unreachable!("message was not queued for resending")
        };
        assert_ne!(link.link_id(), *sent_link, "message must not be resent over original link");

        // Send data.
        tracing::trace!(
            link_id =? link.link_id(), tag =% link.tag(),
            "resending reliable message {} over link: {:?}",
            packet.seq, reliable_msg
        );
        let (msg, data) = reliable_msg.to_link_msg(packet.seq);
        link.start_send_msg(msg, data);

        // Update link statistics.
        if let ReliableMsg::Data(data) = reliable_msg {
            link.txed_unacked_data += data.len();
        }

        // Adjust last buffer increase sequence number if necessary.
        match &mut link.txed_unacked_data_limit_increased {
            Some(last_increased) if packet.seq < *last_increased => {
                *last_increased = packet.seq;
            }
            _ => (),
        }

        // Update packet.
        *status = SentReliableStatus::Sent {
            sent: Instant::now(),
            flushed: None,
            link_id: id,
            link: link.link_id(),
            msg: reliable_msg.clone(),
            resent: true,
        };

        link.txed_packets.push_back(Arc::downgrade(&packet));
    }

    /// Unconfirms a link.
    fn unconfirm_link(&mut self, id: usize, reason: NotWorkingReason) {
        // Flush link.
        self.flush_link(id);

        // Mark link as unconfirmed.
        let link = self.links[id].as_mut().unwrap();
        link.set_unconfirmed(Some((Instant::now(), reason)));
        self.idle_links.retain(|&idle_id| idle_id != id);
        self.unflushed_links.remove(&id);

        // Reset limits.
        link.reset();

        // Mark packets as being resent and put them into resend queue.
        for p in &mut self.txed_packets {
            let mut status = p.status.borrow_mut();
            match &*status {
                SentReliableStatus::Sent { link_id, link: sent_link, msg, .. } if *link_id == id => {
                    // Update link statistics.
                    if let ReliableMsg::Data(data) = &msg {
                        link.txed_unacked_data -= data.len();
                    }

                    *status = SentReliableStatus::ResendQueued { msg: msg.clone(), sent_link: *sent_link };
                    self.resend_queue.push_back(p.clone());
                }
                _ => (),
            };
        }
        link.clean_txed_packets();

        // Sort resend queue, so that oldest packets are resend first.
        self.resend_queue.make_contiguous().sort_by_key(|packet| packet.seq);

        // Re-test other links that have failed testing.
        for link in self.links.iter_mut().flatten() {
            if let LinkTest::Failed(_) = link.test {
                link.test = LinkTest::Inactive;
            }
        }
    }

    /// Considers activating a link that has been disabled due to confirmation timeout.
    ///
    /// Returns time when next testing step is due.
    fn link_testing_step(&mut self, id: usize) -> Option<Instant> {
        let (limit_ping, _good_ping) = self.ping_spread_limits().unzip();
        let others_slow = self.links.iter().enumerate().all(|(link_id, link_opt)| {
            link_opt.as_ref().is_none_or(|link| {
                link_id == id
                    || link.unconfirmed().is_some()
                    || link.is_blocked()
                    || link.link_cfg().max_ping.is_none_or(|max_ping| link.roundtrip > max_ping)
            })
        });

        let link = self.links[id].as_mut()?;
        let link_id = link.link_id();

        match link.test {
            LinkTest::Failed(when) if when.elapsed() >= link.link_cfg().retest_interval => {
                tracing::trace!(?link_id, tag =% link.tag(), "link {id} is ready for retry of test");
                link.test = LinkTest::Inactive;
            }
            _ => (),
        }

        match link.test {
            LinkTest::Inactive => {
                if let &Some((mut since, ref reason)) = link.unconfirmed()
                    && link.tx_polling().is_none()
                    && link.current_ping_sent.is_none()
                    && !link.has_outstanding_ack()
                {
                    if *reason != NotWorkingReason::AckTimeout || link.link_cfg().test_after_ack_timeout {
                        if link.is_blocked() {
                            // We do not test links that are blocked; however, to prevent them from
                            // being disconnected due to the non-working timeout we regularly update
                            // the unconfirmed timestamp.
                            if since.elapsed() >= link.link_cfg().non_working_timeout / 2 {
                                tracing::trace!(
                                    ?link_id, tag =% link.tag(),
                                    "postponing test of link that is currently blocked"
                                );
                                since = Instant::now();
                                link.set_unconfirmed(Some((since, reason.clone())));
                            }
                            return Some(since + link.link_cfg().non_working_timeout / 2);
                        }

                        let test_data_limit = if link.link_cfg().max_ping.is_some() {
                            link.link_cfg().unacked_init.get()
                        } else {
                            link.link_cfg().unacked_limit.get().min(self.cfg.send_buffer.get() as usize)
                        }
                        .min(link.link_cfg().test_data_limit);
                        let test_data = link.send_test_data(self.cfg.io_write_size.get(), test_data_limit);
                        link.send_ping = true;
                        link.test = LinkTest::InProgress;
                        tracing::debug!(
                            ?link_id, tag =% link.tag(),
                            "started test of link using {test_data} bytes of test data"
                        );
                    } else {
                        tracing::debug!(
                            ?link_id, tag =% link.tag(),
                            "link recovered after {} ms with ping {} ms",
                            since.elapsed().as_millis(), link.roundtrip.as_millis()
                        );
                        link.set_unconfirmed(None);

                        self.idle_links.retain(|&idle_id| idle_id != id);
                        link.report_ready();
                    }
                }

                None
            }

            LinkTest::InProgress => {
                if link.current_ping_sent.is_none() && !link.send_ping {
                    // Ping has completed.
                    let mut limits = vec![link.link_cfg().ack_timeout_max / 2];
                    if let Some(link_max_ping) = link.link_cfg().max_ping
                        && !others_slow
                    {
                        limits.push(link_max_ping);
                    }
                    if let Some(limit_ping) = limit_ping {
                        limits.push(limit_ping);
                    }
                    let roundtrip_limit = limits.into_iter().min().unwrap();

                    if link.roundtrip <= roundtrip_limit {
                        // Ping response arrived quickly enough, thus mark link as confirmed.
                        tracing::debug!(
                            ?link_id, tag =% link.tag(),
                            "link successfully completed test with ping {} ms",
                            link.roundtrip.as_millis()
                        );
                        link.set_unconfirmed(None);
                        link.test = LinkTest::Inactive;

                        self.idle_links.retain(|&idle_id| idle_id != id);
                        link.report_ready();

                        None
                    } else {
                        // Link is too slow, schedule retest.
                        tracing::debug!(
                            ?link_id, tag =% link.tag(),
                            "link failed test with ping {} ms (limit={} ms)",
                            link.roundtrip.as_millis(), roundtrip_limit.as_millis(),
                        );
                        let when = Instant::now();
                        link.test = LinkTest::Failed(when);
                        let since = match link.unconfirmed() {
                            Some((since, _reason)) => *since,
                            None => when,
                        };
                        link.set_unconfirmed(Some((since, NotWorkingReason::MaxPingExceeded)));
                        Some(when + link.link_cfg().retest_interval)
                    }
                } else {
                    None
                }
            }

            LinkTest::Failed(when) => Some(when + link.link_cfg().retest_interval),
        }
    }

    // Next reliable transmission sequence number.
    fn next_tx_seq(&mut self) -> Seq {
        let seq = self.tx_seq;
        self.tx_seq += 1;
        seq
    }

    /// Starts flushing the specified link.
    fn flush_link(&mut self, id: usize) {
        let link = self.links[id].as_mut().unwrap();
        link.start_flush();
        self.idle_links.retain(|&idle_id| idle_id != id);
    }

    /// Handle a received message.
    ///
    /// Returns whether task should terminate.
    fn handle_received_msg(&mut self, id: usize, msg: LinkMsg, data: Option<Bytes>) -> Result<bool, io::Error> {
        let link = self.links[id].as_mut().unwrap();
        let link_id = link.link_id();
        let tag = link.tag();

        match msg {
            LinkMsg::Ping => {
                // Respond with pong on same link.
                tracing::trace!(?link_id, tag =% link.tag(), "ping received, requesting sending resposne");
                link.send_pong = true;
                self.flush_link(id);
            }
            LinkMsg::Pong => {
                if let Some(current_ping_sent) = link.current_ping_sent.take() {
                    let elapsed = current_ping_sent.elapsed();
                    tracing::trace!(?link_id, tag =% link.tag(), "ping round-trip time is {} ms", elapsed.as_millis());
                    link.roundtrip = elapsed;
                    link.roundtrip_estimates = Some(1);
                    link.last_ping = Some(Instant::now());
                    self.link_testing_step(id);
                }
            }
            msg @ (LinkMsg::Data { .. }
            | LinkMsg::Consumed { .. }
            | LinkMsg::SendFinish { .. }
            | LinkMsg::ReceiveClose { .. }
            | LinkMsg::ReceiveFinish { .. }) => {
                let (reliable_msg, seq) = ReliableMsg::from_link_msg(msg, data);
                tracing::trace!(?link_id, %tag, "received reliable message {seq}: {reliable_msg:?}");
                self.handle_received_reliable_msg(id, seq, reliable_msg)?;
            }
            LinkMsg::Ack { received } => {
                tracing::trace!(?link_id, %tag, "link acked reception up to {received}");
                self.handle_ack(id, received);
            }
            LinkMsg::TestData { size } => {
                tracing::trace!(?link_id, %tag, "link received {size} bytes of test data");
            }
            LinkMsg::SetBlock { blocked } => {
                tracing::debug!(?link_id, %tag, %blocked, "remote block status of link changed");
                link.remotely_blocked.store(blocked, Ordering::Relaxed);
                self.idle_links.retain(|&idle_id| idle_id != id);
                link.report_ready();
                link.blocked_changed_out_tx.send_replace(());
            }
            LinkMsg::Goodbye => {
                match link.disconnecting {
                    Some(DisconnectInitiator::Local) => {
                        if link.goodbye_sent {
                            // Remote endpoint has received all our previous message, our goodbye and
                            // finished sending all outstanding messages.
                            tracing::info!(?link_id, %tag, "removing link due to local request");
                            self.remove_link(id, DisconnectReason::LocallyRequested);
                        }
                    }
                    Some(DisconnectInitiator::Remote) => {
                        return Err(protocol_err!("received Goodbye message more than once"));
                    }
                    None => {
                        // Remote endpoint is initiating disconnection.
                        tracing::debug!(?link_id, %tag, "remote requests disconnection of link");
                        link.disconnecting = Some(DisconnectInitiator::Remote);
                        self.unconfirm_link(id, NotWorkingReason::Disconnecting);
                    }
                }
            }
            LinkMsg::Terminate => {
                tracing::trace!(?link_id, %tag, "link recevied forceful connection termination request");
                return Ok(true);
            }
            LinkMsg::Welcome { .. } | LinkMsg::Connect { .. } | LinkMsg::Accepted | LinkMsg::Refused { .. } => {
                return Err(protocol_err!("received unexpected message"));
            }
        }

        Ok(false)
    }

    /// Handle received data.
    fn handle_received_reliable_msg(&mut self, id: usize, seq: Seq, msg: ReliableMsg) -> Result<(), io::Error> {
        let link = self.links[id].as_mut().unwrap();

        // Update link and queue sending of ack.
        link.tx_ack_queue.push_back(seq);
        self.idle_links.retain(|&idle_id| idle_id != id);
        link.report_ready();

        let link_id = link.link_id();
        let tag = link.tag();

        if seq < self.rx_seq {
            // The sequence number belongs to a packet that has already been
            // received and consumed. Thus the acknowledgement has been
            // lost and must be resend.
            tracing::trace!(?link_id, %tag, "re-received consumed reliable message {}", seq);
        } else {
            let offset = (seq - self.rx_seq) as usize;
            if offset > Seq::USABLE_INTERVAL as usize {
                return Err(protocol_err!("sequence number underflow"));
            }

            if self.rxed_reliable.len() <= offset {
                self.rxed_reliable.resize(offset + 1, None);
            }

            if self.rxed_reliable[offset].is_none() {
                tracing::trace!(?link_id, %tag, "received reliable message {}", seq);

                match &msg {
                    ReliableMsg::Data(data) => {
                        self.rxed_reliable_size += data.len();
                        if self.rxed_reliable_size > self.cfg.recv_buffer.get() as usize {
                            return Err(protocol_err!("receive buffer overflow"));
                        }
                    }
                    ReliableMsg::SendFinish => {
                        // Handled during consumption.
                    }
                    ReliableMsg::Consumed(consumed) => {
                        tracing::trace!(?link_id, %tag, "remote consumed {consumed} bytes");
                        match self.txed_unconsumed.checked_sub(*consumed as usize) {
                            Some(txed_unconsumed) => self.txed_unconsumed = txed_unconsumed,
                            None => return Err(protocol_err!("txed_unconsumed underflow")),
                        }
                    }
                    ReliableMsg::ReceiveClose => {
                        self.write_error_tx.send_replace(SendError::Closed);
                        self.write_closed.store(true, Ordering::Relaxed);
                        self.rxed_reliable_consumed_force_ack = true;
                    }
                    ReliableMsg::ReceiveFinish => {
                        self.write_error_tx.send_replace(SendError::Dropped);
                        self.write_rx = None;
                        self.send_finish_sent = true;
                        self.rxed_reliable_consumed_force_ack = true;
                    }
                }

                self.rxed_reliable[offset] = Some(ReceivedReliableMsg { seq, msg });
            } else {
                // The sequence number belongs to a packet that has alredy been
                // received. Thus the acknowledgement has been lost and must be resend.
                tracing::trace!(?link_id, %tag, "re-received unconsumed reliable message {}", seq);
            }
        }

        // Forward received messages that are ready for consumption.
        while let Some(Some(_)) = self.rxed_reliable.front().as_ref() {
            let msg = self.rxed_reliable.pop_front().unwrap().unwrap();

            assert_eq!(msg.seq, self.rx_seq);
            self.rx_seq += 1;

            if matches!(&msg.msg, ReliableMsg::Data(_) | ReliableMsg::SendFinish) {
                self.rxed_reliable_consumable.push_back(msg);
            }
        }

        Ok(())
    }

    /// Returns whether sending a Consumed message is required.
    fn is_consume_ack_required(&self) -> bool {
        self.rxed_reliable_consumed_since_last_ack > self.cfg.recv_buffer.get() as usize / 10
            || self.rxed_reliable_consumed_force_ack
    }

    /// Handles a received acknowledgement.
    fn handle_ack(&mut self, id: usize, rxed_seq: Seq) {
        let link = self.links[id].as_mut().unwrap();
        let link_id = link.link_id();
        let tag = link.tag();

        tracing::trace!(?link_id, %tag, "processing received ack for {rxed_seq} on link");

        // Possibly unblock send buffer increase.
        if let Some(last_increased) = link.txed_unacked_data_limit_increased {
            if last_increased <= rxed_seq {
                tracing::trace!(?link_id, %tag, "re-allowing increase of send limit of link");
                link.txed_unacked_data_limit_increased = None;
            } else {
                link.roundtrip_estimates = Some(0);
            }
        }

        // Remove packet that has been received by remote endpoint.
        let back_idx = self.tx_seq - rxed_seq;
        if 0 < back_idx && (back_idx as usize) <= self.txed_packets.len() {
            let idx = self.txed_packets.len() - back_idx as usize;
            let packet = &mut self.txed_packets[idx];
            assert_eq!(packet.seq, rxed_seq);

            let mut status = packet.status.borrow_mut();
            match &*status {
                SentReliableStatus::Sent { sent, link_id, msg, .. } if *link_id == id => {
                    let size = if let ReliableMsg::Data(data) = &msg { data.len() } else { 0 };

                    link.txed_unacked_data -= size;
                    self.txed_unacked -= size;
                    self.txed_unconsumable += size;

                    if link.roundtrip_estimates == Some(0) {
                        link.roundtrip = sent.elapsed();
                    } else if sent.elapsed() > link.roundtrip {
                        link.roundtrip = (link.roundtrip + 3 * sent.elapsed()) / 4;
                    } else {
                        link.roundtrip = (99 * link.roundtrip + sent.elapsed()) / 100;
                    }

                    if let Some(n) = &mut link.roundtrip_estimates {
                        *n = n.saturating_add(1);
                    }

                    *status = SentReliableStatus::Received { size };
                }
                SentReliableStatus::ResendQueued { msg, .. } => {
                    let size = if let ReliableMsg::Data(data) = &msg { data.len() } else { 0 };

                    self.txed_unacked -= size;
                    self.txed_unconsumable += size;
                    self.resend_queue.retain(|packet| packet.seq != rxed_seq);

                    *status = SentReliableStatus::Received { size };
                }
                _ => (),
            }
        }

        // Swipe front of unconfirmed queue.
        while let Some(packet) = self.txed_packets.front() {
            self.txed_last_consumed = packet.seq;

            if let SentReliableStatus::Received { size, .. } = &*packet.status.borrow() {
                self.txed_unconsumable -= size;
            } else {
                break;
            }

            self.txed_packets.pop_front();
        }

        link.clean_txed_packets();
    }

    /// Sends statistics data.
    fn send_stats(&mut self) {
        let Some(interval) = self.cfg.stats_intervals.iter().min() else { return };
        if self.stats_last_sent.elapsed() >= *interval {
            self.stats_last_sent = Instant::now();

            self.stats_tx.send_replace(Stats {
                established: self.established,
                not_working_since: self.links_not_working_since,
                send_space: self.tx_space(),
                sent_unacked: self.txed_unacked,
                sent_unconsumed: self.txed_unconsumed,
                sent_unconsumed_count: self.txed_packets.len(),
                sent_unconsumable: self.txed_unconsumable,
                resend_queue_len: self.resend_queue.len(),
                recved_unconsumed: self.rxed_reliable_size,
                recved_unconsumed_count: self.rxed_reliable.len(),
            });
        }
    }

    /// The connection identifier.
    pub fn id(&self) -> ConnId {
        self.conn_id.get()
    }

    /// The direction of the connection.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// Sets the link filter function.
    ///
    /// The link filter function is called for each new link and can inspect
    /// the new link (provided as the first argument) as well as the existing
    /// links of the connection (provided as the second argument).
    ///
    /// It should return whether the link should be accepted.
    ///
    /// While the link filter function is being executed, the connection is
    /// blocked. It should thus execute quickly.
    pub fn set_link_filter<F, Fut>(&mut self, mut link_filter: F)
    where
        F: FnMut(Link<TAG>, Vec<Link<TAG>>) -> Fut + Send + 'static,
        Fut: Future<Output = bool> + Send + 'static,
    {
        self.link_filter = Box::new(move |link, others| link_filter(link, others).boxed());
    }

    /// Enables dumping of analysis data over the provided channel while the aggregator task is running.
    ///
    /// The purpose of the dumped data is to debug connection performance issues
    /// and to help with the development of Aggligator.
    /// Normally there is no need to enable it and it may cause a significant performance overhead.
    ///
    /// Sending over the channel is performed without blocking,
    /// i.e. if no sufficient send space is available the dump data is discarded.
    #[cfg(feature = "dump")]
    #[cfg_attr(docsrs, doc(cfg(feature = "dump")))]
    pub fn dump(&mut self, tx: mpsc::Sender<super::dump::ConnDump>) {
        self.dump_tx = Some(tx);
    }

    /// Sends dump data.
    #[cfg(feature = "dump")]
    fn send_dump(&mut self) {
        if let Some(tx) = &self.dump_tx {
            let mut closed = false;

            match tx.try_reserve() {
                Ok(permit) => permit.send(super::dump::ConnDump::from(&*self)),
                Err(mpsc::error::TrySendError::Full(_)) => (),
                Err(mpsc::error::TrySendError::Closed(_)) => closed = true,
            }

            if closed {
                self.dump_tx = None;
            }
        }

        if self.read_tx.is_none() || self.write_rx.is_none() {
            tracing::trace!(
                direction = ?self.direction,
                read_tx_none = self.read_tx.is_none(),
                write_rx_none = self.write_rx.is_none(),
                txed_packets = self.txed_packets.len(),
                txed_packets_front = ?self.txed_packets.front(),
                resend_queue = self.resend_queue.len(),
                resend_queue_front = ?self.resend_queue.front(),
                txed_unconsumed = self.txed_unconsumed,
                rxed_reliable = self.rxed_reliable.len(),
                rxed_reliable_front = ?self.rxed_reliable.front(),
                rxed_reliable_size = self.rxed_reliable_size,
                rxed_reliable_consumed_since_last_ack = self.rxed_reliable_consumed_since_last_ack,
                self.send_finish_sent,
                self.receive_finish_sent,
                "connection state while a channel side is closed"
            );
        }
    }
}

impl<TX, RX, TAG> IntoFuture for Task<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + Sync + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + Sync + 'static,
    TAG: fmt::Display + Send + Sync + 'static,
{
    type Output = Result<(), TaskError>;

    type IntoFuture = BoxFuture<'static, Result<(), TaskError>>;

    fn into_future(self) -> Self::IntoFuture {
        self.run().boxed()
    }
}

#[cfg(feature = "dump")]
impl<TX, RX, TAG> From<&Task<TX, RX, TAG>> for super::dump::ConnDump {
    fn from(task: &Task<TX, RX, TAG>) -> Self {
        use super::dump::LinkDump;

        let mut links: Vec<_> = task.links.iter().map(|opt| opt.as_ref().map(LinkDump::from)).collect();

        Self {
            conn_id: task.conn_id.get().0,
            runtime: task.start_time.elapsed().as_secs_f32(),
            txed_unacked: task.txed_unacked,
            txed_unconsumable: task.txed_unconsumable,
            txed_unconsumed: task.txed_unconsumed,
            send_buffer: task.cfg.send_buffer.get(),
            remote_receive_buffer: task.remote_cfg.as_ref().map(|cfg| cfg.recv_buffer.get()).unwrap_or_default(),
            resend_queue: task.resend_queue.len(),
            rxed_reliable_size: task.rxed_reliable_size,
            rxed_reliable_consumed_since_last_ack: task.rxed_reliable_consumed_since_last_ack,
            link0: links.get_mut(0).and_then(Option::take).unwrap_or_default(),
            link1: links.get_mut(1).and_then(Option::take).unwrap_or_default(),
            link2: links.get_mut(2).and_then(Option::take).unwrap_or_default(),
            link3: links.get_mut(3).and_then(Option::take).unwrap_or_default(),
            link4: links.get_mut(4).and_then(Option::take).unwrap_or_default(),
            link5: links.get_mut(5).and_then(Option::take).unwrap_or_default(),
            link6: links.get_mut(6).and_then(Option::take).unwrap_or_default(),
            link7: links.get_mut(7).and_then(Option::take).unwrap_or_default(),
            link8: links.get_mut(8).and_then(Option::take).unwrap_or_default(),
            link9: links.get_mut(9).and_then(Option::take).unwrap_or_default(),
        }
    }
}
