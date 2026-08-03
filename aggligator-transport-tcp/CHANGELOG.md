# Changelog

All notable changes to the Aggligator TCP transport will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.3.0 - 2026-08-03
### Added
- `TcpConnector::set_socket_setup` and `TcpConnector::set_stream_setup` as well as
  `TcpAcceptor::set_stream_setup` allow configuration of the underlying TCP socket
  before connecting and of the TCP stream after it has been established
- links can be bound to a local IP address instead of a network interface,
  expressed by the new `Local` enum
- `local_address_for_target` returns the local IP address used for reaching a target
### Changed
- **breaking:** the field `TcpLinkTag::interface` has been replaced by
  `TcpLinkTag::local`, which also carries the local IP address of a link;
  `TcpLinkTag::new` takes a `Local` instead of an interface name
- **breaking:** `interface_names_for_target` takes an `IpAddr` instead of a `SocketAddr`
- `TcpConnector::new`, `TcpConnector::unresolved`, `tcp_connect` and `tls_connect`
  accept any string-like values (`AsRef<str>`) instead of requiring `String`
- available network interfaces are rescanned when a link is disconnected, so that
  links are reestablished faster after a network change
- update aggligator to 0.10.0
- minimum supported Rust version is 1.97
