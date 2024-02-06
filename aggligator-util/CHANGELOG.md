# Changelog

All notable changes to aggligator utilities will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.13.0 - 2024-02-06
### Changes
- update dependencies
### Fixed
- make sure that IPv4 connections are accepted

## 0.12.2 - 2024-02-06
### Fixed
- make sure that IPv4 connections are accepted

## 0.12.1 - 2024-02-05
### Fixed
- build on non-Linux platforms

## 0.12.0 - 2023-11-11
### Changed
- update upc to 0.4.0

## 0.11.0 - 2023-11-07
### Changed
- update upc to 0.3.1

## 0.10.2 - 2023-11-03
### Added
- USB transport using USB packet channel (UPC)

## 0.10.1 - 2023-09-08
### Changed
- minimum supported Rust version is now 1.70.0

## 0.10.0 - 2023-09-06
### Changed
- update dependencies

## 0.9.0 - 2023-07-05
### Added
- support for packet-based transports
- WebSocket transport
- method to query whether Acceptor is empty

## 0.8.0 - 2023-02-13
### Changed
- use Aggligator 0.8.0

## 0.7.2 - 2023-02-08
### Changed
- monitor: use stats interval for refresh

## 0.7.1 - 2023-02-08
### Added
- agg-tunnel: RFCOMM transport support
- agg-tunnel: once connection mode in client
### Fixed
- agg-tunnel: handle multiple connections in server

## 0.7.0 - 2023-02-08
### Changed
- use Aggligator 0.7.0

## 0.6.0 - 2023-02-07
### Changed
- use Aggligator 0.6.0
- rename crate features for consistency and shortness

## 0.5.0 - 2023-02-06
### Changed
- use Aggligator 0.5.0

## 0.4.0 - 2023-02-06
### Added
- link monitor: display link unconfirmed reason
- link monitor: display link connected time
### Changed
- use Aggligator 0.4.0

## 0.3.3 - 2023-02-05
### Added
- command line option `--individual-interfaces` for servers to listen on each
  network interface individually, necessary when bandwidth limits are in place
### Changed
- use Aggligator 0.3.3

## 0.3.2 - 2023-02-02
### Changed
- use Aggligator 0.3.2
### Fixed
- ignore temporary failures in name resolution

## 0.3.1
### Added
- Bluetooth RFCOMM support using profiles

## 0.3.0
### Added
- transport module providing management of heterogenous transports for a connection
- Bluetooth RFCOMM support
### Removed
- net::adv functions superseeded by transport module

## 0.2.2
### Changed
- disable Nagle's algorithm for TCP connections

## 0.2.1
### Fixed
- wait for connection termination when quitting agg-speed

## 0.2.0
### Added
- TLS over TCP support
- boxed wrapper types for link transports

## 0.1.3
### Added
- report error when link connection task fails

## 0.1.2
### Fixed
- no reconnection possible if DNS name resolution fails temporarily

## 0.1.1
### Added
- support for non-Linux platforms

## 0.1.0
### Added
- initial release

