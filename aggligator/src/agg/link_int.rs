//! Internal link data.

use bytes::Bytes;
use futures::{future, future::poll_fn, FutureExt, Sink, SinkExt, Stream, StreamExt};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{
    collections::VecDeque,
    fmt,
    fs::File,
    io,
    io::{BufWriter, Write},
    mem,
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::{
    select,
    sync::{mpsc, watch},
    time::{sleep_until, Instant},
};

use crate::cfg::MinRoundtrip;
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
    /// Time packet was sent since connection was established.
    sent: Duration,
    size: usize,
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
    /// Trip time to remote endpoint per byte.
    pub(crate) tx_trip_per_byte: f32,
    /// Whether this is the fastest link for sending.
    pub(crate) tx_fastest: bool,
    pub(crate) tx_dummy: bool,
    pub(crate) txed_unacked_data_all: usize,
    tx_trip_estimates: usize,
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
    /// Milliseconds to add to clock of remote endpoint.
    clock_offset: Arc<AtomicU32>,
    /// Link statistics calculator.
    stats: LinkStatistican,
    resets: usize,
    log_file: BufWriter<File>,
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

        let link_id = LinkId::generate();

        Self {
            tag: Arc::new(tag),
            conn_id,
            link_id,
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
            tx_trip_per_byte: 1.,
            tx_trip_estimates: 0,
            tx_fastest: false,
            tx_dummy: false,
            txed_unacked_data_all: 0,
            last_ping: None,
            current_ping_sent: None,
            send_ping: None,
            send_pong: false,
            avg_roundtrip: roundtrip,
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
            clock_offset: Arc::new(AtomicU32::default()),
            remote_user_data: Arc::new(remote_user_data),
            resets: 0,
            log_file: BufWriter::new(File::create(format!("linklog-{conn_id}-{link_id}.csv")).unwrap()),
        }
    }

    /// Sets the shared clock offset reference.
    pub(crate) fn set_clock_offset(&mut self, clock_offset: Arc<AtomicU32>) {
        self.clock_offset = clock_offset;
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
                        self.stats.record(0, buf.len(), false);

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
    pub(crate) fn start_send_msg(&mut self, msg: LinkMsg, data: Option<Bytes>, dummy: bool) {
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

        self.stats.record(msg_len + data_len, 0, dummy);

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
    pub(crate) fn reliable_message_sent(&mut self, seq: Seq, size: usize, sent: Duration) {
        self.txed_unacked_data_all += size;
        self.txed_packets.push_back(LinkSentReliable {
            seq,
            sent,
            size,
            unacked_data: self.txed_unacked_data_all,
        });
    }

    // For the unacked data we don't actually know the true value.
    // It may be unacked because the endpoint didn't receive it yet.
    // Or because it sent the ack but we haven't received the ack yet.
    // This gives lots of variation.
    //
    // Thus we would need to know how much of the data was not cleared when sending the message.
    // Actually we cannot know it as this point but we should be able to calculate from ACK?
    // If we get ACK and ack has sent time before our sent time, then we know it was already acked actually.

    // Okay what do we need to do to get that working robustly?
    // There are two problems:
    // 1. a link may massively overestimate its speed
    // 2. a link may masssively underestimate its speed
    //
    // In 1 it will time out
    // In 2 it will never be used.
    //
    // We could first improve the initial estimate of the link capacity
    // when confirming it. Then in case 1 the link should fix itself.
    //
    // In case 2 we could also trigger unconfirming the link, but what
    // criteria to use?
    // Not used for so many packets?
    // I.e. constant times number of links?
    // Could be a possibility.
    // Time-based is bad because there might be no traffic.
    //
    // And what can we do against a link overestimating its speed?
    // Well, this would be similar to slow buffer increase.
    // We could limit the maximum increase from last send buffer size.
    //
    // But how to make sure that it does fall again when not used
    // for longer period of time?
    // Well maybe do averaging then?
    // Could be an idea.
    //
    // Okay, what is first step?
    // Correctly calculate m from link test.

    //
    // Still two problems:
    //
    // 1. links that are thought slow will never be used
    //    => they need to send test data?
    //    => hmm how difficult is it to send non-useful data?
    //    => maybe just duplicate a packet, becaue double-ack doesn't matter but should be received by the link
    //    => could be a good idea to give some traffic to the link.
    //
    // 2. packets can be sent over a link even if it is predicted that maximum trip time will be exceeded
    //
    // 3. we should use estimated trip time for unconfirmed timeout

    /// Notify link for statistics that a reliable message has been acknowledged.
    pub(crate) fn reliable_message_acked(&mut self, acked_seq: Seq, recved: Duration, remote_recved: Duration) {
        const S: usize = 200;
        const D: usize = 100;

        //let mut ack_size = None;

        // Remove acked packets.
        loop {
            match self.txed_packets.front() {
                Some(LinkSentReliable { seq, sent, size, unacked_data }) if *seq <= acked_seq => {
                    self.txed_unacked_data_all =
                        self.txed_unacked_data_all.checked_sub(*size).expect("unacked data underflow");

                    // Estimate trip and roundtrip times.
                    if *seq == acked_seq {
                        let data = *unacked_data;
                        //ack_size = Some(*size);

                        let roundtrip = recved - *sent;
                        let s = S as u32;
                        self.avg_roundtrip = (s * self.avg_roundtrip + roundtrip) / (s + 1);

                        if let LinkSteering::MinRoundtrip(_) = &self.cfg.link_steering {
                            let data = (data * 12 / 10).max(self.cfg.io_write_size.get() * 5);
                            if data >= self.txed_unacked_data_limit {
                                self.txed_unacked_data_limit = data;
                            } else {
                                self.txed_unacked_data_limit = self.txed_unacked_data_limit * (D - 1) / D;
                            }
                        }

                        if data >= 128 {
                            let data = data as f32;
                            let clock_offset = self.clock_offset.load(Ordering::Relaxed);
                            let sent = sent.as_secs_f32();
                            let remote_recved = remote_recved.as_secs_f32() + clock_offset as f32 / 1000.;

                            let trip = if sent <= remote_recved {
                                remote_recved - sent
                            } else {
                                let new_offset = clock_offset + ((sent - remote_recved) * 1000.) as u32;
                                self.clock_offset.store(new_offset, Ordering::Relaxed);
                                0.
                            };

                            // Direct estimation:
                            let est = trip / *unacked_data as f32;
                            let s = S as f32;
                            self.tx_trip_per_byte = (s * self.tx_trip_per_byte + est) / (s + 1.);
                            self.tx_trip_estimates += 1;

                            writeln!(&mut self.log_file, "{data};{trip}").unwrap();
                        }
                    }

                    self.txed_packets.pop_front();
                }
                _ => break,
            }
        }
        //
        //         // Adjust unacked size of packets sent before receiving ack that was in flight.
        //         if let Some(ack_size) = ack_size {
        //             for LinkSentReliable { sent, unacked_data, .. } in &mut self.txed_packets {
        //                 if *sent >= remote_recved {
        //                     *unacked_data = unacked_data.saturating_sub(ack_size);
        //                 }
        //             }
        //         }
    }

    /// The expected duration in ms for the message to arrive at the remote endpoint
    /// if data of the specified size was sent over the link now.
    pub(crate) fn expected_trip(&self, data_size: usize) -> f32 {
        let total = (self.txed_unacked_data + data_size) as f32;
        let mut t = self.tx_trip_per_byte * total;

        // Penalize estimates outside of measured domain.
        if total > self.txed_unacked_data_limit as f32 {
            t *= (total / self.txed_unacked_data_limit.max(1) as f32).powi(2);
        }

        t
    }

    /// Reset statistics and limits when the link is unconfirmed.
    pub(crate) fn reset(&mut self) {
        match &self.cfg.link_steering {
            LinkSteering::MinRoundtrip(MinRoundtrip { .. }) => {
                // Reset trip estimates.
                self.tx_trip_estimates = 0;
                self.tx_trip_per_byte = 1.;
                self.txed_unacked_data_limit = 0;
            }
            LinkSteering::UnackedLimit(UnackedLimit { init, .. }) => {
                // Reset unacked data limit.
                self.txed_unacked_data_limit = self.txed_unacked_data_limit.clamp(100, init.get());
                self.txed_unacked_data_limit_increased = None;
                self.txed_unacked_data_limit_increased_consecutively = 0;
            }
        }

        self.resets += 1;

        // Reset sent packets.
        //self.txed_packets.clear();
    }

    /// Provide the link with the result of a ping.
    pub(crate) fn ping_result(&mut self, roundtrip: Duration, data_size: usize) {
        self.avg_roundtrip = roundtrip;

        let trip = roundtrip.as_secs_f32() / 2.;
        self.txed_unacked_data_limit = self.txed_unacked_data_limit.max(data_size);

        if data_size > 128 {
            self.tx_trip_per_byte = 5. * trip / data_size as f32;
        } else {
            self.tx_trip_per_byte = 1.;
        }

        self.last_ping = Some(Instant::now());
    }

    /// Publishes link statistics.
    pub(crate) fn publish_stats(&mut self) {
        self.stats.current.working = self.unconfirmed.is_none();
        self.stats.current.sent_unacked = self.txed_unacked_data as _;
        self.stats.current.sent_unacked_dummy =
            (self.txed_unacked_data_all as u64).saturating_sub(self.stats.current.sent_unacked);
        self.stats.current.unacked_limit = self.txed_unacked_data_limit as _;
        self.stats.current.roundtrip = self.avg_roundtrip;
        self.stats.current.expected_empty = self.expected_trip(0);
        self.stats.current.bandwidth = 1. / self.tx_trip_per_byte;
        self.stats.current.estimates = self.tx_trip_estimates;
        self.stats.current.resets = self.resets;

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
        let trip = roundtrip.as_secs_f32() / 2.;

        let current = LinkStats {
            established: Instant::now(),
            working: false,
            total_sent: 0,
            total_sent_dummy: 0,
            total_recved: 0,
            sent_unacked: 0,
            sent_unacked_dummy: 0,
            unacked_limit: 0,
            roundtrip,
            expected_empty: trip,
            bandwidth: 0.,
            estimates: 0,
            resets: 0,
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
    fn record(&mut self, sent: usize, received: usize, dummy: bool) {
        if dummy {
            self.current.total_sent_dummy = self.current.total_sent_dummy.wrapping_add(sent as _);
        } else {
            self.current.total_sent = self.current.total_sent.wrapping_add(sent as _);
        }

        self.current.total_recved = self.current.total_recved.wrapping_add(received as _);

        for ts in &mut self.running_stats {
            if dummy {
                ts.sent_dummy = ts.sent_dummy.wrapping_add(sent as _);
            } else {
                ts.sent = ts.sent.wrapping_add(sent as _);
            }

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
        //let (last_roundtrip, last_roundtrip_unacked_data, _last_delay) = link.last_trip;

        Self {
            present: true,
            link_id: link.link_id.0,
            unconfirmed: link.unconfirmed.is_some(),
            tx_flushing: link.tx_flushing,
            tx_flushed: link.tx_unflushed.is_none(),
            avg_roundtrip: link.avg_roundtrip.as_secs_f32(),
            //expected_roundtrip: last_roundtrip.as_secs_f32(),
            //expected_roundtrip_unacked_data: last_roundtrip_unacked_data,
            expected_roundtrip: 0.,
            expected_roundtrip_unacked_data: 0,
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
