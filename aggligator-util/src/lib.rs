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

//! Utilities for working with the [Aggligator link aggregator](aggligator).
//!
//! This crate provides utility functions and command line tools for working with the
//! Aggligator link aggregator.
//!
//! It provides the following modules:
//!   * functions for establishing a connection consisting of [aggregated TCP links](net),
//!   * [transport implementations](transport) for TCP, Bluetooth RFCOMM sockets and WebSockets,
//!   * optional TLS link authentication and encryption,
//!   * a text-based, interactive [connection and link montor](monitor),
//!   * a [speed test](speed).
//!
//! The following command line tools are included:
//!   * `agg-speed` — performs a speed test over a connection of aggregated TCP links,
//!   * `agg-tunnel` — forwards arbitrary TCP ports over a connection of aggregated TCP links.
//!
//! Both tools display a text-based, interactive connection and link monitor.
//!
//! #### Simple aggregation of TCP links
//! Use the [tcp_connect](net::tcp_connect) and [tcp_server](net::tcp_server) functions
//! from the [net module](net).
//!

#[cfg(feature = "cli")]
#[doc(hidden)]
pub mod cli;
#[cfg(feature = "monitor")]
#[cfg_attr(docsrs, doc(cfg(feature = "monitor")))]
pub mod monitor;
#[cfg(feature = "tcp")]
#[cfg_attr(docsrs, doc(cfg(feature = "tcp")))]
pub mod net;
#[cfg(feature = "speed")]
#[cfg_attr(docsrs, doc(cfg(feature = "speed")))]
pub mod speed;
pub mod transport;
