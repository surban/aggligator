//! Tools for binding a value to a thread.
//!
//! This is used for WebAssembly targets, since JavaScript types are usually not Send + Sync
//! but occur within a type that must be Send + Sync. However, when running in a
//! JavaScript environment the async executor is single-threaded anyway, so the
//! types will never be accessed from another thread.
//!

#![allow(dead_code)]

use futures::{Sink, Stream};
use std::{
    fmt,
    future::Future,
    mem::{needs_drop, ManuallyDrop},
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll},
    thread,
    thread::ThreadId,
};

/// Allows access to a value only from the thread that created this,
/// but always implements [`Send`] and [`Sync`].
///
/// The debug representation can be safely used from any thread.
///
/// ### Panics
/// Panics if the inner value is accessed in any way or,
/// if it needs drop, dropped from another thread.
pub struct ThreadBound<T> {
    value: ManuallyDrop<T>,
    thread_id: ThreadId,
    taken: bool,
}

unsafe impl<T> Send for ThreadBound<T> {}
unsafe impl<T> Sync for ThreadBound<T> {}

impl<T> ThreadBound<T> {
    /// Binds the value to the current thread.
    pub fn new(value: T) -> Self {
        Self { thread_id: thread::current().id(), value: ManuallyDrop::new(value), taken: false }
    }

    /// The id of the thread that is allowed to access the inner value.
    pub fn thread_id(this: &Self) -> ThreadId {
        this.thread_id
    }

    /// Takes the inner value out.
    ///
    /// ### Panics
    /// Panics if this was created by another thread.
    pub fn into_inner(mut this: Self) -> T {
        this.check();
        this.taken = true;
        unsafe { ManuallyDrop::take(&mut this.value) }
    }

    /// Whether the value is usable from the current thread.
    #[inline]
    pub fn is_usable(this: &Self) -> bool {
        thread::current().id() == this.thread_id
    }

    #[inline]
    fn check(&self) {
        if !Self::is_usable(self) {
            panic!(
                "cannot use value on thread {:?} since it belongs to thread {:?}",
                thread::current().id(),
                self.thread_id
            );
        }
    }
}

impl<T> Deref for ThreadBound<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.check();
        &self.value
    }
}

impl<T> DerefMut for ThreadBound<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.check();
        &mut self.value
    }
}

impl<T> fmt::Debug for ThreadBound<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut d = f.debug_struct("ThreadBound");
        d.field("thread_id", &self.thread_id);
        if Self::is_usable(self) {
            d.field("value", &self.value);
        }
        d.finish()
    }
}

impl<T> fmt::Display for ThreadBound<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.check();
        self.value.fmt(f)
    }
}

impl<T> Clone for ThreadBound<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        self.check();
        Self { thread_id: self.thread_id, value: self.value.clone(), taken: self.taken }
    }
}

impl<T> std::borrow::Borrow<T> for ThreadBound<T> {
    fn borrow(&self) -> &T {
        self.check();
        &self.value
    }
}

impl<T> std::borrow::BorrowMut<T> for ThreadBound<T> {
    fn borrow_mut(&mut self) -> &mut T {
        self.check();
        &mut self.value
    }
}

impl<T> PartialEq for ThreadBound<T>
where
    T: PartialEq,
{
    fn eq(&self, other: &ThreadBound<T>) -> bool {
        self.check();
        other.check();
        self.value.eq(&other.value)
    }
}

impl<T> PartialEq<T> for ThreadBound<T>
where
    T: PartialEq,
{
    fn eq(&self, other: &T) -> bool {
        self.check();
        (*self.value).eq(other)
    }
}

impl<T> Eq for ThreadBound<T> where T: Eq {}

impl<T> PartialOrd for ThreadBound<T>
where
    T: PartialOrd,
{
    fn partial_cmp(&self, other: &ThreadBound<T>) -> Option<std::cmp::Ordering> {
        self.check();
        other.check();
        self.value.partial_cmp(&other.value)
    }
}

impl<T> Ord for ThreadBound<T>
where
    T: Ord,
{
    fn cmp(&self, other: &ThreadBound<T>) -> std::cmp::Ordering {
        self.check();
        other.check();
        self.value.cmp(&other.value)
    }
}

impl<T> std::hash::Hash for ThreadBound<T>
where
    T: std::hash::Hash,
{
    fn hash<H>(&self, state: &mut H)
    where
        H: std::hash::Hasher,
    {
        self.check();
        self.value.hash(state)
    }
}

impl<T> Drop for ThreadBound<T> {
    fn drop(&mut self) {
        if needs_drop::<T>() && !self.taken {
            self.check();
            unsafe { ManuallyDrop::drop(&mut self.value) };
        }
    }
}

impl<T> Future for ThreadBound<T>
where
    T: Future,
{
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.check();
        let future = unsafe { self.map_unchecked_mut(|s| &mut *s.value) };
        future.poll(cx)
    }
}

impl<T, S> Sink<S> for ThreadBound<T>
where
    T: Sink<S>,
{
    type Error = T::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.check();
        let sink = unsafe { self.map_unchecked_mut(|s| &mut *s.value) };
        sink.poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: S) -> Result<(), Self::Error> {
        self.check();
        let sink = unsafe { self.map_unchecked_mut(|s| &mut *s.value) };
        sink.start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.check();
        let sink = unsafe { self.map_unchecked_mut(|s| &mut *s.value) };
        sink.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.check();
        let sink = unsafe { self.map_unchecked_mut(|s| &mut *s.value) };
        sink.poll_close(cx)
    }
}

impl<T> Stream for ThreadBound<T>
where
    T: Stream,
{
    type Item = T::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.check();
        let stream = unsafe { self.map_unchecked_mut(|s| &mut *s.value) };
        stream.poll_next(cx)
    }
}
