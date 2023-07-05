//! Link aggregator task.

use atomic_refcell::AtomicRefCell;
use bytes::Bytes;
use futures::{
    future, future::BoxFuture, stream, stream::FuturesUnordered, Future, FutureExt, Sink, Stream, StreamExt,
};
use rand::prelude::*;
use rand_xoshiro::SplitMix64;
use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    fmt,
    future::IntoFuture,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    select,
    sync::{mpsc, oneshot, watch},
    time::{interval, sleep_until, timeout, Instant},
};
use tokio_stream::wrappers::IntervalStream;

use crate::{
    agg::link_int::{DisconnectInitiator, LinkInt, LinkIntEvent, LinkTest},
    alc::{RecvError, SendError},
    cfg::{Cfg, ExchangedCfg, LinkPing},
    control::{Direction, DisconnectReason, Link, NotWorkingReason, Stats},
    id::{ConnId, LinkId, OwnedConnId},
    msg::{LinkMsg, RefusedReason, ReliableMsg},
    peekable_mpsc::{PeekableReceiver, RecvIfError},
    protocol_err,
    seq::Seq,
};

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
    /// The task was terminated, possibly due to the runtime shutting down.
    Terminated,
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::AllUnconfirmedTimeout => write!(f, "all links unconfirmed timeout"),
            Self::NoLinksTimeout => write!(f, "no links available timeout"),
            Self::ProtocolError { link_id, error } => write!(f, "protocol error on link {link_id}: {error}"),
            Self::ServerIdMismatch => write!(f, "a new link connected to another server"),
            Self::Terminated => write!(f, "task terminated"),
        }
    }
}

impl Error for TaskError {}

