# Changelog

All notable changes to the Aggligator WebSocket transport for the web will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.5.0 - 2026-08-03
### Changed
- the minimum acknowledgement timeout of a link is increased by 3 seconds,
  since connections from a web browser tend to lag
- update aggligator to 0.10.0
- migration to Rust edition 2024 and minimum supported Rust version 1.97
