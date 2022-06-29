//
// Copyright 2022 Sebastian Urban <surban@surban.net>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! Aggregates multiple links into one connection.
//!
//! Aggligator takes multiple network links (for example [TCP] connections) between two
//! endpoints and combines them into one connection that has the combined bandwidth
//! of all links. Additionally it provides resiliency against failure of individual
//! links and allows adding and removing of links on-the-fly.
//!
//! It serves the same purpose as [Multipath TCP] and [SCTP] but works over existing,
//! widely adopted protocols such as TCP, HTTPS, TLS and WebSockets and is completely
//! implemented in user space without the need for any support from the operating system.
//!
//! Aggligator is written in 100% safe Rust and builds upon the [Tokio](tokio)
//! asynchronous runtime.
//!
//! [TCP]: https://en.wikipedia.org/wiki/Transmission_Control_Protocol
//! [Multipath TCP]: https://en.wikipedia.org/wiki/Multipath_TCP
//! [SCTP]: https://en.wikipedia.org/wiki/Stream_Control_Transmission_Protocol
//!
//! # Link requirements
//!
//! A link can either be stream-based (implementing the [AsyncRead] and [AsyncWrite] traits)
//! or packet-based (implementing the [Sink] and [Stream] traits).
//! In both cases the implementation of the link must ensure data integrity and deliver data
//! in the same order as it was sent.
//! If data has been lost or corrupted underway, the link must handle retransmission
//! and, if that is unsuccessful, fail by disconnecting itself.
//!
//! In the case of TCP this is handled by the operating system and thus
//! a [TcpStream] or protocols building on top of that (such as TLS or WebSockets)
//! can be directly used as links.
//!
//! Other then the requirements stated above, Aggligator makes no assumption about
//! the type of links and can work over any networking methodology such as
//! TCP/IP, Bluetooth, and serial links. It never interfaces directly with the
//! operating system and only uses links provided by the user.
//!
//! [AsyncRead]: tokio::io::AsyncRead
//! [AsyncWrite]: tokio::io::AsyncWrite
//! [Sink]: futures::sink::Sink
//! [Stream]: futures::stream::Stream
//! [TcpStream]: https://docs.rs/tokio/1/tokio/net/struct.TcpStream.html
//!
//! # Basic usage
//!
//! See the [connect module](connect) on how to accept incoming connections
//! and establish outgoing connections.
//!
//! # Working with TCP links and examples
//!
//! Useful utils for working with TCP-based links, a visualizing link monitor
//! and a completely worked out example are provided in the [aggligator-util] crate.
//!
//! [aggligator-util]: https://docs.rs/aggligator-util/latest/aggligator_util/
//!

mod agg;
pub mod alc;
pub mod cfg;
pub mod connect;
pub mod control;
pub mod id;
pub mod io;
mod msg;
mod peekable_mpsc;
mod seq;

#[cfg(feature = "dump")]
#[cfg_attr(docsrs, doc(cfg(feature = "dump")))]
pub use agg::dump;

pub use agg::task::Task;

/// Link aggregator protocol error.
macro_rules! protocol_err {
    ($($t:tt)*) => {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!($($t)*))
    };
}

pub(crate) use protocol_err;