impl From<TaskError> for std::io::Error {
    fn from(err: TaskError) -> Self {
        io::Error::new(io::ErrorKind::ConnectionReset, err)
    }
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
struct SentReliable {
    /// Sequence number.
    seq: Seq,
    /// Status.
    status: AtomicRefCell<SentReliableStatus>,
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
enum SentReliableStatus {
    /// Message was sent, but its reception is not yet confirmed.
    Sent {
        /// Time packet was sent.
        sent: Instant,
        /// Index of link used to send the packet.
        link_id: usize,
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
    /// A new link has been established.
    NewLink(LinkInt<TX, RX, TAG>),
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
    /// The server id changed.
    ServerChanged,
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
    /// Server changed notification.
    server_changed_rx: mpsc::Receiver<()>,
    /// Result of task sender.
    result_tx: watch::Sender<Result<(), TaskError>>,
    /// Channel for sending analysis data.
    #[cfg(feature = "dump")]
    dump_tx: Option<mpsc::Sender<super::dump::ConnDump>>,
}

impl<TX, RX, TAG> fmt::Debug for Task<TX, RX, TAG> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Task").field("id", &self.conn_id).field("direction", &self.direction).finish()
    }
}

impl<TX, RX, TAG> Task<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    TAG: Send + Sync + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cfg: Arc<Cfg>, remote_cfg: Option<Arc<ExchangedCfg>>, conn_id: OwnedConnId, direction: Direction,
        links_tx: watch::Sender<Vec<Link<TAG>>>, link_rx: mpsc::Receiver<LinkInt<TX, RX, TAG>>,
        connected_tx: oneshot::Sender<Arc<ExchangedCfg>>, read_tx: mpsc::Sender<Bytes>,
        read_closed_rx: mpsc::Receiver<()>, write_rx: mpsc::Receiver<SendReq>,
        read_error_tx: watch::Sender<Option<RecvError>>, write_error_tx: watch::Sender<SendError>,
        stats_tx: watch::Sender<Stats>, server_changed_rx: mpsc::Receiver<()>,
        result_tx: watch::Sender<Result<(), TaskError>>, links: Vec<LinkInt<TX, RX, TAG>>,
    ) -> Self {
        Self {
            cfg,
            remote_cfg,
            conn_id,
            direction,
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
            server_changed_rx,
            result_tx,
            #[cfg(feature = "dump")]
            dump_tx: None,
        }
    }

    /// Runs the task that manages the connection of aggregated links.
    ///
    /// This returns when the connection has been terminated.
    /// Cancelling the returned future leads to immediate termination of the connection.
    #[tracing::instrument(level = "debug")]
    pub async fn run(mut self) -> Result<(), TaskError> {
        tracing::debug!("link aggregator task starting");
        self.start_time = Instant::now();

        let mut stat_timers =
            stream::select_all(self.cfg.stats_intervals.iter().map(|t| IntervalStream::new(interval(*t))));

        let mut fast_rng = SplitMix64::seed_from_u64(1);

        // Termination reasons when exiting main loop.
        let read_term;
        let write_term;
        let link_term;
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
            if links_available {
                if let Some(connected_tx) = self.connected_tx.take() {
                    tracing::debug!("sending connection established notification");
                    let _ = connected_tx.send(self.remote_cfg.clone().unwrap());
                    self.established = Some(Instant::now());
                }
            }

            // Notify that flushing has completed.
            if self.unflushed_links.is_empty() {
                if let Some(tx) = self.flushed_tx.take() {
                    tracing::trace!("flush request completed");
                    let _ = tx.send(());
                }
            }

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
            let next_pong_timeout =
                self.earliest_link_specific_timeout(self.cfg.link_ping_timeout, |link| link.current_ping_sent);

            // Timeout for removing an unconfirmed link.
            let next_unconfirmed_timeout = self
                .earliest_link_specific_timeout(self.cfg.link_non_working_timeout, |link| {
                    link.unconfirmed.as_ref().map(|(since, _)| *since)
                });

            // Timeout for removing a link that takes too long to send data.
            let next_send_timeout =
                self.earliest_link_specific_timeout(self.cfg.link_ping_timeout, |link| link.tx_polling());

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
                    Some((link_id, timeout)) => {
                        sleep_until(timeout).await;
                        link_id
                    }
                    None => future::pending().await,
                }
            };

            // Task for receiving a new link.
            let new_link_task = async {
                match &mut self.link_rx {
                    _ if !self.init_links.is_empty() => TaskEvent::NewLink(self.init_links.pop_front().unwrap()),
                    Some(link_rx) => match link_rx.recv().await {
                        Some(link) => TaskEvent::NewLink(link),
                        None => TaskEvent::NoNewLinks,
                    },
                    None => future::pending().await,
                }
            };

            // Task for receiving requests from sender.
            let sendable_idle_link_id =
                self.idle_links.iter().rev().cloned().find(|id| self.links[*id].as_ref().unwrap().is_sendable());
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
                            link_opt.as_mut().map(|link| async move { (id, link.event(id).await) }.boxed())
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
                if resending && sendable_idle_link_id.is_some() {
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
                new_link_event = new_link_task => new_link_event,
                ((id, event), _, _) = link_task => TaskEvent::LinkEvent { id, event },
                write_event = write_rx_task => write_event,
                link_id = recv_confirm_timeout => TaskEvent::ConfirmTimedOut(link_id),
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
                Some(()) = self.server_changed_rx.recv() => TaskEvent::ServerChanged,
            };

            // Handle event.
            match event {
                TaskEvent::NewLink(mut link) => {
                    if self.remote_cfg.is_none() {
                        let remote_cfg = link.remote_cfg();
                        tracing::debug!("obtained remote configuration: {remote_cfg:?}");
                        self.remote_cfg = Some(remote_cfg);
                    }
                    let others =
                        self.links.iter().filter_map(|link_opt| link_opt.as_ref().map(Link::from)).collect();
                    if (self.link_filter)(Link::from(&link), others).await {
                        let id = self.add_link(link);
                        tracing::info!("added new link with id {id}");
                    } else {
                        tracing::debug!("link was refused by link filter");
                        let link_non_working_timeout = self.cfg.link_non_working_timeout;
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
                    match event {
                        LinkIntEvent::TxReady => {
                            // Link is ready to send more data.
                            let link = self.links[id].as_mut().unwrap();
                            let link_blocked = link.blocked.load(Ordering::SeqCst);
                            if link.needs_tx_accepted {
                                tracing::debug!("sending Accepted over link {id}");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Accepted, None);
                                link.needs_tx_accepted = false;
                            } else if link.send_pong {
                                tracing::trace!("sending Pong over link {id}");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Pong, None);
                                link.send_pong = false;
                            } else if let Some(initiator) = link.disconnecting {
                                if !link.goodbye_sent {
                                    tracing::debug!("sending GoodBye over link {id}");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    link.start_send_msg(LinkMsg::Goodbye, None);
                                    link.goodbye_sent = true;
                                } else if initiator == DisconnectInitiator::Remote {
                                    // All outstanding messages and Goodbye have been sent and flushed,
                                    // thus we can now disconnect the link.
                                    tracing::info!("removing link {id} by remote request");
                                    self.remove_link(id, DisconnectReason::RemotelyRequested);
                                }
                            } else if link.send_ping {
                                tracing::trace!("sending Ping over link {id}");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Ping, None);
                                link.current_ping_sent = Some(Instant::now());
                                link.send_ping = false;
                            } else if link_blocked != link.blocked_sent {
                                tracing::debug!("local block status of link {id} has become {link_blocked}");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::SetBlock { blocked: link_blocked }, None);
                                link.blocked_sent = link_blocked;
                            } else if let Some(recved_seq) = link.tx_ack_queue.pop_front() {
                                tracing::trace!("acking sequence {recved_seq} over non-idle link {id}");
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_send_msg(LinkMsg::Ack { received: recved_seq }, None);
                            } else if link.unconfirmed.is_none() && !link.is_blocked() {
                                // This is a link that is believed to be working, so we can submit
                                // reliable messages over it. Do so by priority.
                                if is_consume_ack_required {
                                    let consumed = self.rxed_reliable_consumed_since_last_ack as u32;
                                    tracing::trace!("acking {consumed} consumed bytes over non-idle link {id}");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::Consumed(consumed));
                                    self.rxed_reliable_consumed_since_last_ack = 0;
                                    self.rxed_reliable_consumed_force_ack = false;
                                } else if resending && link.is_sendable() {
                                    let packet = self.resend_queue.pop_front().unwrap();
                                    tracing::trace!("resending packet {} over non-idle link {id}", packet.seq);
                                    self.idle_links.retain(|idle_id| *idle_id != id);
                                    self.resend_reliable_over_link(id, packet);
                                } else if self.read_closed_rx.is_none() && !self.receive_close_sent {
                                    tracing::trace!("sending ReceiveClose over non-idle link {id}");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::ReceiveClose);
                                    self.receive_close_sent = true;
                                } else if self.read_tx.is_none() && !self.receive_finish_sent {
                                    tracing::trace!("sending ReceiveFinish over non-idle link {id}");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::ReceiveFinish);
                                    self.receive_finish_sent = true;
                                } else if self.write_rx.is_none() && !self.send_finish_sent {
                                    tracing::trace!("sending SendFinish over non-idle link {id}");
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
                                        "sending data of size {} over non-idle link {id}",
                                        data.len()
                                    );
                                    self.idle_links.retain(|idle_id| *idle_id != id);
                                    self.send_reliable_over_link(id, ReliableMsg::Data(data));
                                } else if link.need_ack_flush() {
                                    tracing::trace!("flushing link {id} due to sent acks");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    link.start_flush();
                                } else if link.needs_flush() && !link.is_sendable() {
                                    tracing::trace!("flushing link {id} because it is not sendable");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    link.start_flush();
                                } else if !self.idle_links.contains(&id) {
                                    // Store link in idle list.
                                    tracing::trace!("link {id} has become idle");
                                    link.mark_idle();
                                    self.idle_links.push(id);
                                }
                            } else {
                                // Link is unconfirmed, make sure it is flushed.
                                if link.needs_flush() || link.need_ack_flush() {
                                    tracing::trace!("flushing link {id} because it is not unconfirmed");
                                    self.idle_links.retain(|&idle_id| idle_id != id);
                                    link.start_flush();
                                }
                            }
                        }
                        LinkIntEvent::TxFlushed => {
                            // Link has completed flushing.
                            self.unflushed_links.remove(&id);
                        }
                        LinkIntEvent::Rx { msg, data } => {
                            // Link has received a message.
                            if let Err(err) = self.handle_received_msg(id, msg, data) {
                                tracing::warn!("link {id} caused protocol error: {err}");
                                result = Err(TaskError::ProtocolError {
                                    link_id: self.links[id].as_ref().unwrap().link_id(),
                                    error: err.to_string(),
                                });
                                read_term = Some(RecvError::ProtocolError);
                                write_term = SendError::ProtocolError;
                                link_term = DisconnectReason::ProtocolError(err.to_string());
                                break;
                            }
                        }
                        LinkIntEvent::FlushDelayPassed => {
                            // Link requires send buffer flushing.
                            let link = self.links[id].as_mut().unwrap();
                            tracing::trace!("flushing link {id}");
                            self.idle_links.retain(|&idle_id| idle_id != id);
                            link.start_flush();
                        }
                        LinkIntEvent::TxError(err) | LinkIntEvent::RxError(err) => {
                            // Link has failed.
                            tracing::warn!("disconnecting link {id} due to IO error: {err}");
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
                        LinkIntEvent::Disconnect => {
                            // Local request to disconnect link.
                            let link = self.links[id].as_mut().unwrap();
                            if link.disconnecting.is_none() {
                                tracing::info!("starting disconnection of link {id} by local request");
                                link.disconnecting = Some(DisconnectInitiator::Local);
                                self.idle_links.retain(|&idle_id| idle_id != id);
                                link.start_flush();
                            }
                        }
                    }
                }
                TaskEvent::WriteRx { id, data } => {
                    tracing::trace!("sending data of size {} over idle link {id}", data.len());
                    self.idle_links.retain(|&idle_id| idle_id != id);
                    self.send_reliable_over_link(id, ReliableMsg::Data(data));
                }
                TaskEvent::SendConsumed => {
                    let id = self.idle_links.pop().unwrap();
                    let consumed = self.rxed_reliable_consumed_since_last_ack as u32;
                    tracing::trace!("acking {consumed} consumed bytes over idle link {id}");
                    self.send_reliable_over_link(id, ReliableMsg::Consumed(consumed));
                    self.rxed_reliable_consumed_since_last_ack = 0;
                    self.rxed_reliable_consumed_force_ack = false;
                }
                TaskEvent::WriteEnd => {
                    tracing::debug!("sender was dropped");
                    self.write_rx = None;
                    if let Some(id) = self.idle_links.pop() {
                        tracing::debug!("sending SendFinish over idle link {id}");
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
                                if link.unconfirmed.is_none() {
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
                    tracing::warn!("acknowledgement timeout on link {id}");
                    self.unconfirm_link(id, NotWorkingReason::AckTimeout);
                }
                TaskEvent::Resend(packet) => {
                    let id = sendable_idle_link_id.unwrap();
                    self.idle_links.retain(|&idle_id| idle_id != id);
                    tracing::trace!("resending message {} over idle link {id}", packet.seq);
                    self.resend_reliable_over_link(id, packet);
                }
                TaskEvent::ReadDropped => {
                    tracing::debug!("receiver was dropped");
                    self.read_tx = None;
                    self.read_closed_rx = None;
                    if let Some(id) = self.idle_links.pop() {
                        tracing::debug!("sending ReceiveFinish over idle link {id}");
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
                    tracing::trace!("requesting ping of link {id}");
                    let link = self.links[id].as_mut().unwrap();
                    link.send_ping = true;
                    self.flush_link(id);
                }
                TaskEvent::LinkPingTimeout(id) => {
                    tracing::warn!("removing link {id} due to ping timeout");
                    self.remove_link(id, DisconnectReason::PingTimeout);
                }
                TaskEvent::LinkUnconfirmedTimeout(id) => {
                    tracing::warn!("removing link {id} due to unconfirmed timeout");
                    self.remove_link(id, DisconnectReason::UnconfirmedTimeout);
                }
                TaskEvent::LinkSendTimeout(id) => {
                    tracing::warn!("removing link {id} due to send timeout");
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
                TaskEvent::ServerChanged => {
                    tracing::warn!("disconnecting because server id changed");
                    result = Err(TaskError::ServerIdMismatch);
                    read_term = Some(RecvError::ServerIdMismatch);
                    write_term = SendError::ServerIdMismatch;
                    link_term = DisconnectReason::ServerIdMismatch;
                    break;
                }
            }

            // Check for link ping exceeding configured limit.
            if let Some(max_ping) = self.cfg.link_max_ping {
                let all_links_slow = self.links.iter().all(|link_opt| {
                    link_opt
                        .as_ref()
                        .map(|link| link.unconfirmed.is_some() || link.is_blocked() || link.roundtrip > max_ping)
                        .unwrap_or(true)
                });

                if !all_links_slow {
                    let slow: Vec<_> = self
                        .links
                        .iter()
                        .enumerate()
                        .filter_map(|(id, link_opt)| match link_opt {
                            Some(link) if link.unconfirmed.is_none() && link.roundtrip > max_ping => {
                                tracing::warn!(
                                    "unconfirming link {id} due to slow ping of {} ms",
                                    link.roundtrip.as_millis()
                                );
                                Some(id)
                            }
                            _ => None,
                        })
                        .collect();

                    for id in slow {
                        self.unconfirm_link(id, NotWorkingReason::MaxPingExceeded);
                    }
                }
            }
        }

        // Publish termination reasons.
        let _ = self.result_tx.send_replace(result.clone());
        if *self.read_error_tx.borrow() == Some(RecvError::TaskTerminated) {
            self.read_error_tx.send_replace(read_term);
        }
        if *self.write_error_tx.borrow() == SendError::TaskTerminated {
            self.write_error_tx.send_replace(write_term);
        }
        for link in &mut self.links {
            if let Some(link) = link.take() {
                link.notify_disconnected(link_term.clone());
            }
        }

        match &result {
            Ok(()) => tracing::debug!("link aggregator task exiting"),
            Err(err) => tracing::warn!("link aggregator task failed: {err}"),
        }
        result
    }

    /// Adds a newly established link and returns its id.
    fn add_link(&mut self, mut link: LinkInt<TX, RX, TAG>) -> usize {
        link.report_ready();
        link.unconfirmed = Some((Instant::now(), NotWorkingReason::New));

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
        tracing::debug!("removing link {id} for reason {reason:?}");

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
            .any(|link_opt| link_opt.as_ref().map(|link| link.unconfirmed.is_none()).unwrap_or_default());

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
                    "decreasing unacked limit of link {id} to {} bytes",
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
        match self.tx_overrun_since {
            Some(since) if since.elapsed() >= Duration::from_secs(1) => {
                tracing::trace!("re-arming send overrun handling due to timeout");
                self.tx_overrun = SendOverrun::Armed;
                self.tx_overrun_since = None
            }
            _ => (),
        }

        // Decrease data limits of links that approach maximum ping.
        let all_links_slow;
        match self.cfg.link_max_ping {
            Some(max_ping) => {
                all_links_slow = self.links.iter().all(|link_opt| {
                    link_opt
                        .as_ref()
                        .map(|link| {
                            link.unconfirmed.is_some() || link.is_blocked() || link.roundtrip > max_ping / 2
                        })
                        .unwrap_or(true)
                });

                if !all_links_slow {
                    for (id, link_opt) in self.links.iter_mut().enumerate() {
                        match link_opt {
                            Some(link)
                                if link.unconfirmed.is_none()
                                    && link.txed_unacked_data_limit_increased.is_none()
                                    && link.roundtrip > max_ping * 3 / 4 =>
                            {
                                // Decrease limit.
                                let current = link.txed_unacked_data.min(link.txed_unacked_data_limit);
                                link.txed_unacked_data_limit = current * 95 / 100;
                                tracing::trace!(
                                    "decreasing unacked limit of link {id} to {} bytes due to ping",
                                    link.txed_unacked_data_limit
                                );

                                // Block link from increasing its send data limit.
                                link.txed_unacked_data_limit_increased = Some(coming_seq);
                                link.txed_unacked_data_limit_increased_consecutively = 0;
                            }
                            _ => (),
                        }
                    }
                }
            }
            None => {
                all_links_slow = true;
            }
        };

        // Check if data is available for sending but no link is available.
        let send_data_avail = self.write_rx.as_mut().map(|rx| rx.try_peek().is_ok()).unwrap_or_default()
            || !self.resend_queue.is_empty();
        let sendable_link_avail = self.links.iter().any(|link_opt| {
            link_opt
                .as_ref()
                .map(|link| {
                    !link.tx_pending
                        && link.unconfirmed.is_none()
                        && !link.is_blocked()
                        && link.txed_unacked_data < link.txed_unacked_data_limit
                })
                .unwrap_or_default()
        });

        // Increase the unacked data limits of links that are currently blocked by it.
        if send_data_avail && !sendable_link_avail {
            for (id, link_opt) in self.links.iter_mut().enumerate() {
                match link_opt {
                    Some(link)
                        if !link.tx_pending
                            && link.unconfirmed.is_none()
                            && !link.is_blocked()
                            && link.txed_unacked_data >= link.txed_unacked_data_limit
                            && link.txed_unacked_data_limit_increased.is_none()
                            && link.txed_unacked_data_limit < self.cfg.link_unacked_limit.get()
                            && self
                                .cfg
                                .link_max_ping
                                .map(|max_ping| link.roundtrip <= max_ping / 2 || all_links_slow)
                                .unwrap_or(true) =>
                    {
                        // Increase limit, faster if done many times consecutively.
                        link.txed_unacked_data_limit =
                            if link.txed_unacked_data_limit_increased_consecutively >= 100 {
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
                            "increasing unacked limit of link {id} to {} bytes (done {} times without overrun)",
                            link.txed_unacked_data_limit,
                            link.txed_unacked_data_limit_increased_consecutively
                        );

                        // Block link from increasing limit again until newly sent data is received.
                        link.txed_unacked_data_limit_increased = Some(coming_seq);
                        link.txed_unacked_data_limit_increased_consecutively =
                            link.txed_unacked_data_limit_increased_consecutively.saturating_add(1);
                    }
                    _ => (),
                }
            }
        }

        // Reset consecutive increase count.
        if !low_level {
            for link_opt in self.links.iter_mut() {
                if let Some(link) = link_opt.as_mut() {
                    link.txed_unacked_data_limit_increased_consecutively = 0;
                }
            }
        }
    }

    /// Computes the earliest link-specific timeout.
    fn earliest_link_specific_timeout(
        &self, timeout: Duration, since_fn: impl Fn(&LinkInt<TX, RX, TAG>) -> Option<Instant>,
    ) -> impl Future<Output = usize> {
        let earliest_timeout = self
            .links
            .iter()
            .enumerate()
            .filter_map(|(id, link_opt)| link_opt.as_ref().and_then(&since_fn).map(|sent| (id, sent + timeout)))
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
    fn earliest_confirm_timeout(&self) -> Option<(usize, Instant)> {
        for p in &self.txed_packets {
            if let SentReliableStatus::Sent { link_id, sent, resent, .. } = &*p.status.borrow() {
                let link = self.links[*link_id].as_ref().unwrap();
                let dur_factor = if *resent { 3 } else { 1 };
                let dur = (link.roundtrip * self.cfg.link_ack_timeout_roundtrip_factor.get() * dur_factor)
                    .clamp(self.cfg.link_ack_timeout_min, self.cfg.link_ack_timeout_max);
                return Some((*link_id, *sent + dur));
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
                    if link.current_ping_sent.is_none() && !link.send_ping && link.unconfirmed.is_none() =>
                {
                    match self.cfg.link_ping {
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
        tracing::trace!("sending reliable message {seq} over link {id}: {reliable_msg:?}");
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
                link_id: id,
                msg: reliable_msg,
                resent: false,
            }),
        };
        self.txed_packets.push_back(Arc::new(packet));

        seq
    }

    /// Resends a packet over the specified link.
    fn resend_reliable_over_link(&mut self, id: usize, packet: Arc<SentReliable>) {
        let link = self.links[id].as_mut().unwrap();

        // Extract message and link used for sending.
        let mut status = packet.status.borrow_mut();
        let SentReliableStatus::ResendQueued { msg: reliable_msg } = &*status else {
            unreachable!("message was not queued for resending")
        };

        // Send data.
        tracing::trace!("resending reliable message {} over link {id}: {:?}", packet.seq, reliable_msg);
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
            link_id: id,
            msg: reliable_msg.clone(),
            resent: true,
        };
    }

    /// Unconfirms a link.
    fn unconfirm_link(&mut self, id: usize, reason: NotWorkingReason) {
        // Mark link as unconfirmed.
        let link = self.links[id].as_mut().unwrap();
        link.unconfirmed = Some((Instant::now(), reason));
        self.idle_links.retain(|&idle_id| idle_id != id);
        self.unflushed_links.remove(&id);

        // Flush link.
        link.start_flush();

        // Reset limits.
        link.reset();

        // Mark packets as being resent and put them into resend queue.
        for p in &mut self.txed_packets {
            let mut status = p.status.borrow_mut();
            match &*status {
                SentReliableStatus::Sent { link_id, msg, .. } if *link_id == id => {
                    // Update link statistics.
                    if let ReliableMsg::Data(data) = &msg {
                        let old_link = self.links[*link_id].as_mut().unwrap();
                        old_link.txed_unacked_data -= data.len();
                    }

                    *status = SentReliableStatus::ResendQueued { msg: msg.clone() };
                    self.resend_queue.push_back(p.clone());
                }
                _ => (),
            };
        }

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
        let others_slow = match self.cfg.link_max_ping {
            Some(max_ping) => self.links.iter().enumerate().all(|(link_id, link_opt)| {
                link_opt
                    .as_ref()
                    .map(|link| {
                        link_id == id
                            || link.unconfirmed.is_some()
                            || link.is_blocked()
                            || link.roundtrip > max_ping
                    })
                    .unwrap_or(true)
            }),
            None => false,
        };

        if let Some(link) = self.links[id].as_mut() {
            match link.test {
                LinkTest::Failed(when) if when.elapsed() >= self.cfg.link_retest_interval => {
                    tracing::trace!("link {id} is ready for retry of test");
                    link.test = LinkTest::Inactive;
                }
                _ => (),
            }

            match link.test {
                LinkTest::Inactive => {
                    if link.unconfirmed.is_some()
                        && link.tx_polling().is_none()
                        && link.current_ping_sent.is_none()
                        && !link.has_outstanding_ack()
                    {
                        let test_data_limit = if self.cfg.link_max_ping.is_some() {
                            self.cfg.link_unacked_init.get()
                        } else {
                            self.cfg.link_unacked_limit.get().min(self.cfg.send_buffer.get() as usize)
                        }
                        .min(self.cfg.link_test_data_limit);
                        let test_data = link.send_test_data(self.cfg.io_write_size.get(), test_data_limit);
                        link.send_ping = true;
                        link.test = LinkTest::InProgress;
                        tracing::debug!("started test of link {id} using {test_data} bytes of test data");
                    }
                    None
                }
                LinkTest::InProgress => {
                    if link.current_ping_sent.is_none() && !link.send_ping {
                        // Ping has completed.

                        if link.roundtrip <= self.cfg.link_ack_timeout_max / 2
                            && self
                                .cfg
                                .link_max_ping
                                .map(|max_ping| link.roundtrip <= max_ping || others_slow)
                                .unwrap_or(true)
                        {
                            // Ping response arrived quickly enough, thus mark link as confirmed.
                            tracing::debug!(
                                "link {id} successfully completed test with ping {} ms",
                                link.roundtrip.as_millis()
                            );
                            link.unconfirmed = None;
                            link.test = LinkTest::Inactive;

                            self.idle_links.retain(|&idle_id| idle_id != id);
                            link.report_ready();

                            None
                        } else {
                            // Link is too slow, schedule retest.
                            tracing::debug!(
                                "link {id} failed test with ping {} ms, retrying in {} s",
                                link.roundtrip.as_millis(),
                                self.cfg.link_retest_interval.as_secs()
                            );
                            let when = Instant::now();
                            link.test = LinkTest::Failed(when);
                            match &mut link.unconfirmed {
                                Some((_since, reason)) => *reason = NotWorkingReason::TestFailed,
                                None => link.unconfirmed = Some((Instant::now(), NotWorkingReason::TestFailed)),
                            }
                            Some(when + self.cfg.link_retest_interval)
                        }
                    } else {
                        None
                    }
                }
                LinkTest::Failed(when) => Some(when + self.cfg.link_retest_interval),
            }
        } else {
            None
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
    fn handle_received_msg(&mut self, id: usize, msg: LinkMsg, data: Option<Bytes>) -> Result<(), io::Error> {
        let link = self.links[id].as_mut().unwrap();

        match msg {
            LinkMsg::Ping => {
                // Respond with pong on same link.
                tracing::trace!("ping received, requesting sending resposne");
                link.send_pong = true;
                self.flush_link(id);
            }
            LinkMsg::Pong => {
                if let Some(current_ping_sent) = link.current_ping_sent.take() {
                    let elapsed = current_ping_sent.elapsed();
                    tracing::trace!("ping round-trip time is {} ms", elapsed.as_millis());
                    link.roundtrip = elapsed;
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
                tracing::trace!("received reliable message {seq}: {reliable_msg:?}");
                self.handle_received_reliable_msg(id, seq, reliable_msg)?;
            }
            LinkMsg::Ack { received } => {
                tracing::trace!("link {id} acked reception up to {received}");
                self.handle_ack(id, received);
            }
            LinkMsg::TestData { size } => {
                tracing::trace!("link {id} received {size} bytes of test data");
            }
            LinkMsg::SetBlock { blocked } => {
                tracing::debug!("remote block status of link {id} has become {blocked}");
                link.remotely_blocked.store(blocked, Ordering::SeqCst);
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
                            tracing::info!("removing link {id} due to local request");
                            self.remove_link(id, DisconnectReason::LocallyRequested);
                        }
                    }
                    Some(DisconnectInitiator::Remote) => {
                        return Err(protocol_err!("received Goodbye message more than once"));
                    }
                    None => {
                        // Remote endpoint is initiating disconnection.
                        tracing::debug!("remote requests disconnection of link {id}");
                        link.disconnecting = Some(DisconnectInitiator::Remote);
                    }
                }
            }
            LinkMsg::Welcome { .. } | LinkMsg::Connect { .. } | LinkMsg::Accepted | LinkMsg::Refused { .. } => {
                return Err(protocol_err!("received unexpected message"))
            }
        }

        Ok(())
    }

    /// Handle received data.
    fn handle_received_reliable_msg(&mut self, id: usize, seq: Seq, msg: ReliableMsg) -> Result<(), io::Error> {
        // Update link and queue sending of ack.
        let link = self.links[id].as_mut().unwrap();
        link.tx_ack_queue.push_back(seq);
        self.idle_links.retain(|&idle_id| idle_id != id);
        link.report_ready();

        if seq < self.rx_seq {
            // The sequence number belongs to a packet that has already been
            // received and consumed. Thus the acknowledgement has been
            // lost and must be resend.
            tracing::trace!("rereceived consumed reliable message {}", seq);
        } else {
            let offset = (seq - self.rx_seq) as usize;
            if self.rxed_reliable.len() <= offset {
                self.rxed_reliable.resize(offset + 1, None);
            }

            if self.rxed_reliable[offset].is_none() {
                tracing::trace!("received reliable message {}", seq);

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
                        tracing::trace!("remote consumed {consumed} bytes");
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
                tracing::trace!("rereceived unconsumed reliable message {}", seq);
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
            || (self.rxed_reliable_size == 0 && self.rxed_reliable_consumed_since_last_ack > 0)
            || self.rxed_reliable_consumed_force_ack
    }

    /// Handles a received acknowledgement.
    fn handle_ack(&mut self, id: usize, rxed_seq: Seq) {
        let link = self.links[id].as_mut().unwrap();
        tracing::trace!("processing received ack for {rxed_seq} on link {id}");

        // Possibly unblock send buffer increase.
        match link.txed_unacked_data_limit_increased {
            Some(last_increased) if last_increased <= rxed_seq => {
                tracing::trace!("re-allowing increase of send limit of link {id}");
                link.txed_unacked_data_limit_increased = None;
            }
            _ => (),
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

                    link.roundtrip = (99 * link.roundtrip + sent.elapsed()) / 100;

                    *status = SentReliableStatus::Received { size };
                }
                SentReliableStatus::ResendQueued { msg } => {
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

            let status = packet.status.borrow();
            if let SentReliableStatus::Received { size, .. } = &*status {
                self.txed_unconsumable -= size;

                drop(status);
                self.txed_packets.pop_front();
            } else {
                break;
            }
        }
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

        if !self.tx_seq_avail() {
            tracing::warn!("no sequence number available for sending");
        }

        if self.read_tx.is_none() || self.write_rx.is_none() {
            tracing::trace!("direction={:?} read_tx_none={} write_tx_none={} txed_packets={} txed_packets_front={:?} \
                resend_queue={} resend_queue_front={:?} txed_unconsumed={} rxed_reliable={} rxed_reliable_front={:?} \
                rxed_reliable_size={} rxed_reliable_consumed_since_last_ack={}, send_finish_sent={} receive_finish_sent={}",
                &self.direction, self.read_tx.is_none(), self.write_rx.is_none(),
                self.txed_packets.len(), self.txed_packets.front(), self.resend_queue.len(),
                self.resend_queue.front(), self.txed_unconsumed, self.rxed_reliable.len(),
                self.rxed_reliable.front(), self.rxed_reliable_size, self.rxed_reliable_consumed_since_last_ack,
                self.send_finish_sent, self.receive_finish_sent,
            );
        }
    }
}

impl<TX, RX, TAG> IntoFuture for Task<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + Sync + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + Sync + 'static,
    TAG: Send + Sync + 'static,
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
