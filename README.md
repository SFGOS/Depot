# depot

Depot is a source-based package manager for Linux, designed for Linux. It focuses on reproducibility, atomic installations, and ease of cross-compilation.

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
- `remove <PACKAGE>`: Remove an installed package.
- `build <SPEC>`: Build a package and create an archive without installing.
- `info <PACKAGE_OR_SPEC>`: Show information about a package.
- `list`: List all installed packages.
- `repo create [DIR]`: Create a repository database from a directory of packages.
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
```

## Configuration

Depot can be configured via `/etc/depot.d/` (or relative to the rootfs).

- `/etc/depot.d/build.toml`: System-wide build overrides and flag appends.
- `/etc/depot.d/package.toml`: Package-specific overrides.
