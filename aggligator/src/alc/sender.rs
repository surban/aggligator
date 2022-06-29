//! Sender front-end of aggregated stream.

use bytes::Bytes;
use futures::{ready, FutureExt, Sink, SinkExt};
use std::{
    fmt, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::AsyncWrite,
    sync::{mpsc, oneshot, watch},
};
use tokio_util::sync;

use crate::{
    agg::task::SendReq,
    cfg::{Cfg, ExchangedCfg},
    id::ConnId,
};

/// Error sending to an aggregated link channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The remote endpoint closed the connection.
    Closed,
    /// The remote endpoint dropped the receiver.
    Dropped,
    /// The connection was shutdown by this endpoint.
    Shutdown,
    /// Data length exceeds receive buffer of remote endpoint.
    DataTooBig,
    /// All links have failed.
    AllLinksFailed,
    /// The connection task was terminated.
    TaskTerminated,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "closed by remote endpoint"),
            Self::Dropped => write!(f, "dropped by remote endpoint"),
            Self::Shutdown => write!(f, "connection was shut down or closed locally"),
            Self::DataTooBig => write!(f, "data too big for remote endpoint"),
            Self::AllLinksFailed => write!(f, "all links failed"),
            Self::TaskTerminated => write!(f, "task terminated"),
        }
    }
}

impl std::error::Error for SendError {}

impl From<SendError> for io::Error {
    fn from(err: SendError) -> Self {
        let kind = match &err {
            SendError::Closed | SendError::Dropped => io::ErrorKind::ConnectionReset,
            SendError::Shutdown => io::ErrorKind::BrokenPipe,
            SendError::DataTooBig => io::ErrorKind::InvalidData,
            SendError::AllLinksFailed | SendError::TaskTerminated => io::ErrorKind::ConnectionAborted,
        };
        io::Error::new(kind, err)
    }
}

fn max_send_size(remote_cfg: &ExchangedCfg) -> usize {
    (remote_cfg.recv_buffer.get() as usize / 2).max(2) - 1
}

/// The sending half of an aggregated link channel.
pub struct Sender {
    cfg: Arc<Cfg>,
    remote_cfg: Arc<ExchangedCfg>,
    conn_id: ConnId,
    tx: mpsc::Sender<SendReq>,
    error_rx: watch::Receiver<SendError>,
}

impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").field("id", &self.conn_id).finish()
    }
}

impl Sender {
    pub(crate) fn new(
        cfg: Arc<Cfg>, remote_cfg: Arc<ExchangedCfg>, conn_id: ConnId, tx: mpsc::Sender<SendReq>,
        error_rx: watch::Receiver<SendError>,
    ) -> Self {
        Self { cfg, remote_cfg, conn_id, tx, error_rx }
    }

    /// Connection id.
    pub fn id(&self) -> ConnId {
        self.conn_id
    }

    /// Enqueues data for sending.
    #[inline]
    pub async fn send(&self, data: Bytes) -> Result<(), SendError> {
        if data.len() > self.max_size() {
            return Err(SendError::DataTooBig);
        }

        self.tx.send(SendReq::Send(data)).await.map_err(|_| self.error_rx.borrow().clone())
    }

    /// Flushes data queued for sending.
    #[inline]
    pub async fn flush(&self) -> Result<(), SendError> {
        let (flushed_tx, flushed_rx) = oneshot::channel();
        self.tx.send(SendReq::Flush(flushed_tx)).await.map_err(|_| self.error_rx.borrow().clone())?;
        flushed_rx.await.map_err(|_| self.error_rx.borrow().clone())?;
        Ok(())
    }

    /// Maximum data size.
    pub fn max_size(&self) -> usize {
        max_send_size(&self.remote_cfg)
    }

    /// Converts this sender into a [SenderSink], that implements the [Sink] and [AsyncWrite] traits.
    pub fn into_sink(self) -> SenderSink {
        let Self { cfg, remote_cfg, conn_id, tx, error_rx } = self;
        SenderSink {
            cfg,
            remote_cfg,
            conn_id,
            tx: sync::PollSender::new(tx),
            flushed_rx: None,
            error_rx,
            closed: false,
        }
    }
}

/// The sending sink of an aggregated link channel, implementing [Sink] and [AsyncWrite].
///
/// This is called `WriteHalf` in Tokio.
pub struct SenderSink {
    cfg: Arc<Cfg>,
    remote_cfg: Arc<ExchangedCfg>,
    conn_id: ConnId,
    tx: sync::PollSender<SendReq>,
    flushed_rx: Option<oneshot::Receiver<()>>,
    error_rx: watch::Receiver<SendError>,
    closed: bool,
}

impl fmt::Debug for SenderSink {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SenderSink").field("id", &self.conn_id.0).field("closed", &self.closed).finish()
    }
}

impl From<Sender> for SenderSink {
    fn from(sender: Sender) -> Self {
        sender.into_sink()
    }
}

impl SenderSink {
    /// Connection id.
    pub fn id(&self) -> ConnId {
        self.conn_id
    }

    /// Maximum data size.
    pub fn max_size(&self) -> usize {
        max_send_size(&self.remote_cfg)
    }
}

impl Sink<Bytes> for SenderSink {
    type Error = SendError;

    #[inline]
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);

        if this.closed {
            return Poll::Ready(Err(SendError::Shutdown));
        }

        this.tx.poll_ready_unpin(cx).map_err(|_| this.error_rx.borrow().clone())
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        let this = Pin::into_inner(self);

        if this.closed {
            return Err(SendError::Shutdown);
        }

        if item.len() > this.max_size() {
            return Err(SendError::DataTooBig);
        }

        this.tx.start_send_unpin(SendReq::Send(item)).map_err(|_| this.error_rx.borrow().clone())
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);

        if this.closed {
            return Poll::Ready(Ok(()));
        }

        if this.flushed_rx.is_none() {
            ready!(this.tx.poll_ready_unpin(cx)).map_err(|_| this.error_rx.borrow().clone())?;

            let (flushed_tx, flushed_rx) = oneshot::channel();
            this.tx.start_send_unpin(SendReq::Flush(flushed_tx)).map_err(|_| this.error_rx.borrow().clone())?;
            this.flushed_rx = Some(flushed_rx);
        }

        let flushed_rx = this.flushed_rx.as_mut().unwrap();
        ready!(flushed_rx.poll_unpin(cx)).map_err(|_| this.error_rx.borrow().clone())?;
        this.flushed_rx = None;

        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);

        if this.closed {
            return Poll::Ready(Ok(()));
        }

        ready!(this.poll_flush_unpin(cx))?;
        ready!(this.tx.poll_close_unpin(cx)).unwrap();
        this.closed = true;

        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SenderSink {
    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        let this = Pin::into_inner(self);

        ready!(this.poll_ready_unpin(cx))?;

        let max_packet_size = this.cfg.io_write_size.get().min(this.remote_cfg.recv_buffer.get() as usize);
        let len = buf.len().min(max_packet_size);
        let data = Bytes::copy_from_slice(&buf[..len]);
        this.start_send_unpin(data)?;

        Poll::Ready(Ok(len))
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Pin::into_inner(self).poll_flush_unpin(cx).map_err(|err| err.into())
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Pin::into_inner(self).poll_close_unpin(cx).map_err(|err| err.into())
    }
}
