#!/bin/sh
set -eu

if [ "$#" -ne 15 ]; then
    echo "usage: $0 <cargo> <src_root> <build_root> <profile> <release_flag> <build_static> <autotools_pkg> <cmake_pkg> <meson_pkg> <perl_pkg> <custom_pkg> <python_pkg> <rust_pkg> <makefile_pkg> <development_pkg>" >&2
    exit 2
fi

cargo_bin="$1"
src_root="$2"
build_root="$3"
profile="$4"
release_flag="$5"
build_static="$6"
autotools_pkg="$7"
cmake_pkg="$8"
meson_pkg="$9"
perl_pkg="${10}"
custom_pkg="${11}"
python_pkg="${12}"
rust_pkg="${13}"
makefile_pkg="${14}"
development_pkg="${15}"

cargo_home="$build_root/cargo-home"
cargo_target_dir="$build_root/cargo-target"

mkdir -p "$cargo_home" "$cargo_target_dir"

export CARGO_HOME="$cargo_home"
export CARGO_TARGET_DIR="$cargo_target_dir"
export BUILD_DEPOT_STATIC="$build_static"
export DEPOT_AUTOTOOLS_PACKAGE="$autotools_pkg"
export DEPOT_CMAKE_PACKAGE="$cmake_pkg"
export DEPOT_MESON_PACKAGE="$meson_pkg"
export DEPOT_PERL_PACKAGE="$perl_pkg"
export DEPOT_CUSTOM_PACKAGE="$custom_pkg"
export DEPOT_PYTHON_PACKAGE="$python_pkg"
export DEPOT_RUST_PACKAGE="$rust_pkg"
export DEPOT_MAKEFILE_PACKAGE="$makefile_pkg"
export DEPOT_DEVELOPMENT_PACKAGE="$development_pkg"

if [ "$release_flag" = "1" ]; then
    exec "$cargo_bin" test --locked --manifest-path "$src_root/Cargo.toml" --profile "$profile"
fi

exec "$cargo_bin" test --locked --manifest-path "$src_root/Cargo.toml"
