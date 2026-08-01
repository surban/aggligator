//! Internal link data.

use bytes::Bytes;
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt, future, future::poll_fn};
use std::{
    collections::VecDeque,
    fmt,
    io::{self, Error, ErrorKind},
    mem,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    select,
    sync::{mpsc, watch},
};

use crate::{
    agg::task::{SentReliable, SentReliableStatus},
    cfg::{Cfg, ExchangedCfg, LinkCfg},
    control::{Direction, DisconnectReason, Link, LinkIntervalStats, LinkStats, NotWorkingReason},
    exec::time::{Instant, sleep_until},
    id::{ConnId, LinkId},
    msg::LinkMsg,
    seq::Seq,
};

/// Optional deadline.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Deadline {
    instant: Option<Instant>,
    reason: &'static str,
}

impl Deadline {
    /// Unset deadline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Require specified deadline.
    pub fn require(&mut self, reason: &'static str, deadline: Instant) {
        if self.instant.is_none_or(|existing| deadline < existing) {
            self.instant = Some(deadline);
            self.reason = reason;
        }
    }

    /// Wait until deadline elapses or forever, if unset.
    pub async fn wait(&self) -> &'static str {
        match self.instant {
            None => future::pending().await,
            Some(deadline) => sleep_until(deadline).await,
        }

        self.reason
    }
}

/// Link event.
#[derive(Debug)]
pub(crate) enum LinkIntEvent {
    /// Link has become ready for sending.
    TxReady,
    /// Link has been flushed.
    TxFlushed,
    /// Sending over the link has failed.
    TxError(io::Error),
    /// A message has been received.
    Rx {
        /// Message.
        msg: LinkMsg,
        /// Data, if data message.
        data: Option<Bytes>,
    },
    /// Receiving over the link has failed.
    RxError(io::Error),
    /// Link now requires flushing.
    FlushRequired,
    /// Local disconnection request.
    Disconnect,
    /// Link blocked status has changed.
    BlockedChanged,
    /// Link configuration changed.
    LinkCfgChanged,
}

/// Link test status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkTest {
    /// Link is not being tested.
    Inactive,
    /// Link test is in progress.
    InProgress,
    /// Link test failed.
    Failed(Instant),
}

/// Initiator of disconnection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisconnectInitiator {
    /// Locally initiated disconnection in progress.
    Local,
    /// Remotely initiated disconnection in progress.
    Remote,
}

