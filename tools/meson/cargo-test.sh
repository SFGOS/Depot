#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <cargo> <src_root> <build_root>" >&2
    exit 2
fi

cargo_bin="$1"
src_root="$2"
build_root="$3"

cargo_home="$build_root/cargo-home"
cargo_target_dir="$build_root/cargo-target"

mkdir -p "$cargo_home" "$cargo_target_dir"

export CARGO_HOME="$cargo_home"
export CARGO_TARGET_DIR="$cargo_target_dir"

exec "$cargo_bin" test --locked --manifest-path "$src_root/Cargo.toml"
