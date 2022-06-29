//! Test channel.
#![allow(dead_code)]

use bytes::Bytes;
use futures::{future, ready, Sink, SinkExt, Stream, StreamExt};
use std::{
    io::{Error, ErrorKind},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, Semaphore},
    time::{sleep, sleep_until, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::{PollSemaphore, PollSender};

/// Test channel configuration.
#[derive(Clone, Debug)]
pub struct Cfg {
    /// Speed in bytes per second.
    ///
    /// Zero for no throtteling.
    pub speed: usize,
    /// Maximum buffer size in items.
    pub buffer_items: usize,
    /// Maximum buffer size in bytes.
    pub buffer_size: usize,
    /// Latency.
    pub latency: Option<Duration>,
}

impl Default for Cfg {
    fn default() -> Self {
        Self { speed: 0, buffer_items: 128, buffer_size: 16384, latency: None }
    }
}

struct Packet {
    data: Bytes,
    sent: Instant,
}

enum ControlReq {
    PauseFor(Duration),
    PauseThenDisconnect(Duration),
    SetLatency(Option<Duration>),
    SetSpeed(usize),
    Disconnect,
}

struct ControlMsg {
    req: ControlReq,
    processed_tx: oneshot::Sender<()>,
}

/// Creates a new test channel using the provided configuration.
pub fn channel(mut cfg: Cfg) -> (Sender, Receiver, Control) {
    let sender_items = (cfg.buffer_items / 2).max(1);
    let receiver_items = (cfg.buffer_items - sender_items).max(1);
    let (sender_tx, mut sender_rx) = mpsc::channel(sender_items);
    let (receiver_tx, receiver_rx) = mpsc::channel(receiver_items);

    let buffer_size = Arc::new(AtomicUsize::new(0));
    let buffer_consumed = Arc::new(Semaphore::new(0));

    let disconnected = Arc::new(AtomicBool::new(false));

    let sender = Sender {
        cfg: cfg.clone(),
        tx: PollSender::new(sender_tx),
        buffer_size: buffer_size.clone(),
        buffer_consumed: PollSemaphore::new(buffer_consumed.clone()),
        not_ready_since: None,
        last_check: Instant::now(),
    };

    let receiver = Receiver {
        rx: ReceiverStream::new(receiver_rx),
        buffer_size,
        buffer_consumed,
        disconnected: disconnected.clone(),
    };

    let (control_tx, control_rx) = mpsc::channel(1);
    let control = Control { tx: control_tx };

    tokio::spawn(async move {
        let mut control_rx_opt = Some(control_rx);
        let mut sleep_need = Duration::ZERO;
        loop {
            tokio::select! {
                packet_opt = sender_rx.recv() => {
                    let Some(packet) = packet_opt else { break };

                    if let Some(latency) = cfg.latency {
                        let until = packet.sent + latency;
                        if until > Instant::now() {
                            // println!("latency wait");
                            sleep_until(until).await;
                        }
                    }

                    if cfg.speed > 0 {
                        sleep_need += Duration::from_secs_f64(packet.data.len() as f64 / cfg.speed as f64);
                        if sleep_need >= Duration::from_millis(100) {
                            sleep(sleep_need).await;
                            sleep_need = Duration::ZERO;
                        }
                    }

                    if receiver_tx.send(packet).await.is_err() {
                        break;
                    }
                }
                msg_opt = async {
                    match control_rx_opt.as_mut() {
                        Some(control_rx) => control_rx.recv().await,
                        None => future::pending().await,
                    }
                } => {
                    match msg_opt {
                        Some(ControlMsg {req, processed_tx}) => {
                            match req {
                                ControlReq::PauseFor (dur) => sleep(dur).await,
                                ControlReq::PauseThenDisconnect (dur) => {
                                    sleep(dur).await;
                                    disconnected.store(true, Ordering::SeqCst);
                                    break;
                                }
                                ControlReq::SetLatency (latency) => cfg.latency = latency,
                                ControlReq::SetSpeed (speed) => cfg.speed = speed,
                                ControlReq::Disconnect => {
                                    disconnected.store(true, Ordering::SeqCst);
                                    break;
                                }
                            }
                            let _ = processed_tx.send(());
                        },
                        None => control_rx_opt = None,
                    }
                }
            }
        }
    });

    (sender, receiver, control)
}

/// Controls the test channel.
#[derive(Clone)]
pub struct Control {
    tx: mpsc::Sender<ControlMsg>,
}

impl Control {
    async fn send_req(&self, req: ControlReq) -> Result<(), Error> {
        let (processed_tx, processed_rx) = oneshot::channel();
        self.tx.send(ControlMsg { req, processed_tx }).await.map_err(|_| ErrorKind::BrokenPipe)?;
        let _ = processed_rx.await;
        Ok(())
    }

    /// Pauses the channel for the specified amount of time.
    pub async fn pause_for(&self, duration: Duration) -> Result<(), Error> {
        self.send_req(ControlReq::PauseFor(duration)).await
    }

    /// Pauses the channel and then disconnects it.
    pub async fn pause_then_disconnected(self, duration: Duration) -> Result<(), Error> {
        self.send_req(ControlReq::PauseThenDisconnect(duration)).await
    }

    /// Sets the latency.
    pub async fn set_latency(&self, latency: Option<Duration>) -> Result<(), Error> {
        self.send_req(ControlReq::SetLatency(latency)).await
    }

    /// Sets the speed.
    pub async fn set_speed(&self, speed: usize) -> Result<(), Error> {
        self.send_req(ControlReq::SetSpeed(speed)).await
    }

    /// Disconnects the channel.
    pub async fn disconnect(self) -> Result<(), Error> {
        self.send_req(ControlReq::Disconnect).await
    }
}

/// Sending half of test channel.
pub struct Sender {
    cfg: Cfg,
    tx: PollSender<Packet>,
    buffer_size: Arc<AtomicUsize>,
    buffer_consumed: PollSemaphore,
    not_ready_since: Option<Instant>,
    last_check: Instant,
}

impl Sink<Bytes> for Sender {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);

        while this.buffer_size.load(Ordering::SeqCst) >= this.cfg.buffer_size {
            match ready!(this.buffer_consumed.poll_acquire(cx)) {
                Some(permit) => permit.forget(),
                None => return Poll::Ready(Err(ErrorKind::BrokenPipe.into())),
            }
        }

        match this.tx.poll_ready_unpin(cx).map_err(|_| ErrorKind::BrokenPipe)? {
            Poll::Ready(()) => {
                if let Some(not_ready_since) = this.not_ready_since.take() {
                    let elapsed = not_ready_since.elapsed().as_secs_f64();
                    if elapsed >= 0.1 {
                        println!(
                            "********* test channel was blocked for {:.2} s and last tried {:.2} s ago",
                            elapsed,
                            this.last_check.elapsed().as_secs_f64()
                        );
                    }
                }
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                if this.not_ready_since.is_none() {
                    //println!("test channel NOT ready for sending");
                    this.not_ready_since = Some(Instant::now());
                }
                this.last_check = Instant::now();
                Poll::Pending
            }
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        let this = Pin::into_inner(self);

        let size = item.len();

        this.tx
            .start_send_unpin(Packet { data: item, sent: Instant::now() })
            .map_err(|_| ErrorKind::BrokenPipe)?;

        this.buffer_size.fetch_add(size, Ordering::SeqCst);

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);

        ready!(this.tx.poll_flush_unpin(cx)).map_err(|_| ErrorKind::BrokenPipe)?;

        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);

        ready!(this.tx.poll_close_unpin(cx)).map_err(|_| ErrorKind::BrokenPipe)?;

        Poll::Ready(Ok(()))
    }
}

/// Receiving half of test channel.
pub struct Receiver {
    rx: ReceiverStream<Packet>,
    buffer_size: Arc<AtomicUsize>,
    buffer_consumed: Arc<Semaphore>,
    disconnected: Arc<AtomicBool>,
}

impl Stream for Receiver {
    type Item = Result<Bytes, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        let packet = match ready!(this.rx.poll_next_unpin(cx)) {
            Some(packet) => packet,
            None => {
                if this.disconnected.load(Ordering::SeqCst) {
                    return Poll::Ready(Some(Err(ErrorKind::BrokenPipe.into())));
                } else {
                    return Poll::Ready(None);
                }
            }
        };

        this.buffer_size.fetch_sub(packet.data.len(), Ordering::SeqCst);
        if this.buffer_consumed.available_permits() == 0 {
            this.buffer_consumed.add_permits(1);
        }

        Poll::Ready(Some(Ok(packet.data)))
    }
}
