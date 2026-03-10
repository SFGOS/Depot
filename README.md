# depot

Depot is a source-based package manager designed for Linux. It focuses on reproducibility, atomic installations, and ease of cross-compilation.

## Features

- **Source-based**: Downloads, extracts, and builds packages from source.
- **Dependency Management**: Automatically handles build-time and runtime dependencies.
- **Atomic Installation**: Uses a transactional approach to ensure system consistency.
- **Multi-system Build Support**: Built-in support for Autotools, CMake, Meson, Rust (Cargo), and custom build scripts.
- **Cross-Compilation**: Easily build packages for different architectures using cross-toolchains.
- **Repository Management**: Create and manage local package repositories.

## Quick Start

### Building Depot

```bash
cargo build --release
```

When installing via Meson (`meson install`), Depot now generates and installs:
- Shell completions for Bash, Zsh, and Fish
- A `depot(1)` man page

### Installing a Package

To install a package from a spec file:

```bash
depot install packages/zlib.toml
```

To install from a pre-built package archive:

```bash
depot install zlib-1.2.11-1-x86_64.depot.pkg.tar.zst
```

## Command Reference

- `install <SPEC_OR_ARCHIVE>`: Build and install a package.
  - Resolves a full dependency plan first (binary repos and/or source specs), then executes in dependency order.
  - Use `--yes` for non-interactive confirmation/provider selection.
  - Use `--dry-run` to print the plan without performing work.
  - Use `--test-deps` to include declared test dependencies in dependency installation.
  - Binary package installs verify both checksums and detached minisign signatures (`.sig`).
- `remove <PACKAGE>`: Remove an installed package.
- `build <SPEC>`: Build a package and create an archive without installing.
  - Resolves and offers to install missing build dependencies before fetching/building.
  - Missing test dependencies automatically disable test execution unless `--test-deps` or `[install].test_deps = true` is set.
- `update [PACKAGE ...]`: Update installed packages from configured repositories.
  - With no package names, updates every installed package that has a newer repo version available.
  - Refreshes source repos first, compares installed package version/revision and UTC completion time against repo metadata, and installs any newly introduced runtime dependencies before applying updates.
- `check [DIR]`: Recursively scan `DIR` (default `.`) for package specs and report newer upstream versions when they can be inferred from versioned source URLs.
  - Supports git tag-style sources such as `...git#v$version`.
  - Also checks tag-style release URLs such as GitHub `releases/download/$version/...`.
- `info <PACKAGE_OR_SPEC>`: Show information about a package.
- `search <QUERY>`: Search enabled source/binary repos by package name and provided features.
  - Use `--files` to search binary repo metadata file lists.
- `owns <PATH>`: Show which installed package owns a path.
- `list`: List all installed packages.
- `repo owns <PATH>`: Query binary repo metadata for path ownership.
- `repo create [DIR]`: Create a repository database from a directory of packages.
- `repo index [DIR] [--subdir <NAME> ...]`: Create/update `depot-index.tsv` at a source repo root for fast source lookup.
- `config`: Show current configuration and overrides.

## Package Specification

Packages are defined using TOML files. Here is a basic example:

```toml
[package]
name = "example"
version = "1.0.0"
description = "An example package"
homepage = "https://example.com"
license = "MIT"

[[source]]
url = "https://example.com/example-$version.tar.gz"
sha256 = "..."
extract_dir = "example-$version"

[build]
type = "autotools"
flags = { configure = ["--enable-feature"] }

[dependencies]
build = ["gcc", "make"]
runtime = ["libc"]
optional = ["bash-completion"]
```

LTO controls are available via `build.flags`:
- `ltoflags`: exported as `LTOFLAGS`
- `use_lto`: defaults to `true`; when enabled, `ltoflags` are appended to `CFLAGS`, `CXXFLAGS`, and `LDFLAGS`

## Configuration

Depot can be configured via `/etc/depot.d/` (or relative to the rootfs).

- `/etc/depot.d/build.toml`: System-wide build overrides and flag appends.
- `/etc/depot.d/package.toml`: Package-specific overrides.
- `/etc/depot.d/hooks/*.toml`: Transaction hooks for `install`/`update`/`remove` pre/post phases.

### Transaction Hooks

Hook files are TOML and use Starpack-like sections:

```toml
[hook]
name = "refresh-cache"

[when]
phase = "post"
operation = ["install", "update"]
packages = ["glibc", "linux*"]
paths = ["usr/lib/*"]
negation = ["usr/lib/debug/*"]

[exec]
command = "ldconfig"
needs_paths = true
```

`needs_paths = true` passes affected paths to the command via stdin (newline-separated).
