#!/bin/bash
#
# Generate CLI documentation.
#

SCRIPT_PATH="$(dirname "${BASH_SOURCE[0]}")"

cd "$SCRIPT_PATH/.."
mkdir -p docs

cargo run --quiet --bin agg-speed -- markdown > docs/agg-speed.md
cargo run --quiet --bin agg-tunnel -- markdown > docs/agg-tunnel.md

