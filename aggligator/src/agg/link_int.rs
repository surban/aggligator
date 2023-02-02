//! Internal link data.

use bytes::Bytes;
use futures::{future, future::poll_fn, FutureExt, Sink, SinkExt, Stream, StreamExt};
use std::{collections::VecDeque, fmt, io, mem, sync::Arc, task::Poll, time::Duration};
use tokio::{
    select,
    sync::{mpsc, watch},
    time::{sleep_until, Instant},
};

use crate::{
    cfg::{Cfg, ExchangedCfg, LinkSteering, UnackedLimit},
    control::{Direction, DisconnectReason, Link, LinkIntervalStats, LinkStats},
    id::{ConnId, LinkId},
    msg::LinkMsg,
    seq::Seq,
};

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
    /// Link has been idle for the configured flush delay and now requires flushing.
    FlushDelayPassed,
    /// Local disconnection request.
    Disconnect,
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

/// Metadata of a sent reliable packet.
#[derive(Debug, Clone)]
struct LinkSentReliable {
    /// Sequence number.
    seq: Seq,
    /// Time packet was sent.
    sent: Instant,
    /// Size of data sent and not yet acknowledged when packet was sent.
    unacked_data: usize,
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
    /// Configuration.
    cfg: Arc<Cfg>,
    /// Configuration of remote endpoint.
    remote_cfg: Arc<ExchangedCfg>,
    /// Whether the Accepeted message needs to be sent.
    pub(crate) needs_tx_accepted: bool,
    /// Transmit sink.
    tx: TX,
    /// Data to transmit next.
    tx_data: Option<Bytes>,
    /// Last transmit error.
    tx_error: Option<io::Error>,
    /// Since when sink `tx` is being polled for readyness.
    tx_polling: Option<Instant>,
    /// Whether sink `tx` returned pending status when polled for readyness.
    pub(crate) tx_pending: bool,
    /// When last message has been sent.
    pub(crate) tx_last_msg: Option<Instant>,
    /// Sent reliable packets that are not yet acknowledged.
    txed_packets: VecDeque<LinkSentReliable>,
    /// Since when the transmit part of the link is idle.
    tx_idle_since: Option<Instant>,
    /// Performing flushing of sink `tx`.
    tx_flushing: bool,
    /// Time of sending last message that has not yet been flushed.
    tx_unflushed: Option<Instant>,
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
    /// Whether this is the fastest link for sending.
    pub(crate) tx_fastest: bool,
    /// Receive sink.
    rx: RX,
    /// Received data message, when waiting for the corresponding data packet.
    rxed_data_msg: Option<LinkMsg>,
    /// Reason for link disconnection.
    disconnected_tx: watch::Sender<DisconnectReason>,
    /// Disconnect notification sender.
    disconnect_tx: mpsc::Sender<()>,
    /// Graceful disconnect request receiver.
    disconnect_rx: mpsc::Receiver<()>,
    /// Since when the link is unconfirmed, i.e. it has not been tested or message
    /// acknowledgement timed out.
    pub(crate) unconfirmed: Option<Instant>,
    /// Link test status.
    pub(crate) test: LinkTest,
    /// Average measured roundtrip duration.
    pub(crate) avg_roundtrip: Duration,
    /// Last measured roundtrip duration and unacked data.
    pub(crate) last_roundtrip: (Duration, usize),
    /// When last ping has been performed.
    pub(crate) last_ping: Option<Instant>,
    /// When current (not yet answered) ping has been sent and the amount of test data sent before it.
    pub(crate) current_ping_sent: Option<(Instant, usize)>,
    /// If Some(_), send ping when link becomes ready for sending.
    /// Specifies the amount of test data sent.
    pub(crate) send_ping: Option<usize>,
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
{
    /// Creates new internal link data.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tag: TAG, conn_id: ConnId, tx: TX, rx: RX, cfg: Arc<Cfg>, remote_cfg: ExchangedCfg, direction: Direction,
        roundtrip: Duration, remote_user_data: Vec<u8>,
    ) -> Self {
        let (disconnected_tx, _) = watch::channel(DisconnectReason::TaskTerminated);
        let (disconnect_tx, disconnect_rx) = mpsc::channel(1);
        let stats = LinkStatistican::new(&cfg.stats_intervals, roundtrip);
        let txed_unacked_data_limit =
            if let LinkSteering::UnackedLimit(UnackedLimit { init, .. }) = cfg.link_steering {
                init.get()
            } else {
                0
            };

        Self {
            tag: Arc::new(tag),
            conn_id,
            link_id: LinkId::generate(),
            direction,
            tx,
            tx_data: None,
            tx_error: None,
            rx,
            remote_cfg: Arc::new(remote_cfg),
            needs_tx_accepted: direction == Direction::Incoming,
            disconnected_tx,
            disconnect_tx,
            disconnect_rx,
            stats,
            goodbye_sent: false,
            tx_polling: None,
            unconfirmed: None,
            test: LinkTest::Inactive,
            tx_flushing: false,
            tx_unflushed: None,
            rxed_data_msg: None,
            tx_last_msg: None,
            txed_packets: VecDeque::new(),
            tx_fastest: false,
            last_ping: None,
            current_ping_sent: None,
            send_ping: None,
            send_pong: false,
            avg_roundtrip: roundtrip,
            last_roundtrip: (roundtrip, 0),
            disconnecting: None,
            txed_unacked_data: 0,
            txed_unacked_data_limit,
            txed_unacked_data_limit_increased: None,
            txed_unacked_data_limit_increased_consecutively: 45,
            txed_acks_unflushed: 0,
            tx_ack_queue: VecDeque::new(),
            tx_idle_since: None,
            tx_pending: false,
            cfg,
            remote_user_data: Arc::new(remote_user_data),
        }
    }

