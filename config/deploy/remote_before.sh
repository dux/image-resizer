#!/usr/bin/env bash
# remote_before.sh - runs on the SERVER in <app_dir>/new-release/, before the swap.
# Non-zero exit aborts the deploy and release/ keeps serving.
#
# rs-vips links the box's libvips (libvips-dev + pkg-config), so the binary is built here.
# Rust comes from ./mise.toml (lux-deploy has run `mise install`). tmp/ is the shared dir that
# survives releases, so the cargo target dir there keeps repeat builds incremental.
set -euo pipefail

export CARGO_TARGET_DIR="$PWD/tmp/cargo-target"
mise exec -- cargo build --release --locked
mkdir -p bin
cp "$CARGO_TARGET_DIR/release/image-resizer" bin/server