/// Internal link data.
pub(crate) struct LinkInt<TX, RX, TAG> {
    /// User-supplied link name.
    tag: Arc<TAG>,
    /// Connection id.
    conn_id: ConnId,
    /// Link id.
    link_id: LinkId,
    /// Direction of link.
    direction: Direction,
    /// Connection configuration.
    cfg: Arc<Cfg>,
    /// Configuration of remote endpoint.
    remote_cfg: Arc<ExchangedCfg>,
    /// Link-specific configuration.
    link_cfg: Option<LinkCfg>,
    /// Sender for updating link-specific configuration.
    link_cfg_tx: watch::Sender<Option<LinkCfg>>,
    /// Receiver for updating link-specific configuration.
    link_cfg_rx: watch::Receiver<Option<LinkCfg>>,
    /// Whether the Accepeted message needs to be sent.
    pub(crate) needs_tx_accepted: bool,
    /// Transmit sink.
    tx: TX,
    /// Data to transmit next.
    tx_data: Option<Bytes>,
    /// Last transmit error.
    tx_error: Option<io::Error>,
    /// Whether the transmit sink failed previously.
    tx_failed: bool,
    /// Since when sink `tx` is being polled for readyness.
    tx_polling: Option<Instant>,
    /// Whether sink `tx` returned pending status when polled for readyness.
    pub(crate) tx_pending: bool,
    /// When last message has been sent.
    pub(crate) tx_last_msg: Option<Instant>,
    /// Sequence number of sent and not yet acknowledged packet.
    txed_unacked: Option<Seq>,
    /// Packets that have been sent over this link but not yet become consumable by the remote endpoint.
    pub(super) txed_packets: VecDeque<Weak<SentReliable>>,
    /// Since when the transmit part of the link is idle.
    tx_idle_since: Option<Instant>,
    /// Number of bytes sent that are not yet flushed.
    txed_unflushed: usize,
    /// Performing flushing of sink `tx`.
    tx_flushing: bool,
    /// When a data message was first sent over link after it was flushed.
    txed_first_unflushed_data: Option<Instant>,
    /// Number of bytes sent for which no acknowledgement has been received yet.
    pub(crate) txed_unacked_data: usize,
    /// Limit of sent unacknowledged bytes.
    pub(crate) txed_unacked_data_limit: usize,
    /// Sequence number when limit of sent unacknowledged bytes was last increased.
    pub(crate) txed_unacked_data_limit_increased: Option<Seq>,
    /// Times `txed_unacked_data_limit` was increased consecutively.
    pub(crate) txed_unacked_data_limit_increased_consecutively: usize,
    /// Acks queued for sending.
    pub(crate) tx_ack_queue: VecDeque<Seq>,
    /// Number of acks sent since last flush.
    txed_acks_unflushed: usize,
    /// When oldest unflushed ack was sent.
    txed_first_unflushed_ack: Option<Instant>,
    /// Receive stream.
    rx: RX,
    /// Received data message, when waiting for the corresponding data packet.
    rxed_data_msg: Option<LinkMsg>,
    /// Reason for link disconnection.
    disconnected_tx: watch::Sender<DisconnectReason>,
    /// Disconnect notification sender.
    disconnect_tx: mpsc::Sender<()>,
    /// Graceful disconnect request receiver.
    disconnect_rx: mpsc::Receiver<()>,
    /// Link blocked by user.
    pub(crate) blocked: Arc<AtomicBool>,
    /// Blocked status last sent to remote endpoint.
    pub(crate) blocked_sent: bool,
    /// Link blocking changed.
    pub(crate) blocked_changed_tx: mpsc::Sender<()>,
    /// Link blocking changed receiver.
    blocked_changed_rx: mpsc::Receiver<()>,
    /// Link blocking changed notification to link handle.
    pub(crate) blocked_changed_out_tx: watch::Sender<()>,
    /// Link blocking changed notification to link handle.
    blocked_changed_out_rx: watch::Receiver<()>,
    /// Link blocked by remote endpoint.
    pub(crate) remotely_blocked: Arc<AtomicBool>,
    /// Since when the link is unconfirmed, i.e. it has not been tested or message
    /// acknowledgement timed out.
    unconfirmed: Option<(Instant, NotWorkingReason)>,
    /// Channel for publishing `unconfirmed`.
    unconfirmed_tx: watch::Sender<Option<(Instant, NotWorkingReason)>>,
    /// Channel for publishing `unconfirmed`.
    unconfirmed_rx: watch::Receiver<Option<(Instant, NotWorkingReason)>>,
    /// Link test status.
    pub(crate) test: LinkTest,
    /// Last measured roundtrip duration.
    pub(crate) roundtrip: Duration,
    /// Number of reliable roundtrip estimates.
    pub(crate) roundtrip_estimates: Option<usize>,
    /// When last ping has been performed.
    pub(crate) last_ping: Option<Instant>,
    /// When current (not yet answered) ping has been sent.
    pub(crate) current_ping_sent: Option<Instant>,
    /// Send ping when link becomes ready for sending.
    pub(crate) send_ping: bool,
    /// Send ping reply when link becomes ready for sending.
    pub(crate) send_pong: bool,
    /// Initiator of disconnection.
    pub(crate) disconnecting: Option<DisconnectInitiator>,
    /// Goodbye message has been sent.
    pub(crate) goodbye_sent: bool,
    /// User data provided by remote endpoint.
    remote_user_data: Arc<Vec<u8>>,
    /// Link statistics calculator.
    stats: LinkStatistican,
}

