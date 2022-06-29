//! Aggregated link connection.
//!
//! This provides a connection that is backed by a connection consisting of aggregated links.
//!
//! An [aggregated link channel](Channel) supports both message-based communication,
//! using a [Sender] and [Receiver], and [stream-based IO](Stream).
//!

mod channel;
pub(crate) mod receiver;
pub(crate) mod sender;

pub use channel::{Channel, Stream};
pub use receiver::{Receiver, ReceiverStream, RecvError};
pub use sender::{SendError, Sender, SenderSink};
