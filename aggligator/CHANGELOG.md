# Changelog

All notable changes to Aggligator will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