impl<TX, RX, TAG> fmt::Debug for LinkInt<TX, RX, TAG> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LinkInt")
            .field("conn_id", &self.conn_id)
            .field("link_id", &self.link_id)
            .field("direction", &self.direction)
            .finish_non_exhaustive()
    }
}

impl<TX, RX, TAG> LinkInt<TX, RX, TAG> {
    /// User-supplied link name.
    pub(crate) fn tag(&self) -> &TAG {
        &self.tag
    }

    /// Remote user data.
    pub(crate) fn remote_user_data(&self) -> &[u8] {
        &self.remote_user_data
    }

    /// Configuration of remote endpoint.
    pub(crate) fn remote_cfg(&self) -> Arc<ExchangedCfg> {
        self.remote_cfg.clone()
    }
}

impl<TX, RX, TAG> LinkInt<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin,
    TX: Sink<Bytes, Error = io::Error> + Unpin,
    TAG: fmt::Display,
{
    /// Creates new internal link data.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tag: TAG, conn_id: ConnId, tx: TX, rx: RX, cfg: Arc<Cfg>, remote_cfg: ExchangedCfg,
        link_cfg: Option<LinkCfg>, direction: Direction, roundtrip: Duration, remote_user_data: Vec<u8>,
    ) -> Self {
        let (link_cfg_tx, link_cfg_rx) = watch::channel(link_cfg.clone());
        let (disconnected_tx, _) = watch::channel(DisconnectReason::TaskTerminated);
        let (disconnect_tx, disconnect_rx) = mpsc::channel(1);
        let (blocked_changed_tx, blocked_changed_rx) = mpsc::channel(2);
        let stats = LinkStatistican::new(&cfg.stats_intervals, roundtrip);
        let (unconfirmed_tx, unconfirmed_rx) = watch::channel(None);
        let (blocked_changed_out_tx, blocked_changed_out_rx) = watch::channel(());

        let txed_unacked_data_limit = match &link_cfg {
            Some(link_cfg) => link_cfg.unacked_init.get(),
            None => cfg.link.unacked_init.get(),
        };

        Self {
            tag: Arc::new(tag),
            conn_id,
            link_id: LinkId::generate(),
            direction,
            tx,
            tx_data: None,
            tx_error: None,
            tx_failed: false,
            rx,
            cfg,
            link_cfg,
            link_cfg_tx,
            link_cfg_rx,
            remote_cfg: Arc::new(remote_cfg),
            needs_tx_accepted: direction == Direction::Incoming,
            disconnected_tx,
            disconnect_tx,
            disconnect_rx,
            stats,
            goodbye_sent: false,
            tx_polling: None,
            blocked: Arc::new(AtomicBool::new(false)),
            blocked_sent: false,
            blocked_changed_tx,
            blocked_changed_rx,
            blocked_changed_out_tx,
            blocked_changed_out_rx,
            remotely_blocked: Arc::new(AtomicBool::new(false)),
            unconfirmed: None,
            unconfirmed_tx,
            unconfirmed_rx,
            test: LinkTest::Inactive,
            txed_unflushed: 0,
            tx_flushing: false,
            txed_first_unflushed_data: None,
            rxed_data_msg: None,
            tx_last_msg: None,
            txed_unacked: None,
            txed_packets: VecDeque::new(),
            last_ping: None,
            current_ping_sent: None,
            send_ping: false,
            send_pong: false,
            roundtrip,
            roundtrip_estimates: Some(0),
            disconnecting: None,
            txed_unacked_data: 0,
            txed_unacked_data_limit,
            txed_unacked_data_limit_increased: None,
            txed_unacked_data_limit_increased_consecutively: 45,
            txed_acks_unflushed: 0,
            txed_first_unflushed_ack: None,
            tx_ack_queue: VecDeque::new(),
            tx_idle_since: None,
            tx_pending: false,
            remote_user_data: Arc::new(remote_user_data),
        }
    }

    /// Link id.
    pub(crate) fn link_id(&self) -> LinkId {
        self.link_id
    }

    /// Link-specific configuration.
    pub(crate) fn link_cfg(&self) -> &LinkCfg {
        self.link_cfg.as_ref().unwrap_or(&self.cfg.link)
    }

    /// Update link-specific configuration.
    pub(crate) fn update_link_cfg(&mut self) {
        self.link_cfg = self.link_cfg_rx.borrow_and_update().clone();
    }

    /// Checks whether the sink has failed.
    fn check_tx_failed(&self) -> Result<(), io::Error> {
        match self.tx_failed {
            true => Err(Error::new(ErrorKind::ConnectionAborted, "link has failed")),
            false => Ok(()),
        }
    }

    /// Since when the link is unconfirmed, i.e. it has not been tested or message
    /// acknowledgement timed out.    
    pub(crate) fn unconfirmed(&self) -> &Option<(Instant, NotWorkingReason)> {
        &self.unconfirmed
    }

    /// Sets the unconfirmed state and publishes it.
    pub(crate) fn set_unconfirmed(&mut self, unconfirmed: Option<(Instant, NotWorkingReason)>) {
        self.unconfirmed = unconfirmed;
        self.unconfirmed_tx.send_if_modified(|m| {
            if *m != self.unconfirmed {
                m.clone_from(&self.unconfirmed);
                true
            } else {
                false
            }
        });
    }

    /// Returns the next event for this link.
    pub(crate) async fn event(&mut self) -> LinkIntEvent {
        let link_id = self.link_id();
        let tag = &self.tag;

        // Check for errors.
        if let Some(err) = self.tx_error.take() {
            return LinkIntEvent::TxError(err);
        }
        if let Err(err) = self.check_tx_failed() {
            return LinkIntEvent::TxError(err);
        }

        // Flush request task.
        let flush_req_task = {
            let mut deadline = Deadline::new();

            match self.txed_first_unflushed_data {
                Some(tx_first_sent) if !self.tx_flushing => {
                    if let Some(idle_since) = self.tx_idle_since {
                        deadline.require("link data idle flush", idle_since + self.link_cfg().flush_delay);
                    }
                    if let Some(link_flush_interval) = self.link_cfg().flush_interval {
                        deadline.require("link data flush interval", tx_first_sent + link_flush_interval);
                    }
                }
                _ => (),
            }

            if let (Some(txed_first_unflushed_ack), Some(link_ack_flush_interval)) =
                (self.txed_first_unflushed_ack, self.link_cfg().ack_flush_interval)
            {
                deadline.require("link ack flush interval", txed_first_unflushed_ack + link_ack_flush_interval);
            }

            async move {
                let reason = deadline.wait().await;
                tracing::trace!(?link_id, %tag, "deadline for {reason} elapsed");
            }
        };

        // Transmit task.
        let tx_task = async {
            loop {
                if self.tx_polling.is_none() {
                    assert!(self.tx_data.is_none());
                    future::pending().await
                } else if self.tx_flushing && self.tx_data.is_none() {
                    match self.tx.flush().await {
                        Ok(()) => {
                            self.tx_flushing = false;
                            self.txed_first_unflushed_data = None;
                            break LinkIntEvent::TxFlushed;
                        }
                        Err(err) => {
                            self.tx_failed = true;
                            break LinkIntEvent::TxError(err);
                        }
                    }
                } else {
                    let tx_ready = |cx: &mut Context| {
                        let res = self.tx.poll_ready_unpin(cx);
                        match &res {
                            Poll::Pending => self.tx_pending = true,
                            Poll::Ready(_) => self.tx_pending = false,
                        }
                        res
                    };
                    match poll_fn(tx_ready).await {
                        Ok(()) => match self.tx_data.take() {
                            Some(data) => {
                                self.txed_first_unflushed_data.get_or_insert_with(Instant::now);
                                if let Err(err) = self.tx.start_send_unpin(data) {
                                    self.tx_failed = true;
                                    break LinkIntEvent::TxError(err);
                                }
                            }
                            None => {
                                self.tx_polling = None;
                                break LinkIntEvent::TxReady;
                            }
                        },
                        Err(err) => {
                            tracing::debug!(?link_id, %tag, %err, "link poll ready failure");
                            self.tx_failed = true;
                            break LinkIntEvent::TxError(err);
                        }
                    }
                }
            }
        };

        // Receive task.
        let rx_task = async {
            loop {
                match self.rx.next().await {
                    Some(Ok(buf)) => {
                        self.stats.record(0, buf.len());

                        match self.rxed_data_msg.take() {
                            Some(msg) => {
                                break LinkIntEvent::Rx { msg, data: Some(buf) };
                            }
                            None => {
                                let cursor = io::Cursor::new(buf);
                                match LinkMsg::read(cursor) {
                                    Ok(msg) => {
                                        match (&msg, self.txed_unacked) {
                                            (LinkMsg::Ack { received }, Some(sent)) if *received >= sent => {
                                                self.txed_unacked = None
                                            }
                                            _ => (),
                                        }

                                        if let LinkMsg::Data { .. } = &msg {
                                            self.rxed_data_msg = Some(msg);
                                        } else {
                                            break LinkIntEvent::Rx { msg, data: None };
                                        }
                                    }
                                    Err(err) => break LinkIntEvent::RxError(err),
                                }
                            }
                        }
                    }
                    Some(Err(err)) => {
                        tracing::debug!(?link_id, %tag, %err, "link receive failure");
                        break LinkIntEvent::RxError(err);
                    }
                    None => {
                        tracing::debug!(?link_id, %tag, "link receive end");
                        break LinkIntEvent::RxError(io::ErrorKind::BrokenPipe.into());
                    }
                }
            }
        };

        select! {
            tx_event = tx_task => tx_event,
            rx_event = rx_task => rx_event,
            () = flush_req_task => LinkIntEvent::FlushRequired,
            Some(()) = self.blocked_changed_rx.recv() => LinkIntEvent::BlockedChanged,
            Some(()) = self.disconnect_rx.recv() => LinkIntEvent::Disconnect,
            Ok(()) = self.link_cfg_rx.changed() => LinkIntEvent::LinkCfgChanged,
        }
    }

    /// Waits for the link to become ready, sends a message and flushes it.
    pub(crate) async fn send_msg_and_flush(&mut self, msg: LinkMsg) -> Result<(), io::Error> {
        self.check_tx_failed()?;

        self.tx_polling = Some(Instant::now());
        self.tx.send(msg.encode()).await.inspect_err(|_| self.tx_failed = true)?;
        Ok(())
    }

    /// Send message over link, optionally followed by data.
    ///
    /// Link must be ready for sending.
    pub(crate) fn start_send_msg(&mut self, msg: LinkMsg, data: Option<Bytes>) {
        assert!(self.tx_polling.is_none());
        assert!(self.tx_data.is_none());

        if let Err(err) = self.check_tx_failed() {
            if self.tx_error.is_none() {
                self.tx_error = Some(err);
            }
            return;
        }

        self.tx_polling = Some(Instant::now());
        self.tx_idle_since = None;

        let encoded = msg.encode();
        let msg_len = encoded.len();
        let data_len = data.as_ref().map(|data| data.len()).unwrap_or_default();
        let total_len = msg_len + data_len;

        if let Err(err) = self.tx.start_send_unpin(encoded) {
            tracing::debug!(
                link_id =? self.link_id, tag =% self.tag(),
                %err, "link send failure"
            );
            self.tx_error = Some(err);
            self.tx_failed = true;
            return;
        }

        self.stats.record(total_len, 0);

        self.tx_data = data;
        self.tx_last_msg = Some(Instant::now());
        self.txed_unflushed = self.txed_unflushed.saturating_add(total_len);

        match &msg {
            LinkMsg::Ack { .. } | LinkMsg::Consumed { .. } => {
                self.txed_acks_unflushed += 1;
                self.txed_first_unflushed_ack.get_or_insert_with(Instant::now);
            }
            LinkMsg::Data { seq } => match self.txed_unacked {
                Some(txed_unacked) if txed_unacked > *seq => (),
                _ => self.txed_unacked = Some(*seq),
            },
            LinkMsg::Accepted
            | LinkMsg::Ping
            | LinkMsg::Pong
            | LinkMsg::SendFinish { .. }
            | LinkMsg::ReceiveClose { .. }
            | LinkMsg::ReceiveFinish { .. }
            | LinkMsg::Goodbye => self.start_flush(),
            _ => (),
        }

        if let Some(link_unflushed_limit) = self.link_cfg().unflushed_limit
            && self.txed_unflushed >= link_unflushed_limit.get()
        {
            self.start_flush();
        }
    }

    /// Flush the send buffer of the link.
    pub(crate) fn start_flush(&mut self) {
        self.txed_acks_unflushed = 0;
        self.txed_first_unflushed_ack = None;
        self.txed_unflushed = 0;

        self.mark_txed_packets_flushed();

        self.tx_flushing = true;
        self.tx_polling = Some(Instant::now());
    }

    /// Whether flushing is required because of sent acks.
    pub(crate) fn need_ack_flush(&self) -> bool {
        self.txed_acks_unflushed != 0
    }

    /// Whether flushing is required.
    pub(crate) fn needs_flush(&self) -> bool {
        self.txed_first_unflushed_data.is_some() && !self.tx_flushing
    }

    /// Whether the link has an outstanding acknowledgement.
    pub(crate) fn has_outstanding_ack(&self) -> bool {
        self.txed_unacked.is_some()
    }

    /// Report (again) when link becomes ready.
    pub(crate) fn report_ready(&mut self) {
        self.tx_polling = Some(Instant::now());
    }

    /// Sends test data over the link until send function starts blocking or
    /// `data_limit` is reached.
    pub(crate) fn send_test_data(&mut self, packet_size: usize, data_limit: usize) -> usize {
        assert!(self.tx_data.is_none());

        self.tx_polling = Some(Instant::now());
        self.txed_first_unflushed_data.get_or_insert_with(Instant::now);
        self.tx_idle_since = None;

        if let Err(err) = self.check_tx_failed() {
            if self.tx_error.is_none() {
                self.tx_error = Some(err);
            }
            return 0;
        }

        let mut sent = 0;
        while sent < data_limit {
            match poll_fn(|cx| self.tx.poll_ready_unpin(cx)).now_or_never() {
                Some(Ok(())) => (),
                Some(Err(err)) => {
                    self.tx_error = Some(err);
                    self.tx_failed = true;
                    break;
                }
                None => break,
            }

            let size = packet_size.min(data_limit - sent);
            if let Err(err) = self.tx.start_send_unpin(LinkMsg::TestData { size }.encode()) {
                self.tx_error = Some(err);
                self.tx_failed = true;
                break;
            }
            sent += size;
        }

        sent
    }

    /// Notifies of link disconnection.
    pub(crate) fn notify_disconnected(mut self, reason: DisconnectReason) {
        self.disconnected_tx.send_replace(reason);
        self.disconnect_rx.close();
    }

    /// Forefully terminates the connection.
    pub(crate) async fn terminate_connection(&mut self, mut expect_reply: bool) {
        let link_id = self.link_id();

        // Wait for link to become ready.
        tracing::debug!(
            ?link_id, tag =% self.tag(),
            "waiting for link to become ready for termination"
        );
        self.report_ready();
        loop {
            match self.event().await {
                LinkIntEvent::TxReady | LinkIntEvent::TxError(_) => break,
                LinkIntEvent::Rx { msg: LinkMsg::Terminate, .. } => expect_reply = false,
                _ => (),
            }
        }

        // Send termination message.
        tracing::debug!(
            ?link_id, tag =% self.tag(),
            "sending forceful connection termination"
        );
        match self.send_msg_and_flush(LinkMsg::Terminate).await {
            Ok(()) => {
                tracing::debug!(
                    ?link_id, tag =% self.tag(),
                    "forceful connection termination sent"
                );
            }
            Err(err) => {
                tracing::warn!(
                    ?link_id, tag =% self.tag(),
                    %err, "sending forceful connection termination failed"
                );
            }
        }

        // Wait for termination message, if required.
        if expect_reply {
            tracing::debug!(
                ?link_id, tag =% self.tag(),
                "waiting for forceful connection termination reply"
            );
            loop {
                match self.event().await {
                    LinkIntEvent::RxError(err) => {
                        tracing::warn!(
                            ?link_id, tag =% self.tag(),
                            %err, "receiving forceful connection termination reply failed"
                        );
                        break;
                    }
                    LinkIntEvent::Rx { msg: LinkMsg::Terminate, .. } => {
                        tracing::debug!(
                            ?link_id, tag =% self.tag(),
                            "forceful connection termination reply received"
                        );
                        break;
                    }
                    _ => (),
                }
            }
        }
    }

    /// Marks the send part of the link as idle.
    pub(crate) fn mark_idle(&mut self) {
        self.tx_idle_since = Some(Instant::now());
        self.stats.mark_idle();
    }

    /// Marks reliably transmitted packets on this link as flushed.
    pub(crate) fn mark_txed_packets_flushed(&self) {
        for packet in self.txed_packets.iter().rev() {
            let Some(packet) = packet.upgrade() else { continue };
            let mut status = packet.status.borrow_mut();
            match &mut *status {
                SentReliableStatus::Sent { flushed, link, .. } if *link == self.link_id => match flushed {
                    None => *flushed = Some(Instant::now()),
                    Some(_) => break,
                },
                _ => (),
            }
        }
    }

    /// Removes stale references to transmitted packets.
    pub(crate) fn clean_txed_packets(&mut self) {
        while let Some(packet) = self.txed_packets.front() {
            if let Some(packet) = packet.upgrade() {
                match &*packet.status.borrow() {
                    SentReliableStatus::Sent { link, .. } if *link == self.link_id => break,
                    _ => (),
                }
            }

            self.txed_packets.pop_front();
        }
    }

    /// Returns whether unacknowledged sent data is under the limit.
    pub(crate) fn is_sendable(&self) -> bool {
        self.txed_unacked_data < self.txed_unacked_data_limit
    }

    /// Since when transmitter is being polled for readyness.
    pub(crate) fn tx_polling(&self) -> Option<Instant> {
        self.tx_polling
    }

    /// Reset statistics and limits when the link is unconfirmed.
    pub(crate) fn reset(&mut self) {
        // Log hang in statistics.
        self.stats.current.hangs += 1;

        // Reset unacked data limit.
        self.txed_unacked_data_limit = (self.txed_unacked_data_limit / 2).max(128);
        self.txed_unacked_data_limit_increased = None;
        self.txed_unacked_data_limit_increased_consecutively = 0;

        tracing::trace!(
            link_id =? self.link_id(), tag =% self.tag(), hangs =% self.stats.current.hangs,
            "decreasing unacked limit of link to {} bytes due to hang",
            self.txed_unacked_data_limit
        );
    }

    /// Whether link is blocked locally or remotely.
    pub(crate) fn is_blocked(&self) -> bool {
        self.blocked.load(Ordering::Relaxed) || self.remotely_blocked.load(Ordering::Relaxed)
    }

    /// Publishes link statistics.
    pub(crate) fn publish_stats(&mut self) {
        self.stats.current.sent_unacked = self.txed_unacked_data as _;
        self.stats.current.unacked_limit = self.txed_unacked_data_limit as _;
        self.stats.current.roundtrip = self.roundtrip;

        self.stats.publish();
    }
}

