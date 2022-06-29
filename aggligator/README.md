# Aggligator — your friendly link aggregator

[![crates.io page](https://img.shields.io/crates/v/aggligator)](https://crates.io/crates/aggligator)
[![docs.rs page](https://docs.rs/aggligator/badge.svg)](https://docs.rs/aggligator)
[![Apache 2.0 license](https://img.shields.io/crates/l/aggligator)](https://raw.githubusercontent.com/surban/aggligator/master/LICENSE)

Aggligator aggregates multiple links into one connection.

Aggligator takes multiple network links (for example [TCP] connections) between two
endpoints and combines them into one connection that has the combined bandwidth
of all links. Additionally it provides resiliency against failure of individual
links and allows adding and removing of links on-the-fly.

It serves the same purpose as [Multipath TCP] and [SCTP] but works over existing,
widely adopted protocols such as TCP, HTTPS, TLS and WebSockets and is completely
implemented in user space without the need for any support from the operating system.

Aggligator is written in 100% safe [Rust] and builds upon the [Tokio]
asynchronous runtime.

[TCP]: https://en.wikipedia.org/wiki/Transmission_Control_Protocol
[Multipath TCP]: https://en.wikipedia.org/wiki/Multipath_TCP
[SCTP]: https://en.wikipedia.org/wiki/Stream_Control_Transmission_Protocol
[Rust]: https://www.rust-lang.org/
[Tokio]: https://tokio.rs/

## Features

The following crate features are available:

  * `dump` — enables saving of analysis data to disk, mainly useful for debugging 
    connection performance issues; also enables [Serde] support on some data types.

[Serde]: https://serde.rs/

## Working with TCP links and examples

Useful utils for working with TCP-based links, a visualizing link monitor
and a completely worked out example are provided in the [aggligator-util] crate.

[aggligator-util]: https://crates.io/crates/aggligator-util

## Demo

Two machines are connected via Ethernet and Wi-Fi.

Machine A, called `dino` and acting as the speed test server, has two interfaces: 
`enp8s0` (gigabit ethernet, IP address ending in `::b01`) and `wlp6s0` (Wi-Fi, IP address ending in `::83e`).
Both IP addresses are registered with the DNS server.

Machine B, acting as the speed test client, has four interfaces: `enp0s25` (gigabit ethernet), 
`enxf8eXXXXdd` (gigabit ethernet via USB), `enxf8eXXXXc5` (gigabit ethernet via USB) and `wlp3s0` (Wi-Fi).

Running the `agg-speed` tool from the [aggligator-util] crate on Machine B shows the following.

![Interactive monitor](https://raw.githubusercontent.com/surban/aggligator/master/.misc/monitor.png)

Aggligator has created 8 links between the machines, one for each pair of machine A and machine B interfaces.
The connection speed is about 100 MB/s in both directions which is expected from a full-duplex gigabit ethernet link.

Unplugging ethernet cables or disabling the Wi-Fi results in redistribution of the 
traffic over the remaining links, but has no effect on the connection.
If the ethernet cable is plugged in again or Wi-Fi is re-enabled, the link is 
automatically re-established.


## License

Aggligator is licensed under the [Apache 2.0 license].

[Apache 2.0 license]: https://github.com/surban/aggligator/blob/master/LICENSE

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Aggligator by you, shall be licensed as Apache 2.0, without any
additional terms or conditions.
