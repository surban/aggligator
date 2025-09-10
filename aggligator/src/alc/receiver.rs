//! Receiver front-end of aggregated stream.

use bytes::Bytes;
use futures::{ready, Stream};
use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::{mpsc, watch},
};

use crate::id::ConnId;

/// Error receiving from an aggregated link channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecvError {
    /// All links have failed.
    AllLinksFailed,
    /// A protocol error occured on a link.
    ProtocolError,
    /// A link connected to another server than the other links.
    ServerIdMismatch,
    /// The connection was forcefully terminated.
    TaskTerminated,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::AllLinksFailed => write!(f, "all links failed"),
            Self::ProtocolError => write!(f, "protocol error"),
            Self::ServerIdMismatch => write!(f, "a new link connected to another server"),
            Self::TaskTerminated => write!(f, "connection forcefully terminated"),
        }
    }
}

impl std::error::Error for RecvError {}

impl From<RecvError> for io::Error {
    fn from(err: RecvError) -> Self {
        io::Error::new(io::ErrorKind::ConnectionAborted, err)
    }
}

/// The receiving half of an aggregated link channel.
pub struct Receiver {
    conn_id: ConnId,
    rx: mpsc::Receiver<Bytes>,
    closed_tx: mpsc::Sender<()>,
    error_rx: watch::Receiver<Option<RecvError>>,
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").field("conn_id", &self.conn_id).finish()
    }
}

impl Receiver {
    pub(crate) fn new(
        conn_id: ConnId, rx: mpsc::Receiver<Bytes>, closed_tx: mpsc::Sender<()>,
        error_rx: watch::Receiver<Option<RecvError>>,
    ) -> Self {
        Self { conn_id, rx, closed_tx, error_rx }
    }

    /// Connection id.
    pub fn id(&self) -> ConnId {
        self.conn_id
    }

    /// Receives the next data packet.
    #[inline]
    pub async fn recv(&mut self) -> Result<Option<Bytes>, RecvError> {
        match self.rx.recv().await {
            Some(data) => Ok(Some(data)),
            None => match self.error_rx.borrow().clone() {
                None => Ok(None),
                Some(err) => Err(err),
            },
        }
    }

    /// Polls to receive the next data packet.
    #[inline]
    pub fn poll_recv(&mut self, cx: &mut Context) -> Poll<Result<Option<Bytes>, RecvError>> {
        match ready!(self.rx.poll_recv(cx)) {
            Some(data) => Poll::Ready(Ok(Some(data))),
            None => match self.error_rx.borrow().clone() {
                None => Poll::Ready(Ok(None)),
                Some(err) => Poll::Ready(Err(err)),
            },
        }
    }

    /// Prevents the remote endpoint from sending further messages, but allows already
    /// sent messages to be processed.
    pub fn close(&mut self) {
        let _ = self.closed_tx.try_send(());
    }

    /// Converts this receiver into a [ReceiverStream], that implements the [Stream] and
    /// [AsyncRead] traits.
    pub fn into_stream(self) -> ReceiverStream {
        ReceiverStream { receiver: self, buf: Bytes::new() }
    }
}

/// The receiving stream of an aggregated link channel, implementing [Stream] and [AsyncRead].
///
/// This is called `ReadHalf` in Tokio.
pub struct ReceiverStream {
    receiver: Receiver,
    buf: Bytes,
}

impl fmt::Debug for ReceiverStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ReceiverStream").field("conn_id", &self.receiver.conn_id.0).finish()
    }
}

impl From<Receiver> for ReceiverStream {
    fn from(receiver: Receiver) -> Self {
        receiver.into_stream()
    }
}

impl ReceiverStream {
    /// Connection id.
    pub fn id(&self) -> ConnId {
        self.receiver.id()
    }

    /// Prevents the remote endpoint from sending further data, but allows already
    /// sent data to be processed.
    pub fn close(&mut self) {
        self.receiver.close()
    }
}

impl Stream for ReceiverStream {
    type Item = Result<Bytes, RecvError>;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        if !this.buf.is_empty() {
            return Poll::Ready(Some(Ok(this.buf.split_to(this.buf.len()))));
        }

        Poll::Ready(ready!(this.receiver.poll_recv(cx)).transpose())
    }
}

impl AsyncRead for ReceiverStream {
    #[inline]
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf) -> Poll<io::Result<()>> {
        let this = Pin::into_inner(self);

        if this.buf.is_empty() {
            if let Some(data) = ready!(this.receiver.poll_recv(cx))? {
                this.buf = data;
            }
        }

        let len = buf.remaining().min(this.buf.len());
        buf.put_slice(&this.buf.split_to(len));

        Poll::Ready(Ok(()))
    }
}
