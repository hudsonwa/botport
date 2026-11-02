#!/bin/sh
# Build the botport wheel with host paths stripped from the binary (panic locations would otherwise embed
# $HOME/.cargo/... and this checkout's path). Usage: packaging/python/build-wheel.sh [extra maturin args]
# Needs maturin on PATH (e.g. a venv: python3 -m venv /tmp/mat && /tmp/mat/bin/pip install 'maturin>=1.5,<2').
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$root=/botport --remap-path-prefix=$HOME=/home"
export RUSTFLAGS
cd "$here"
maturin build --release --out "$root/dist/wheels" "$@"
