#!/bin/sh
# Installs the harness binary into ~/.cargo/bin and copies the shipped
# replib/*.py into ~/.harness/replib/. Never touches ~/.harness/config.toml,
# sessions/, or the store.
set -eu
cd "$(dirname "$0")"
cargo install --path . --locked     # builds and installs harness into ~/.cargo/bin
mkdir -p "$HOME/.harness/replib"
cp replib/*.py "$HOME/.harness/replib/"   # overwrites the shipped files only; other files there stay
