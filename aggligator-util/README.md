# Utilities for working with Aggligator

[![crates.io page](https://img.shields.io/crates/v/aggligator-util)](https://crates.io/crates/aggligator-util)
[![docs.rs page](https://docs.rs/aggligator-util/badge.svg)](https://docs.rs/aggligator-util)
[![Apache 2.0 license](https://img.shields.io/crates/l/aggligator-util)](https://raw.githubusercontent.com/surban/aggligator/master/LICENSE)

This crate provides utility functions and command line tools for working with the
[Aggligator link aggregator].

It provides the following modules:
  * functions for establishing a connection consisting of aggregated TCP links,
  * a text-based, interactive connection and link montor,
  * a speed test.

The following command line tools are included:
  * `agg-speed` — performs a speed test over a connection of aggregated TCP links,
  * `agg-tunnel` — forwards arbitrary TCP ports over a connection of aggregated TCP links.

Both tools display a text-based, interactive connection and link monitor.

[Aggligator link aggregator]: https://crates.io/crates/aggligator

## Features

The following crate features are available:

  * `monitor` — enables the text-based, interactive connection and link monitor,
  * `speed-test` — enables speed test functions,
  * `dump` — enables saving of analysis data to disk.

## Installing the command line tools

Run the following command to install the command line tools:

    cargo install aggligator-util

## License

Aggligator is licensed under the [Apache 2.0 license].

[Apache 2.0 license]: https://github.com/surban/aggligator/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Aggligator by you, shall be licensed as Apache 2.0, without any
additional terms or conditions.