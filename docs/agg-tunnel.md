# Command-Line Help for `agg-tunnel`

This document contains the help content for the `agg-tunnel` command-line program.

**Command Overview:**

* [`agg-tunnel`↴](#agg-tunnel)
* [`agg-tunnel client`↴](#agg-tunnel-client)
* [`agg-tunnel server`↴](#agg-tunnel-server)
* [`agg-tunnel show-cfg`↴](#agg-tunnel-show-cfg)

## `agg-tunnel`

Forward TCP ports through a connection of aggregated links.

This uses Aggligator to combine multiple TCP links into one connection, providing the combined speed and resilience to individual link faults.

**Usage:** `agg-tunnel [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `client` — Tunnel client
* `server` — Tunnel server
* `show-cfg` — Shows the default configuration

###### **Options:**

* `--cfg <CFG>` — Configuration file
* `-d`, `--dump <DUMP>` — Dump analysis data to file



## `agg-tunnel client`

Tunnel client

**Usage:** `agg-tunnel client [OPTIONS] --port <PORT>`

###### **Options:**

* `-4`, `--ipv4` — Use IPv4

  Possible values: `true`, `false`

* `-6`, `--ipv6` — Use IPv6

  Possible values: `true`, `false`

* `-n`, `--no-monitor` — Do not display the link monitor

  Possible values: `true`, `false`

* `-a`, `--all-links` — Display all possible (including disconnected) links in the link monitor

  Possible values: `true`, `false`

* `-p`, `--port <PORT>` — Ports to forward from server to client
* `-g`, `--global` — Forward ports on all local interfaces

  Possible values: `true`, `false`

* `--once` — Exit after handling one connection

  Possible values: `true`, `false`

* `--tcp <TCP>` — TCP server name or IP addresses and port number
* `--tcp-link-filter <TCP_LINK_FILTER>` — TCP link filter

  Default value: `interface-interface`



## `agg-tunnel server`

Tunnel server

**Usage:** `agg-tunnel server [OPTIONS] --port <PORT>`

###### **Options:**

* `-n`, `--no-monitor` — Do not display the link monitor

  Possible values: `true`, `false`

* `-p`, `--port <PORT>` — Ports to forward to clients
* `--tcp <TCP>` — TCP port to listen on



## `agg-tunnel show-cfg`

Shows the default configuration

**Usage:** `agg-tunnel show-cfg`



<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>

