# Aggligator transport: SOCKS5

[![crates.io page](https://img.shields.io/crates/v/aggligator-transport-socks)](https://crates.io/crates/aggligator-transport-socks)
[![docs.rs page](https://docs.rs/aggligator-transport-socks/badge.svg)](https://docs.rs/aggligator-transport-socks)
[![Apache 2.0 license](https://img.shields.io/crates/l/aggligator-transport-socks)](https://raw.githubusercontent.com/surban/aggligator/master/LICENSE)

This crate provides unauthenticated SOCKS5 proxy transport for the [Aggligator link aggregator].

Each proxy is configured with an IP address or hostname, optionally including a port
number, and provides one link to the target. Proxy hostnames are resolved locally and
re-resolved periodically; one link is established per resolved IP address.
Target domain names are resolved by the proxy.

[Aggligator link aggregator]: https://crates.io/crates/aggligator

## License

Aggligator is licensed under the [Apache 2.0 license].

[Apache 2.0 license]: https://github.com/surban/aggligator/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Aggligator by you, shall be licensed as Apache 2.0, without any
additional terms or conditions.
