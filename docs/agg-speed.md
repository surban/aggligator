# Command-Line Help for `agg-speed`

This document contains the help content for the `agg-speed` command-line program.

**Command Overview:**

* [`agg-speed`↴](#agg-speed)
* [`agg-speed client`↴](#agg-speed-client)
* [`agg-speed server`↴](#agg-speed-server)
* [`agg-speed show-cfg`↴](#agg-speed-show-cfg)

## `agg-speed`

Run speed test using a connection consisting of aggregated TCP links.

This uses Aggligator to combine multiple TCP links into one connection, providing the combined speed and resilience to individual link faults.

**Usage:** `agg-speed [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `client` — Raw speed test client
* `server` — Raw speed test server
* `show-cfg` — Shows the default configuration

###### **Options:**

* `--cfg <CFG>` — Configuration file
* `-d`, `--dump <DUMP>` — Dump analysis data to file



## `agg-speed client`

Raw speed test client

**Usage:** `agg-speed client [OPTIONS]`

###### **Options:**

* `-4`, `--ipv4` — Use IPv4

  Possible values: `true`, `false`

* `-6`, `--ipv6` — Use IPv6

  Possible values: `true`, `false`

* `-l`, `--limit <LIMIT>` — Limit test data to specified number of MB
* `-t`, `--time <TIME>` — Limit test duration to specified number of seconds
* `-s`, `--send-only` — Only measure send speed

  Possible values: `true`, `false`

* `-r`, `--recv-only` — Only measure receive speed

  Possible values: `true`, `false`

* `-b`, `--recv-block` — Block the receiver

  Possible values: `true`, `false`

* `-n`, `--no-monitor` — Do not display the link monitor

  Possible values: `true`, `false`

* `-a`, `--all-links` — Display all possible (including disconnected) links in the link monitor

  Possible values: `true`, `false`

* `-j`, `--json` — Output speed report in JSON format

  Possible values: `true`, `false`

* `--tls` — Encrypt all links using TLS, without authenticating server

  Possible values: `true`, `false`

* `--tcp <TCP>` — TCP server name or IP addresses and port number
* `--tcp-link-filter <TCP_LINK_FILTER>` — TCP link filter

  Default value: `interface-interface`
* `--websocket <WEBSOCKET>` — WebSocket hosts or URLs



## `agg-speed server`

Raw speed test server

**Usage:** `agg-speed server [OPTIONS]`

###### **Options:**

* `-i`, `--individual-interfaces` — Listen on each network interface individually

  Possible values: `true`, `false`

* `-n`, `--no-monitor` — Do not display the link monitor

  Possible values: `true`, `false`

* `--oneshot` — Exit after handling one connection

  Possible values: `true`, `false`

* `--tls` — Encrypt all links using TLS

  Possible values: `true`, `false`

* `--tcp <TCP>` — TCP port to listen on

  Default value: `5700`
* `--websocket <WEBSOCKET>` — WebSocket (HTTP) port to listen on

  Default value: `8080`



## `agg-speed show-cfg`

Shows the default configuration

**Usage:** `agg-speed show-cfg`



<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>

