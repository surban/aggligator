//! The runtime.

use futures::FutureExt;
use std::{future::Future, panic};
use tokio::sync::oneshot;

use super::{
    sync_wrapper::SyncWrapper,
    task::{JoinError, JoinErrorRepr, JoinHandle},
};

/// Handle to the virtual runtime.
#[derive(Debug, Clone)]
pub struct Handle;

impl Handle {
    /// Returns a Handle view over the currently running Runtime.
    pub fn current() -> Self {
        Self
    }

    /// Spawns a future onto the browser.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();
        let (abort_tx, abort_rx) = oneshot::channel();

        wasm_bindgen_futures::spawn_local(async move {
            let res = tokio::select! {
                biased;
                res = panic::AssertUnwindSafe(future).catch_unwind() =>
                    res.map_err(|payload| JoinError(JoinErrorRepr::Panicked(SyncWrapper::new(payload)))),
                Ok(()) = abort_rx => Err(JoinError(JoinErrorRepr::Aborted)),
            };
            let _ = result_tx.send(res);
        });

        JoinHandle { result_rx: Box::pin(result_rx), abort_tx: Some(abort_tx) }
    }
}
