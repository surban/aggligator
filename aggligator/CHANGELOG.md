# Changelog

All notable changes to Aggligator will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.10.1 - 2026-08-03
### Fixed
- possible hang during graceful connection termination when local receiver is dropped

## 0.10.0 - 2026-08-03
### Added
- link-specific configuration via the new `LinkCfg` struct, which can be
  specified globally using `Cfg::link`, per transport using
  `Connector::set_link_cfg` and `Acceptor::set_link_cfg`, per link tag using
  `LinkTag::link_cfg` and changed at runtime using `Link::set_link_cfg`
- configuration options for explicit and automatic flushing of links:
  `Cfg::ignore_flush`, `LinkCfg::flush_interval`, `LinkCfg::ack_flush_interval`
  and `LinkCfg::unflushed_limit`
- configurable schedule for increasing the amount of unacknowledged data sent
  over a link (`LinkCfg::unacked_increase`), allowing much faster discovery of
  the available bandwidth
- `Cfg::link_max_ping_spread` option to exclude links that are much slower
  than the fastest link
- `LinkCfg::test_after_ack_timeout` option to retest a link after an
  acknowledgement timeout
- `LinkCfg::ack_timeout_resent_factor` and `LinkCfg::ack_timeout_unreliable_factor`
  options for fine-tuning of acknowledgement timeouts
- `LinkCfg::io_packet_size` option for the packet size of IO-stream-based links
- `ConnectingTransport::link_disconnected` is called when a link has been
  disconnected, allowing a transport to react, for example by rescanning the
  available network interfaces
- `Control::cfg` provides access to the configuration of a connection
- `IoTx` and `IoRx` can be created with an explicit buffer capacity
### Changed
- **breaking:** migration to Rust edition 2024 and minimum supported Rust
  version 1.97
- **breaking:** link-specific options have been moved from `Cfg` into `LinkCfg`
  for example `Cfg::link_ping_timeout` is now `Cfg::link.ping_timeout`
- **breaking:** `Control::add` and `Control::add_io` take the link-specific
  configuration as an additional argument
- **breaking:** the fields of `IoTx` and `IoRx` are now private
- **breaking:** the variant `AbortedByServer` has been added to `AddLinkError`,
  `DisconnectReason`, `SendError`, `RecvError` and `TaskError`, to indicate
  that a connection has been closed by the server while all links were disconnected
- default configuration has been retuned for significantly higher throughput
  and faster reaction to link failures: larger send and receive buffers,
  shorter link test and non-working timeouts, automatic acknowledgement
  flushing and a limited amount of link test data
- links that are blocked or unusable are tested and reused faster
- log messages contain the link tag and connection id, making them much easier
  to follow
### Fixed
- data is not resent over the link it was originally sent on
- flushing is completed before the sender sink reports readiness

## 0.9.12 - 2026-07-31
### Fixed
- Denial of service due to remotely triggered memory exhaustion:
  a malicious remote endpoint could send a reliable message with a large
  sequence number offset, triggering a huge memory allocation.
  This is fixed by limiting the receive queue size to roughly 1 MB.

## 0.9.11 - 2026-04-13
### Fixed
- panic in SenderSink::poll_flush during teardown

## 0.9.10 - 2026-03-18
### Changed
- faster CRC32 calculation 

## 0.9.9 - 2025-11-15
### Fixed
- do not touch a sink anymore after first error

## 0.9.8 - 2025-09-11
### Added
- Control::terminate method to forcefully terminate a connection

## 0.9.7 - 2025-08-19
### Added
- better integration with tracing crate:
  aggligator now uses spans for connections and transport tasks

## 0.9.6 - 2025-06-22
### Added
- transport: public type aliases for boxed types

## 0.9.5 - 2025-03-08
### Fixed
- panic when transport handle is dropped

## 0.9.4 - 2025-02-19
### Changed
- update dependencies

## 0.9.3 - 2025-01-23
### Changed
- documentation

## 0.9.2 - 2025-01-23
### Fixed
- documentation

## 0.9.1 - 2025-01-23
### Fixed
- documentation

## 0.9.0 - 2025-01-23
### Added
- WebAssembly support
- JavaScript runtime environment support, enabled by `js` crate feature
### Changed
- move transport module from aggligator-util into aggligator crate

## 0.8.3 - 2023-11-02
### Changed
- shorten log messages

## 0.8.2 - 2023-09-06
### Changed
- update dependencies

## 0.8.1 - 2023-02-13
### Changed
- move repetitve debug messages to trace level

## 0.8.0 - 2023-02-13
### Changed
- harmonize change notifications

## 0.7.1 - 2023-02-10
### Added
- configuration option `link_test_data_limit` to limit amount of test data
  for link testing

## 0.7.0 - 2023-02-08
### Added
- improved error reporting

## 0.6.0 - 2023-02-07
### Added
- configuration option to disconnect on server id mismatch

## 0.5.1 - 2023-02-07
### Changed
- reduce debug logging

## 0.5.0 - 2023-02-06
### Added
- link blocking
### Changed
- protocol version 4

## 0.4.0 - 2023-02-06
### Added
- data integrity checking for IO-based links
- publish reason why a link is currently not working
### Changed
- protocol version 3
- optimize resend queue handling

## 0.3.3 - 2023-02-05
### Added
- statistics for number of link hangs
### Changed
- optimize unconfirmed link handling
- optimize resend queue handling

## 0.3.2 - 2023-02-02
### Added
- `link_max_ping` configuration option to only use links
  that satisfy the ping requirement
- control methods to mark links and stats as seen
### Fixed
- optimize resending
- race condition when testing link
- do not wait for flush of unconfirmed links
- do not use crypto random number generator when unnecessary

## 0.3.1
### Added
- `control::links_update` and `control::stats_update` methods

## 0.3.0
### Added
- convert error types into std::io::Error
### Changed
- remove unnecessary async on some functions

## 0.2.2
### Added
- re-exports for easier use

## 0.2.1
### Changed
- use cryptographic random number generator for connection id

## 0.2.0
### Added
- encrypt connection id using a shared secret exchanged using Diffie-Helmann;
  this hinders an eavesdropper to take over a connection by spoofing the
  connection id
### Changed
- increse buffer sizes and adjust timeouts for better performance over high latency
  links
### Fixed
- link disconnect reason for link filter rejection

## 0.1.1
### Fixed
- make `dump` non-default feature

## 0.1.0
### Added
- initial release

