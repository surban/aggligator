//! Peekable MPSC wrapper.

use tokio::sync::mpsc;

/// Receiver that allows peeking at next message.
#[derive(Debug)]
pub struct PeekableReceiver<T> {
    rx: mpsc::Receiver<T>,
    peeked: Option<T>,
}

impl<T> From<mpsc::Receiver<T>> for PeekableReceiver<T> {
    fn from(rx: mpsc::Receiver<T>) -> Self {
        Self::new(rx)
    }
}

impl<T> PeekableReceiver<T> {
    /// Creates a new peekable receiver.
    pub fn new(rx: mpsc::Receiver<T>) -> Self {
        Self { rx, peeked: None }
    }

    /// Receive next message.
    #[allow(dead_code)]
    pub async fn recv(&mut self) -> Option<T> {
        match self.peeked.take() {
            Some(msg) => Some(msg),
            None => self.rx.recv().await,
        }
    }

    /// Receives next message, if one is immediately available.
    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        match self.peeked.take() {
            Some(msg) => Ok(msg),
            None => self.rx.try_recv(),
        }
    }

    /// Peeks at the next message.
    pub async fn peek(&mut self) -> Option<&T> {
        if self.peeked.is_none() {
            self.peeked = self.rx.recv().await;
        }

        self.peeked.as_ref()
    }

    /// Peeks at the next message, if one is available.
    pub fn try_peek(&mut self) -> Result<&T, mpsc::error::TryRecvError> {
        if self.peeked.is_none() {
            self.peeked = Some(self.rx.try_recv()?);
        }

        Ok(self.peeked.as_ref().unwrap())
    }

    /// Receives the next messages if the condition is fulfilled.
    pub async fn recv_if(&mut self, cond: impl FnOnce(&T) -> bool) -> Result<T, RecvIfError> {
        match self.peek().await {
            Some(msg) if cond(msg) => Ok(self.try_recv().unwrap()),
            Some(_) => Err(RecvIfError::NoMatch),
            None => Err(RecvIfError::Disconnected),
        }
    }

    /// Receives the next messages if it is immediately available and the condition is fulfilled.
    pub fn try_recv_if(&mut self, cond: impl FnOnce(&T) -> bool) -> Result<T, TryRecvIfError> {
        if cond(self.try_peek()?) {
            Ok(self.try_recv().unwrap())
        } else {
            Err(TryRecvIfError::NoMatch)
        }
    }
}

/// Receive if condition fulfilled error.
#[derive(Debug, Clone)]
pub enum RecvIfError {
    /// Sender has been closed.
    Disconnected,
    /// Condition is not fulfilled.
    NoMatch,
}

/// Receive if condition fulfilled error.
#[derive(Debug, Clone)]
pub enum TryRecvIfError {
    /// No message is present.
    Empty,
    /// Sender has been closed.
    Disconnected,
    /// Condition is not fulfilled.
    NoMatch,
}

impl From<mpsc::error::TryRecvError> for TryRecvIfError {
    fn from(err: mpsc::error::TryRecvError) -> Self {
        match err {
            mpsc::error::TryRecvError::Empty => Self::Empty,
            mpsc::error::TryRecvError::Disconnected => Self::Disconnected,
        }
    }
}