impl<TX, RX, TAG> From<&LinkInt<TX, RX, TAG>> for Link<TAG> {
    fn from(link_int: &LinkInt<TX, RX, TAG>) -> Self {
        Self {
            conn_id: link_int.conn_id,
            link_id: link_int.link_id,
            direction: link_int.direction,
            tag: link_int.tag.clone(),
            cfg: link_int.cfg.clone(),
            link_cfg_tx: link_int.link_cfg_tx.clone(),
            disconnected_rx: link_int.disconnected_tx.subscribe(),
            disconnect_tx: link_int.disconnect_tx.clone(),
            stats_rx: link_int.stats.subscribe(),
            remote_user_data: link_int.remote_user_data.clone(),
            blocked: link_int.blocked.clone(),
            blocked_changed_tx: link_int.blocked_changed_tx.clone(),
            blocked_changed_rx: link_int.blocked_changed_out_rx.clone(),
            not_working_rx: link_int.unconfirmed_rx.clone(),
            remotely_blocked: link_int.remotely_blocked.clone(),
        }
    }
}

/// Link statistics keeper.
struct LinkStatistican {
    /// Channel for publishing statistics.
    tx: watch::Sender<LinkStats>,
    /// Current statistics.
    current: LinkStats,
    /// Statistics over time intervals that are being calculated.
    running_stats: Vec<LinkIntervalStats>,
}

