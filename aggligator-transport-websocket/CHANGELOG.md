# Changelog

All notable changes to the Aggligator WebSocket transport will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.7.0 - 2026-08-03
### Added
- `WebSocketConnector::set_socket_setup` and `WebSocketConnector::set_stream_setup`
  allow configuration of the underlying TCP socket before connecting and of the
  TCP stream after it has been established
### Changed
- **breaking:** the field `OutgoingWebSocketLinkTag::interface` has been replaced by
  `OutgoingWebSocketLinkTag::local`, which also carries the local IP address of a link
- **breaking:** update tokio-tungstenite and tungstenite to 0.30
- the minimum acknowledgement timeout of a link is increased by 3 seconds,
  since WebSocket connections from web browsers tend to lag
- available network interfaces are rescanned when a link is disconnected, so that
  links are reestablished faster after a network change
- update aggligator to 0.10.0
- minimum supported Rust version is 1.97
