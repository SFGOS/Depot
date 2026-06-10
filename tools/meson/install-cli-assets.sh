#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 5 ]; then
  echo "usage: $0 <depot-bin> <bash-dir> <zsh-dir> <fish-dir> <man1-dir>" >&2
  exit 2
fi

depot_bin="$1"
bash_dir="$2"
zsh_dir="$3"
fish_dir="$4"
man1_dir="$5"

if [ ! -x "$depot_bin" ]; then
  echo "error: depot binary is not executable: $depot_bin" >&2
  exit 1
fi

if [ -z "${MESON_INSTALL_DESTDIR_PREFIX:-}" ]; then
  echo "error: MESON_INSTALL_DESTDIR_PREFIX is not set" >&2
  exit 1
fi

resolve_dest() {
  case "$1" in
    /*) printf '%s%s\n' "${DESTDIR:-}" "$1" ;;
    *) printf '%s/%s\n' "$MESON_INSTALL_DESTDIR_PREFIX" "$1" ;;
  esac
}

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

"$depot_bin" generate-artifacts --out-dir "$tmpdir"

bash_dest="$(resolve_dest "$bash_dir")"
zsh_dest="$(resolve_dest "$zsh_dir")"
fish_dest="$(resolve_dest "$fish_dir")"
man_dest="$(resolve_dest "$man1_dir")"

mkdir -p "$bash_dest" "$zsh_dest" "$fish_dest" "$man_dest"
cp "$tmpdir/depot.bash" "$bash_dest/depot"
cp "$tmpdir/_depot" "$zsh_dest/_depot"
cp "$tmpdir/depot.fish" "$fish_dest/depot.fish"
cp "$tmpdir/depot.1" "$man_dest/depot.1"
