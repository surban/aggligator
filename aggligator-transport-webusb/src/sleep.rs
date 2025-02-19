use js_sys::Function;
use std::{
    cell::RefCell,
    fmt,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Duration,
};
use wasm_bindgen::{prelude::*, JsCast};
use web_sys::{Window, WorkerGlobalScope};

/// JavaScript sleep.
///
/// This is not Send + Sync since the underlying callback is bound to a
/// JavaScript thread.
pub struct JsSleep {
    inner: Rc<RefCell<JsSleepInner>>,
    timeout_id: i32,
    _callback: Closure<dyn FnMut()>,
}

impl fmt::Debug for JsSleep {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sleep").field("timeout_id", &self.timeout_id).finish()
    }
}

#[derive(Default)]
struct JsSleepInner {
    fired: bool,
    waker: Option<Waker>,
}

impl JsSleep {
    pub fn new(duration: Duration) -> Self {
        let inner = Rc::new(RefCell::new(JsSleepInner::default()));

        let callback = {
            let inner = inner.clone();
            Closure::new(move || {
                let mut inner = inner.borrow_mut();
                inner.fired = true;
                if let Some(waker) = inner.waker.take() {
                    waker.wake();
                }
            })
        };

        let timeout = duration.as_millis().try_into().expect("sleep duration overflow");
        let timeout_id = Self::register_timeout(callback.as_ref().unchecked_ref(), timeout);

        Self { inner, timeout_id, _callback: callback }
    }

    fn register_timeout(handler: &Function, timeout: i32) -> i32 {
        let global = js_sys::global();

        if let Some(window) = global.dyn_ref::<Window>() {
            window.set_timeout_with_callback_and_timeout_and_arguments_0(handler, timeout).unwrap()
        } else if let Some(worker) = global.dyn_ref::<WorkerGlobalScope>() {
            worker.set_timeout_with_callback_and_timeout_and_arguments_0(handler, timeout).unwrap()
        } else {
            panic!("unsupported JavaScript global: {global:?}");
        }
    }

    fn unregister_timeout(id: i32) {
        let global = js_sys::global();

        if let Some(window) = global.dyn_ref::<Window>() {
            window.clear_timeout_with_handle(id);
        } else if let Some(worker) = global.dyn_ref::<WorkerGlobalScope>() {
            worker.clear_timeout_with_handle(id);
        } else {
            panic!("unsupported JavaScript global: {global:?}");
        }
    }
}

impl Future for JsSleep {
    type Output = ();

    /// Waits until the sleep duration has elapsed.
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut inner = self.inner.borrow_mut();

        if inner.fired {
            return Poll::Ready(());
        }

        inner.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for JsSleep {
    fn drop(&mut self) {
        let inner = self.inner.borrow_mut();
        if !inner.fired {
            Self::unregister_timeout(self.timeout_id);
        }
    }
}
