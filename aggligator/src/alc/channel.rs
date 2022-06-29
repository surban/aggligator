//! Front-end of aggregated connection.

use bytes::Bytes;
use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{mpsc, watch},
};

use super::{Receiver, ReceiverStream, RecvError, SendError, Sender, SenderSink};
use crate::{
    agg::task::SendReq,
    cfg::{Cfg, ExchangedCfg},
    id::ConnId,
};

/// A bi-directional channel backed by a connection of aggregated links.
///
/// Use the [connect module](crate::connect) to establish a new connection.
///
/// This must either be converted into [sender and receiver for messages](Self::into_tx_rx)
/// or into a [stream supporting async IO](Self::into_stream).
#[derive(Debug)]
pub struct Channel {
    cfg: Arc<Cfg>,
    remote_cfg: Option<Arc<ExchangedCfg>>,
    conn_id: ConnId,
    tx: mpsc::Sender<SendReq>,
    tx_error: watch::Receiver<SendError>,
    rx: mpsc::Receiver<Bytes>,
    rx_closed: mpsc::Sender<()>,
    rx_error: watch::Receiver<Option<RecvError>>,
}

impl Channel {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cfg: Arc<Cfg>, remote_cfg: Option<Arc<ExchangedCfg>>, conn_id: ConnId, tx: mpsc::Sender<SendReq>,
        tx_error: watch::Receiver<SendError>, rx: mpsc::Receiver<Bytes>, rx_closed: mpsc::Sender<()>,
        rx_error: watch::Receiver<Option<RecvError>>,
    ) -> Self {
        Self { cfg, remote_cfg, conn_id, tx, tx_error, rx, rx_closed, rx_error }
    }

    /// Connection id.
    pub fn id(&self) -> ConnId {
        self.conn_id
    }

    /// Sets the remote configuration of the connection.
    pub(crate) fn set_remote_cfg(&mut self, remote_cfg: Arc<ExchangedCfg>) {
        assert!(self.remote_cfg.is_none(), "remote configuration was already set");
        self.remote_cfg = Some(remote_cfg);
    }

    /// Splits this into sender and receiver for messages.
    ///
    /// Note that the local sender is connected to the receiver *of the remote endpoint* and vice versa.
    pub fn into_tx_rx(self) -> (Sender, Receiver) {
        let Self { cfg, remote_cfg, conn_id, tx, tx_error, rx, rx_closed, rx_error } = self;

        let tx = Sender::new(cfg, remote_cfg.unwrap(), conn_id, tx, tx_error);
        let rx = Receiver::new(conn_id, rx, rx_closed, rx_error);

        (tx, rx)
    }

    /// Converts this into a stream that implements the [`AsyncRead`] and [`AsyncWrite`] traits.
    pub fn into_stream(self) -> Stream {
        let (tx, rx) = self.into_tx_rx();
        Stream { tx: tx.into_sink(), rx: rx.into_stream() }
    }
}

/// A bi-directional IO stream backed by a connection of aggregated links,
/// implementing [AsyncRead] and [AsyncWrite].
#[derive(Debug)]
pub struct Stream {
    tx: SenderSink,
    rx: ReceiverStream,
}

impl Stream {
    /// Connection id.
    pub fn id(&self) -> ConnId {
        self.tx.id()
    }

    /// Splits this stream into its receiving and sending halves.
    pub fn into_split(self) -> (ReceiverStream, SenderSink) {
        let Self { tx, rx } = self;
        (rx, tx)
    }

    /// Prevents the remote endpoint from sending further data, but allows already
    /// sent data to be processed.
    pub fn close(&mut self) {
        self.rx.close()
    }
}

impl AsyncRead for Stream {
    #[inline]
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut ReadBuf) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().rx).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    #[inline]
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.get_mut().tx).poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().tx).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.get_mut().tx).poll_shutdown(cx)
    }
}
