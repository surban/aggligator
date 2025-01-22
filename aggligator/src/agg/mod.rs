//! Link aggregation implementation.

use bytes::Bytes;
use futures::{Sink, Stream};
use std::{
    io,
    sync::{atomic::AtomicBool, Arc},
};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::{
    agg::{link_int::LinkInt, task::Task},
    alc::{Channel, RecvError, SendError},
    cfg::{Cfg, ExchangedCfg},
    control::{Control, Direction, Link},
    id::{OwnedConnId, ServerId},
    TaskError,
};

#[cfg(feature = "dump")]
#[cfg_attr(docsrs, doc(cfg(feature = "dump")))]
pub mod dump;

pub(crate) mod link_int;
pub(crate) mod task;

/// Link aggregator parts.
pub(crate) struct AggParts<TX, RX, TAG> {
    pub task: Task<TX, RX, TAG>,
    pub channel: Channel,
    pub control: Control<TX, RX, TAG>,
    pub connected_rx: oneshot::Receiver<Arc<ExchangedCfg>>,
}

impl<TX, RX, TAG> AggParts<TX, RX, TAG>
where
    RX: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
    TX: Sink<Bytes, Error = io::Error> + Unpin + Send + 'static,
    TAG: Send + Sync + 'static,
{
    /// Creates a new aggregated connection and returns its parts.
    #[allow(clippy::type_complexity)]
    pub(crate) fn new(
        cfg: Arc<Cfg>, conn_id: OwnedConnId, direction: Direction, server_id: Option<ServerId>,
        remote_server_id: Option<ServerId>, links: Vec<LinkInt<TX, RX, TAG>>,
        link_tx_rx: Option<(mpsc::Sender<LinkInt<TX, RX, TAG>>, mpsc::Receiver<LinkInt<TX, RX, TAG>>)>,
    ) -> Self {
        let (read_tx, read_rx) = mpsc::channel(cfg.recv_queue.get());
        let (write_tx, write_rx) = mpsc::channel(cfg.send_queue.get());
        let (read_error_tx, read_error_rx) = watch::channel(Some(RecvError::TaskTerminated));
        let (write_error_tx, write_error_rx) = watch::channel(SendError::TaskTerminated);
        let (read_closed_tx, read_closed_rx) = mpsc::channel(1);
        let (links_tx, links_rx) = watch::channel(links.iter().map(Link::from).collect());
        let (link_tx, link_rx) = link_tx_rx.unwrap_or_else(|| mpsc::channel(cfg.connect_queue.get()));
        let (connected_tx, connected_rx) = oneshot::channel();
        let (stats_tx, stats_rx) = watch::channel(Default::default());
        let (server_changed_tx, server_changed_rx) = mpsc::channel(1);
        let (result_tx, result_rx) = watch::channel(Err(TaskError::Terminated));
        let remote_cfg = links.first().as_ref().map(|link| link.remote_cfg());
        let connected = Arc::new(AtomicBool::new(!links.is_empty()));

        Self {
            task: Task::new(
                cfg.clone(),
                remote_cfg.clone(),
                conn_id.clone(),
                direction,
                links_tx,
                link_rx,
                connected_tx,
                read_tx,
                read_closed_rx,
                write_rx,
                read_error_tx,
                write_error_tx,
                stats_tx,
                server_changed_rx,
                result_tx,
                links,
            ),
            channel: Channel::new(
                cfg.clone(),
                remote_cfg,
                conn_id.get(),
                write_tx,
                write_error_rx,
                read_rx,
                read_closed_tx,
                read_error_rx,
            ),
            control: Control {
                cfg,
                conn_id: conn_id.get(),
                server_id,
                remote_server_id: Arc::new(Mutex::new(remote_server_id)),
                direction,
                link_tx,
                links_rx,
                connected,
                stats_rx,
                server_changed_tx,
                result_rx,
            },
            connected_rx,
        }
    }
}
