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
use tokio::io::{AsyncRead, AsyncWrite};
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
