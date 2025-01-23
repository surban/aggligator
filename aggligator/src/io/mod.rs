//! Wrapper types for stream-based links.
//!
//! These wrapper types turn stream-based links into packet-based links
//! by applying the [integrity codec](IntegrityCodec).
//!
//! They are applied by the [`Server::add_incoming_io`](crate::connect::Server::add_incoming_io)
//! and [`Control::add_io`](crate::control::Control::add_io) methods to stream-based links,
//! using the default configuration of the integrity codec.
//!

mod codec;

use bytes::Bytes;
use futures::{Sink, SinkExt, Stream, StreamExt};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::codec::{FramedRead, FramedWrite};

pub use codec::*;

/// Transmit wrapper for using an IO-stream-based link.
#[derive(Debug)]
pub struct IoTx<W>(pub FramedWrite<W, IntegrityCodec>);

impl<W> IoTx<W>
where
    W: AsyncWrite,
{
    /// Wraps an IO writer using the default configuration of the integrity codec.
    pub fn new(write: W) -> Self {
        Self(FramedWrite::new(write, IntegrityCodec::new()))
    }

    /// Wraps an IO writer using a customized integrity codec.
    pub fn with_codec(write: W, codec: IntegrityCodec) -> Self {
        Self(FramedWrite::new(write, codec))
    }
}

impl<W> Sink<Bytes> for IoTx<W>
where
    W: AsyncWrite + Unpin,
{
    type Error = io::Error;

    #[inline]
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).0.poll_ready_unpin(cx)
    }

    #[inline]
    fn start_send(self: Pin<&mut Self>, item: Bytes) -> Result<(), Self::Error> {
        Pin::into_inner(self).0.start_send_unpin(item)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).0.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::into_inner(self).0.poll_close_unpin(cx)
    }
}

/// Receive wrapper for using an IO-stream-based link.
#[derive(Debug)]
pub struct IoRx<R>(pub FramedRead<R, IntegrityCodec>);

impl<R> IoRx<R>
where
    R: AsyncRead,
{
    /// Wraps an IO reader using the default configuration of the integrity codec.
    pub fn new(read: R) -> Self {
        Self(FramedRead::new(read, IntegrityCodec::new()))
    }

    /// Wraps an IO reader using a customized integrity codec.
    pub fn with_codec(read: R, codec: IntegrityCodec) -> Self {
        Self(FramedRead::new(read, codec))
    }
}

impl<R> Stream for IoRx<R>
where
    R: AsyncRead + Unpin,
{
    type Item = Result<Bytes, io::Error>;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Pin::into_inner(self).0.poll_next_unpin(cx).map_ok(|v| v.freeze())
    }
}

/// Type-neutral transmit wrapper for using an IO-stream-based link.
///
/// Useful if a connection consists of different types of links.
pub type IoTxBox = IoTx<Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>>;

/// Type-neutral receive wrapper for using an IO-stream-based link.
///
/// Useful if a connection consists of different types of links.
pub type IoRxBox = IoRx<Pin<Box<dyn AsyncRead + Send + Sync + 'static>>>;

/// A stream, either packet-based or IO-based.
pub enum StreamBox {
    /// Packet-based stream.
    TxRx(TxRxBox),
    /// IO-based stream.
    Io(IoBox),
}

impl StreamBox {
    /// Make stream packet-based.
    ///
    /// A packet-based stream is unaffacted.
    /// An IO-based stream is wrapped in the integrity codec.
    pub fn into_tx_rx(self) -> TxRxBox {
        match self {
            Self::TxRx(tx_rx) => tx_rx,
            Self::Io(IoBox { read, write }) => {
                let tx = IoTxBox::new(write);
                let rx = IoRxBox::new(read);
                TxRxBox::new(tx, rx)
            }
        }
    }
}

impl From<TxRxBox> for StreamBox {
    fn from(value: TxRxBox) -> Self {
        Self::TxRx(value)
    }
}

impl From<IoBox> for StreamBox {
    fn from(value: IoBox) -> Self {
        Self::Io(value)
    }
}

pub(crate) type TxBox = Pin<Box<dyn Sink<Bytes, Error = io::Error> + Send + Sync + 'static>>;
pub(crate) type RxBox = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send + Sync + 'static>>;

/// A boxed packet-based stream.
pub struct TxRxBox {
    /// Sender.
    pub tx: TxBox,
    /// Receiver.
    pub rx: RxBox,
}

impl TxRxBox {
    /// Creates a new instance.
    pub fn new(
        tx: impl Sink<Bytes, Error = io::Error> + Send + Sync + 'static,
        rx: impl Stream<Item = io::Result<Bytes>> + Send + Sync + 'static,
    ) -> Self {
        Self { tx: Box::pin(tx), rx: Box::pin(rx) }
    }

    /// Splits this into boxed transmitter and receiver.
    pub fn into_split(self) -> (TxBox, RxBox) {
        let Self { tx, rx } = self;
        (tx, rx)
    }
}

impl Sink<Bytes> for TxRxBox {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.get_mut().tx.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Bytes) -> io::Result<()> {
        self.get_mut().tx.start_send_unpin(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.get_mut().tx.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.get_mut().tx.poll_close_unpin(cx)
    }
}

impl Stream for TxRxBox {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.get_mut().rx.poll_next_unpin(cx)
    }
}

pub(crate) type ReadBox = Pin<Box<dyn AsyncRead + Send + Sync + 'static>>;
pub(crate) type WriteBox = Pin<Box<dyn AsyncWrite + Send + Sync + 'static>>;

/// A boxed IO stream.
pub struct IoBox {
    /// Reader.
    pub read: ReadBox,
    /// Writer.
    pub write: WriteBox,
}

impl IoBox {
    /// Creates a new instance.
    pub fn new(
        read: impl AsyncRead + Send + Sync + 'static, write: impl AsyncWrite + Send + Sync + 'static,
    ) -> Self {
        Self { read: Box::pin(read), write: Box::pin(write) }
    }

    /// Splits this into boxed reader and writer.
    pub fn into_split(self) -> (ReadBox, WriteBox) {
        let Self { read, write } = self;
        (read, write)
    }
}

impl AsyncRead for IoBox {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().read).poll_read(cx, buf)
    }
}

impl AsyncWrite for IoBox {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().write).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().write).poll_shutdown(cx)
    }
}