impl LinkStatistican {
    /// Initializes link statistics.
    fn new(intervals: &[Duration], roundtrip: Duration) -> Self {
        let running_stats: Vec<_> = intervals.iter().map(|interval| LinkIntervalStats::new(*interval)).collect();

        let current = LinkStats {
            established: Instant::now(),
            total_sent: 0,
            total_recved: 0,
            sent_unacked: 0,
            unacked_limit: 0,
            roundtrip,
            hangs: 0,
            time_stats: running_stats.clone(),
        };

        Self { tx: watch::channel(current.clone()).0, current, running_stats }
    }

    /// Subscribes to link statistics.
    fn subscribe(&self) -> watch::Receiver<LinkStats> {
        self.tx.subscribe()
    }

    /// Publish link statistics.
    fn publish(&mut self) {
        let mut modified = false;

        for (rs, ts) in self.running_stats.iter_mut().zip(self.current.time_stats.iter_mut()) {
            if rs.start.elapsed() > rs.interval {
                if rs.sent == 0 {
                    rs.busy = false;
                }
                *ts = mem::replace(rs, LinkIntervalStats::new(rs.interval));
                modified = true;
            }
        }

        if modified {
            self.tx.send_replace(self.current.clone());
        }
    }

    /// Records sent and received data.
    fn record(&mut self, sent: usize, received: usize) {
        self.current.total_sent = self.current.total_sent.wrapping_add(sent as _);
        self.current.total_recved = self.current.total_recved.wrapping_add(received as _);

        for ts in &mut self.running_stats {
            ts.sent = ts.sent.wrapping_add(sent as _);
            ts.recved = ts.recved.wrapping_add(received as _);
        }
    }

    /// Records that the send part of the link has become idle.
    fn mark_idle(&mut self) {
        for ts in &mut self.running_stats {
            ts.busy = false;
        }
    }
}

#[cfg(feature = "dump")]
impl<TX, RX, TAG> From<&LinkInt<TX, RX, TAG>> for super::dump::LinkDump {
    fn from(link: &LinkInt<TX, RX, TAG>) -> Self {
        Self {
            present: true,
            link_id: link.link_id.0,
            unconfirmed: link.unconfirmed.is_some(),
            tx_flushing: link.tx_flushing,
            tx_flushed: link.txed_first_unflushed_data.is_none(),
            roundtrip: link.roundtrip.as_secs_f32(),
            tx_ack_queue: link.tx_ack_queue.len(),
            txed_unacked_data: link.txed_unacked_data,
            txed_unacked_data_limit: link.txed_unacked_data_limit,
            txed_unacked_data_limit_increased_consecutively: link.txed_unacked_data_limit_increased_consecutively,
            tx_idle: link.tx_idle_since.is_some(),
            tx_pending: link.tx_pending,
            total_sent: link.stats.current.total_sent,
            total_recved: link.stats.current.total_recved,
        }
    }
}