    /// Returns the next event for this link.
    pub(crate) async fn event(&mut self, id: usize) -> LinkIntEvent {
        if let Some(err) = self.tx_error.take() {
            return LinkIntEvent::TxError(err);
        }

        let flushable = !(self.tx_flushing || self.tx_unflushed.is_none());

        let tx_task = async {
            loop {
                if self.tx_polling.is_none() {
                    assert!(self.tx_data.is_none());
                    future::pending().await
                } else if self.tx_flushing && self.tx_data.is_none() {
                    match self.tx.flush().await {
                        Ok(()) => {
                            self.tx_flushing = false;
                            self.tx_unflushed = None;
                            break LinkIntEvent::TxFlushed;
                        }
                        Err(err) => break LinkIntEvent::TxError(err),
                    }
                } else {
                    match poll_fn(|cx| {
                        let res = self.tx.poll_ready_unpin(cx);
                        match &res {
                            Poll::Pending => self.tx_pending = true,
                            Poll::Ready(_) => self.tx_pending = false,
                        }
                        res
                    })
                    .await
                    {
                        Ok(()) => match self.tx_data.take() {
                            Some(data) => {
                                self.tx_unflushed = Some(Instant::now());
                                if let Err(err) = self.tx.start_send_unpin(data) {
                                    break LinkIntEvent::TxError(err);
                                }
                            }
                            None => {
                                self.tx_polling = None;
                                break LinkIntEvent::TxReady;
                            }
                        },
                        Err(err) => {
                            tracing::debug!("link {id} poll ready failure: {}", err);
                            break LinkIntEvent::TxError(err);
                        }
                    }
                }
            }
        };

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
                        tracing::debug!("link {id} receive failure: {}", err);
                        break LinkIntEvent::RxError(err);
                    }
                    None => {
                        tracing::debug!("link {id} receive end");
                        break LinkIntEvent::RxError(io::ErrorKind::BrokenPipe.into());
                    }
                }
            }
        };

        let flush_req_task = async {
            match self.tx_idle_since {
                Some(idle_since) if flushable => sleep_until(idle_since + self.cfg.link_flush_delay).await,
                _ => future::pending().await,
            }
        };

        select! {
            tx_event = tx_task => tx_event,
            rx_event = rx_task => rx_event,
            () = flush_req_task => LinkIntEvent::FlushDelayPassed,
            Some(()) = self.disconnect_rx.recv() => LinkIntEvent::Disconnect,
        }
    }

    /// Waits for the link to become ready, sends a message and flushes it.
    pub(crate) async fn send_msg_and_flush(&mut self, msg: LinkMsg) -> Result<(), io::Error> {
        self.tx_polling = Some(Instant::now());
        self.tx.send(msg.encode()).await?;
        self.tx_unflushed = None;
        Ok(())
    }

    /// Send message over link, optionally followed by data.
    ///
    /// Link must be ready for sending.
    pub(crate) fn start_send_msg(&mut self, msg: LinkMsg, data: Option<Bytes>) {
        assert!(self.tx_polling.is_none());
        assert!(self.tx_data.is_none());

        self.tx_polling = Some(Instant::now());
        self.tx_unflushed = Some(Instant::now());
        self.tx_idle_since = None;

        let encoded = msg.encode();
        let msg_len = encoded.len();
        let data_len = data.as_ref().map(|data| data.len()).unwrap_or_default();

        if let Err(err) = self.tx.start_send_unpin(encoded) {
            tracing::debug!("link send failure: {}", err);
            self.tx_error = Some(err);
            return;
        }

        self.stats.record(msg_len + data_len, 0);

        self.tx_data = data;
        self.tx_last_msg = Some(Instant::now());

        match &msg {
            LinkMsg::Ack { .. } | LinkMsg::Consumed { .. } => self.txed_acks_unflushed += 1,
            LinkMsg::Accepted
            | LinkMsg::Ping
            | LinkMsg::Pong
            | LinkMsg::SendFinish { .. }
            | LinkMsg::ReceiveClose { .. }
            | LinkMsg::ReceiveFinish { .. }
            | LinkMsg::Goodbye => self.start_flush(),
            _ => (),
        }
    }

    /// Flush the send buffer of the link.
    pub(crate) fn start_flush(&mut self) {
        self.txed_acks_unflushed = 0;
        self.tx_flushing = true;
        self.tx_polling = Some(Instant::now());
        self.tx_unflushed = None;
    }

    /// Whether flushing is required because of sent acks.
    pub(crate) fn need_ack_flush(&self) -> bool {
        self.txed_acks_unflushed != 0
    }

    /// Whether flushing is required.
    pub(crate) fn needs_flush(&self) -> bool {
        self.tx_unflushed.is_some() && !self.tx_flushing
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
        self.tx_unflushed = Some(Instant::now());
        self.tx_idle_since = None;

        let mut sent = 0;
        while sent < data_limit {
            match poll_fn(|cx| self.tx.poll_ready_unpin(cx)).now_or_never() {
                Some(Ok(())) => (),
                Some(Err(err)) => {
                    self.tx_error = Some(err);
                    break;
                }
                None => break,
            }

            let size = packet_size.min(data_limit - sent);
            if let Err(err) = self.tx.start_send_unpin(LinkMsg::TestData { size }.encode()) {
                self.tx_error = Some(err);
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

    /// Marks the send part of the link as idle.
    pub(crate) fn mark_idle(&mut self) {
        self.tx_idle_since = Some(Instant::now());
        self.stats.mark_idle();
    }

    /// Returns whether the link can be used for sending data.
    pub(crate) fn is_sendable(&self) -> bool {
        match &self.cfg.link_steering {
            LinkSteering::MinRoundtrip(_) => self.tx_fastest,
            LinkSteering::UnackedLimit(_) => self.txed_unacked_data < self.txed_unacked_data_limit,
        }
    }

    /// Time of sending last message that has not yet been flushed.
    pub(crate) fn tx_unflushed(&self) -> Option<Instant> {
        self.tx_unflushed
    }

    /// Since when transmitter is being polled for readyness.
    pub(crate) fn tx_polling(&self) -> Option<Instant> {
        self.tx_polling
    }

    /// Notify link for statistics that a reliable message has been sent.
    pub(crate) fn reliable_message_sent(&mut self, seq: Seq, sent: Instant) {
        self.txed_packets.push_back(LinkSentReliable { seq, sent, unacked_data: self.txed_unacked_data });
    }

    /// Notify link for statistics that a reliable message has been acknowledged.
    pub(crate) fn reliable_message_acked(&mut self, acked_seq: Seq) {
        loop {
            match self.txed_packets.front() {
                Some(LinkSentReliable { seq, sent, unacked_data }) if *seq <= acked_seq => {
                    if *seq == acked_seq {
                        let rt = sent.elapsed();
                        self.avg_roundtrip = (99 * self.avg_roundtrip + rt) / 100;
                        self.last_roundtrip = (rt, *unacked_data);
                    }

                    self.txed_packets.pop_front();
                }
                _ => break,
            }
        }
    }

    /// The expected duration for the acknowledgement to arrive if data of the
    /// specified size was sent over the link.
    pub(crate) fn expected_roundtrip(&self, data_size: usize) -> Duration {
        let now_ql: u32 = (self.txed_unacked_data + data_size).try_into().unwrap_or(u32::MAX);

        let (last_rt, last_ql) = self.last_roundtrip;
        let last_ql: u32 = last_ql.try_into().unwrap_or(u32::MAX).max(1);

        let adjusted_rt = last_rt * now_ql / last_ql;
        let mut rt = last_rt.max(adjusted_rt);

        if let Some(LinkSentReliable { sent, unacked_data: ql, .. }) = self.txed_packets.front() {
            let ql: u32 = (*ql).try_into().unwrap_or(u32::MAX).max(1);
            let elapsed_rt = sent.elapsed() * now_ql / ql;
            rt = rt.max(elapsed_rt);
        }

        rt
    }

    /// Reset statistics and limits after the link was reconfirmed.
    pub(crate) fn reset(&mut self) {
        // Reset unacked data limit.
        if let LinkSteering::UnackedLimit(UnackedLimit { init, .. }) = &self.cfg.link_steering {
            self.txed_unacked_data_limit = self.txed_unacked_data_limit.clamp(100, init.get());
            self.txed_unacked_data_limit_increased = None;
            self.txed_unacked_data_limit_increased_consecutively = 0;
        }

        // Reset sent packets.
        self.txed_packets.clear();
    }

    /// Publishes link statistics.
    pub(crate) fn publish_stats(&mut self) {
        self.stats.current.working = self.unconfirmed.is_none();
        self.stats.current.sent_unacked = self.txed_unacked_data as _;
        self.stats.current.unacked_limit = self.txed_unacked_data_limit as _;
        self.stats.current.roundtrip = self.avg_roundtrip;
        self.stats.current.expected_empty = self.expected_roundtrip(0);

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
            disconnected_rx: link_int.disconnected_tx.subscribe(),
            disconnect_tx: link_int.disconnect_tx.clone(),
            stats_rx: link_int.stats.subscribe(),
            remote_user_data: link_int.remote_user_data.clone(),
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
            working: false,
            total_sent: 0,
            total_recved: 0,
            sent_unacked: 0,
            unacked_limit: 0,
            roundtrip,
            expected_empty: roundtrip,
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
        let (last_roundtrip, last_roundtrip_unacked_data) = link.last_roundtrip;

        Self {
            present: true,
            link_id: link.link_id.0,
            unconfirmed: link.unconfirmed.is_some(),
            tx_flushing: link.tx_flushing,
            tx_flushed: link.tx_unflushed.is_none(),
            avg_roundtrip: link.avg_roundtrip.as_secs_f32(),
            expected_roundtrip: last_roundtrip.as_secs_f32(),
            expected_roundtrip_unacked_data: last_roundtrip_unacked_data,
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
