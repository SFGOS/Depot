use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

/// Package metadata
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct PackageInfo {
    pub name: String,
    /// Stable package stream name used to associate renamed ABI-split packages.
    #[serde(default, alias = "real-name", skip_serializing_if = "Option::is_none")]
    pub real_name: Option<String>,
    pub version: String,
    /// Maintenance revision of the package (defaults to 1)
    #[serde(default = "default_revision")]
    pub revision: u32,
    pub description: String,
    pub homepage: String,
    /// When true, renamed updates retain versioned shared libraries from the old package.
    #[serde(default, alias = "abi-breaking")]
    pub abi_breaking: bool,
    #[serde(
        deserialize_with = "deserialize_licenses",
        serialize_with = "serialize_licenses"
    )]
    pub license: Vec<String>,
}

impl PackageInfo {
    /// Return the stable package stream name, defaulting to the package name.
    pub fn effective_real_name(&self) -> &str {
        self.real_name.as_deref().unwrap_or(&self.name)
    }
}

fn default_revision() -> u32 {
    1
}

fn deserialize_licenses<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrArray {
        String(String),
        Array(Vec<String>),
    }

    match StringOrArray::deserialize(deserializer)? {
        StringOrArray::String(s) => Ok(vec![s]),
        StringOrArray::Array(v) => Ok(v),
    }
}

fn serialize_licenses<S>(licenses: &[String], serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if licenses.len() == 1 {
        serializer.serialize_str(&licenses[0])
    } else {
        licenses.serialize(serializer)
    }
}

/// Nested alternatives override group used for output-specific variants such as `lib32-*`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct AlternativeGroup {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

/// Package alternatives such as virtual provides, install conflicts, and replacements.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Alternatives {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
    /// Optional alternatives override used only for the generated `lib32-*` companion package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lib32: Option<AlternativeGroup>,
}

impl Alternatives {
    /// Return the optional lib32-specific alternatives override set.
    pub fn lib32_alternatives(&self) -> Option<Alternatives> {
        self.lib32.as_ref().map(|group| Alternatives {
            provides: group.provides.clone(),
            conflicts: group.conflicts.clone(),
            replaces: group.replaces.clone(),
            lib32: None,
        })
    }
}

/// Source tarball information
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Source {
    pub url: String,
    /// Checksum for the source (e.g. `sha256:...`, `sha512:...`, `sha1:...`, `md5:...`, `b2:...`, `b2sum:...`, or raw SHA256 hex).
    /// Defaults to `skip` when omitted.
    #[serde(default = "default_source_sha256")]
    pub sha256: String,
    /// Directory name after extraction (supports $name, $version)
    pub extract_dir: String,

    /// Patch files or URLs to apply after extraction.
    ///
    /// Example:
    /// patches = ["fix-build.patch", "<https://example.com/patches/foo.patch>"]
    #[serde(default)]
    pub patches: Vec<String>,

    /// Commands to run after extraction (and after patches), executed in the source directory.
    ///
    /// Example:
    /// post_extract = ["autoreconf -fi"]
    #[serde(default)]
    pub post_extract: Vec<String>,

    /// Optional list of git commit hashes/revs to cherry-pick after checkout.
    ///
    /// This is only valid for git sources (`*.git` URL or `url#rev` git form).
    /// Example:
    /// cherry_pick = ["a1b2c3d4", "deadbeef"]
    #[serde(
        default,
        alias = "cherry-pick",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub cherry_pick: Vec<String>,
}

/// Manual source copied before standard source fetching.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ManualSource {
    /// Filename in the spec directory (local manual source mode).
    #[serde(default)]
    pub file: Option<String>,
    /// Multiple filenames in the spec directory (local manual source mode).
    #[serde(default)]
    pub files: Vec<String>,
    /// Remote URL to fetch or clone (remote manual source mode).
    #[serde(default)]
    pub url: Option<String>,
    /// Multiple remote URLs to fetch or clone (remote manual source mode).
    #[serde(default)]
    pub urls: Vec<String>,
    /// Checksum (optional, use "skip" to bypass verification).
    #[serde(default)]
    pub sha256: Option<String>,
    /// Destination path relative to build work directory.
    /// Defaults to `file` for local mode, a derived filename for archive URLs,
    /// or the repository directory name for git URLs.
    #[serde(default)]
    pub dest: Option<String>,
}

fn default_source_sha256() -> String {
    "skip".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrManySources {
    One(Source),
    Many(Vec<Source>),
}

pub(super) fn deserialize_sources<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<Source>, D::Error>
where
    D: Deserializer<'de>,
{
    // Try to deserialize; if the field is missing/null, return empty vec
    let parsed = Option::<OneOrManySources>::deserialize(deserializer)?;
    match parsed {
        Some(OneOrManySources::One(s)) => Ok(vec![s]),
        Some(OneOrManySources::Many(v)) => Ok(v),
        None => Ok(Vec::new()),
    }
}

/// Build configuration
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Build {
    #[serde(rename = "type")]
    pub build_type: BuildType,
    #[serde(default)]
    pub flags: BuildFlags,
}

/// Supported build systems
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum BuildType {
    Autotools,
    CMake,
    Meson,
    Perl,
    Custom,
    Python,
    Rust,
    Makefile,
    Bin,
    Meta,
}

/// Build flags and toolchain configuration
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BuildFlags {
    /// Extra flags exported to `CFLAGS`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub cflags: Vec<String>,
    /// Ordered replacement rules applied to `cflags` before export.
    ///
    /// Each entry may use `old=>new`. Plain `old=new` is also accepted and
    /// disambiguated against the current flag set when possible.
    #[serde(
        default,
        alias = "replace-cflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cflags: Vec<String>,
    /// Extra flags exported to `CFLAGS` only for the lib32 build variant.
    #[serde(
        default,
        alias = "cflags-lib32",
        alias = "cflags_lib32",
        alias = "CFLAGS-lib32",
        alias = "CFLAGS_lib32",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub cflags_lib32: Vec<String>,
    /// Ordered replacement rules applied to lib32-only `cflags`.
    #[serde(
        default,
        alias = "replace-cflags-lib32",
        alias = "replace_cflags-lib32",
        alias = "replace_cflags_lib32",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cflags_lib32: Vec<String>,
    /// Extra flags exported to `CXXFLAGS`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub cxxflags: Vec<String>,
    /// Ordered replacement rules applied to `cxxflags` before export.
    #[serde(
        default,
        alias = "replace-cxxflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cxxflags: Vec<String>,
    /// Extra flags exported to `CXXFLAGS` only for the lib32 build variant.
    #[serde(
        default,
        alias = "cxxflags-lib32",
        alias = "cxxflags_lib32",
        alias = "CXXFLAGS-lib32",
        alias = "CXXFLAGS_lib32",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub cxxflags_lib32: Vec<String>,
    /// Ordered replacement rules applied to lib32-only `cxxflags`.
    #[serde(
        default,
        alias = "replace-cxxflags-lib32",
        alias = "replace_cxxflags-lib32",
        alias = "replace_cxxflags_lib32",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cxxflags_lib32: Vec<String>,
    /// Extra flags exported to `LDFLAGS`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub ldflags: Vec<String>,
    /// Linker selected through compiler drivers with `-fuse-ld=<value>`.
    #[serde(default, alias = "fuse-ld")]
    pub fuse_ld: String,
    /// Ordered replacement rules applied to `ldflags` before export.
    #[serde(
        default,
        alias = "replace-ldflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_ldflags: Vec<String>,
    /// Link-time optimization flags exported to `LTOFLAGS`.
    ///
    /// When `use_lto` is true (default), these flags are also appended to
    /// `CFLAGS`, `CXXFLAGS`, and `LDFLAGS`.
    #[serde(
        default,
        alias = "lto-flags",
        alias = "lto_flags",
        alias = "LTOFLAGS",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub ltoflags: Vec<String>,
    /// Rust LTO flags exported to `RUSTLTOFLAGS`.
    ///
    /// When `use_lto` is true (default), these flags are also appended to
    /// `RUSTFLAGS`.
    #[serde(
        default,
        alias = "rust-ltoflags",
        alias = "rust_ltoflags",
        alias = "RUSTLTOFLAGS",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub rustltoflags: Vec<String>,
    /// Ordered replacement rules applied to `ltoflags` before export/injection.
    #[serde(
        default,
        alias = "replace-ltoflags",
        alias = "replace_lto-flags",
        alias = "replace_lto_flags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_ltoflags: Vec<String>,
    /// Keep existing files and install package-provided replacement as `<path>.depotnew`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub keep: Vec<String>,
    /// Split documentation trees into a derived `<package>-docs` output during staging.
    #[serde(
        default,
        alias = "split-docs",
        deserialize_with = "deserialize_boolish"
    )]
    pub split_docs: bool,
    /// Additional documentation directories to move into `<package>-docs`.
    #[serde(
        default,
        alias = "doc-dirs",
        alias = "doc_dirs",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub doc_dirs: Vec<String>,
    /// Disable automatic LTOFLAGS injection into CFLAGS/CXXFLAGS/LDFLAGS.
    #[serde(
        default = "default_use_lto",
        alias = "use-lto",
        deserialize_with = "deserialize_boolish"
    )]
    pub use_lto: bool,
    /// Disable exporting CFLAGS/CXXFLAGS/LDFLAGS for this package build.
    #[serde(default, alias = "no-flags")]
    pub no_flags: bool,
    /// Disable automatic stripping of ELF files during staging.
    #[serde(default, alias = "no-strip")]
    pub no_strip: bool,
    /// Disable automatic deletion of static libraries (`*.a`) during staging.
    #[serde(
        default,
        alias = "no-delete-static",
        alias = "no_remove_static",
        alias = "no-remove-static"
    )]
    pub no_delete_static: bool,
    /// Disable automatic zstd compression of man pages during staging.
    #[serde(
        default,
        alias = "no-compress-man",
        alias = "no_compress_manpages",
        alias = "no-compress-manpages"
    )]
    pub no_compress_man: bool,
    /// Skip automatic build-system test execution (e.g. Autotools `make check`/`make test`).
    ///
    /// Automatic tests are also skipped for multilib (`build_32` / `lib32_only`) builds.
    #[serde(default, alias = "skip-tests")]
    pub skip_tests: bool,
    /// Run an additional lib32 build pass and emit a `lib32-*` package.
    #[serde(
        default,
        alias = "build-32",
        alias = "build_32",
        deserialize_with = "deserialize_boolish"
    )]
    pub build_32: bool,
    /// Build/install only the generated `lib32-*` companion package output.
    #[serde(
        default,
        alias = "lib32-only",
        alias = "lib32_only",
        deserialize_with = "deserialize_boolish"
    )]
    pub lib32_only: bool,
    /// Perform an additional native host-side helper build when the active target arch differs.
    #[serde(
        default,
        alias = "host-build",
        alias = "host_build",
        deserialize_with = "deserialize_boolish"
    )]
    pub host_build: bool,
    #[serde(default)]
    pub configure: Vec<String>,
    /// Configure arguments appended only when the effective target architecture matches.
    ///
    /// Package specs populate this with append keys such as `configure_x86_64 += ["--enable-sse2"]`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub configure_arch: BTreeMap<String, Vec<String>>,
    /// PEP 517 config settings for Python builds (each entry is `KEY=VALUE` or `KEY`).
    #[serde(
        default,
        alias = "config-setting",
        alias = "config-settings",
        alias = "config_setting",
        alias = "config_settings",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub config_settings: Vec<String>,
    /// Configure arguments used only for the lib32 build variant (replaces `configure` when set).
    #[serde(default, alias = "configure-lib32", alias = "configure_lib32")]
    pub configure_lib32: Vec<String>,
    /// Autotools configure script path, relative to source root or absolute.
    #[serde(default, alias = "configure-file")]
    pub configure_file: String,
    /// Directory containing the configured compiler, linker, and binutils tools.
    #[serde(default, alias = "tool-dir", alias = "tools_dir", alias = "tools-dir")]
    pub tool_dir: String,
    /// C compiler
    #[serde(default = "default_cc")]
    pub cc: String,
    /// C++ compiler
    #[serde(default = "default_cxx")]
    pub cxx: String,
    /// Archiver
    #[serde(default = "default_ar")]
    pub ar: String,
    /// Archive indexer exported as `RANLIB` when configured.
    #[serde(default)]
    pub ranlib: String,
    /// Strip executable exported as `STRIP` when configured.
    #[serde(default)]
    pub strip: String,
    /// Linker executable or linker flavor override for supported builders.
    #[serde(default)]
    pub ld: String,
    /// Symbol table dumper exported as `NM` when configured.
    #[serde(default)]
    pub nm: String,
    /// Object copy tool exported as `OBJCOPY` when configured.
    #[serde(default)]
    pub objcopy: String,
    /// Object dump tool exported as `OBJDUMP` when configured.
    #[serde(default)]
    pub objdump: String,
    /// ELF reader exported as `READELF` when configured.
    #[serde(default)]
    pub readelf: String,
    /// C preprocessor executable exported as `CPP` when configured.
    #[serde(default, alias = "CPP")]
    pub cpp: String,
    /// Dynamic loader path
    #[serde(default)]
    pub libc: String,
    /// Root filesystem for installation (per-package override)
    #[serde(default = "default_rootfs")]
    #[allow(dead_code)]
    pub rootfs: String,
    /// Commands to run after configure/setup step, before compile/build step.
    #[serde(default, alias = "post-configure")]
    pub post_configure: Vec<String>,
    /// Commands to run after configure/setup for the lib32 build variant.
    #[serde(
        default,
        alias = "post-configure-lib32",
        alias = "post_configure-lib32",
        alias = "post_configure_lib32"
    )]
    pub post_configure_lib32: Vec<String>,
    /// Commands to run after compile (after make, before make install).
    #[serde(default, alias = "post-compile")]
    pub post_compile: Vec<String>,
    /// Commands to run after compile for the lib32 build variant.
    #[serde(
        default,
        alias = "post-compile-lib32",
        alias = "post_compile-lib32",
        alias = "post_compile_lib32"
    )]
    pub post_compile_lib32: Vec<String>,
    /// Commands to run after install (after make install)
    #[serde(default, alias = "post-install")]
    pub post_install: Vec<String>,
    /// Commands to run after the lib32 install step (replaces `post_install` when set).
    #[serde(
        default,
        alias = "post-install-lib32",
        alias = "post_install-lib32",
        alias = "post_install_lib32"
    )]
    pub post_install_lib32: Vec<String>,

    /// Specific commands for 'makefile' build type
    #[serde(default)]
    pub makefile_commands: Vec<String>,
    #[serde(default)]
    pub makefile_install_commands: Vec<String>,

    /// Installation prefix (default: /usr)
    #[serde(default = "default_prefix")]
    pub prefix: String,

    /// Target architecture triple (CHOST equivalent)
    #[serde(default)]
    pub chost: String,

    /// Build architecture triple (CBUILD equivalent)
    #[serde(default)]
    pub cbuild: String,

    /// CPU architecture short name (CARCH equivalent), e.g. "x86_64", "aarch64"
    #[serde(default = "default_carch")]
    pub carch: String,
    /// MAKEFLAGS environment variable passed to build commands.
    #[serde(
        default,
        alias = "make-flags",
        alias = "make_flags",
        alias = "MAKEFLAGS",
        deserialize_with = "deserialize_string_or_array_joined"
    )]
    pub makeflags: String,
    /// Variable overrides passed directly to `make` (compile step), e.g. ["V=1", "CC=clang"].
    #[serde(
        default,
        alias = "make-vars",
        alias = "make_build_vars",
        alias = "make-build-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_vars: Vec<String>,
    /// Make-like executable for build/test/install phases (default: `make`), e.g. `ninja`.
    #[serde(default, alias = "make-exec")]
    pub make_exec: String,
    /// Target for the compile/build phase (e.g. `all`, `bootstrap`).
    #[serde(
        default,
        alias = "make-target",
        alias = "make_build_target",
        alias = "make-build-target"
    )]
    pub make_target: String,
    /// Targets for the compile/build phase (e.g. `["all", "bootstrap"]`).
    #[serde(
        default,
        alias = "make-targets",
        alias = "make_build_targets",
        alias = "make-build-targets",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_targets: Vec<String>,
    /// Subdirectories (relative to build directory) where `make` should run.
    #[serde(
        default,
        alias = "make-dirs",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_dirs: Vec<String>,
    /// Variable overrides passed directly to `make check` / `make test`.
    #[serde(
        default,
        alias = "make-test-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_test_vars: Vec<String>,
    /// Target for the test phase, passed to the make-like executable.
    #[serde(default, alias = "make-test-target")]
    pub make_test_target: String,
    /// Targets for the test phase, passed to the make-like executable.
    #[serde(
        default,
        alias = "make-test-targets",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_test_targets: Vec<String>,
    /// Subdirectories (relative to build directory) where test targets should run.
    #[serde(
        default,
        alias = "make-test-dirs",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_test_dirs: Vec<String>,
    /// Variable overrides passed directly to `make install`.
    #[serde(
        default,
        alias = "make-install-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_install_vars: Vec<String>,
    /// Target for the install phase (default: `install`).
    #[serde(default, alias = "make-install-target")]
    pub make_install_target: String,
    /// Targets for the install phase.
    #[serde(
        default,
        alias = "make-install-targets",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_install_targets: Vec<String>,
    /// Subdirectories (relative to build directory) where `make install` should run.
    #[serde(
        default,
        alias = "make-install-dirs",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_install_dirs: Vec<String>,
    /// Additional host environment variable names to export unchanged to build commands.
    /// Example: ["RUSTFLAGS", "CARGO_HOME"].
    #[serde(
        default,
        alias = "passthrough-env",
        alias = "pass_env",
        alias = "pass-env",
        alias = "export_env",
        alias = "export-env",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub passthrough_env: Vec<String>,
    /// Explicit environment variable assignments exported to build commands.
    /// Each entry must be `KEY=VALUE`.
    #[serde(
        default,
        alias = "env-vars",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub env_vars: Vec<String>,

    // Rust-specific fields
    /// Rust build profile: "debug" or "release" (default: release)
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Rust target triple (e.g., x86_64-unknown-linux-musl). Optional.
    #[serde(default)]
    pub target: String,
    /// RUSTFLAGS environment variable
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub rustflags: Vec<String>,
    /// Ordered replacement rules applied to `rustflags` before export.
    #[serde(
        default,
        alias = "replace-rustflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_rustflags: Vec<String>,
    /// Additional cargo arguments (short name)
    #[serde(default)]
    pub cargs: Vec<String>,
    /// Binary installation directory relative to DESTDIR (default: /usr/bin)
    #[serde(default = "default_bindir")]
    pub bindir: String,
    /// System binary installation directory for supported builders (default: /usr/bin).
    #[serde(default)]
    pub sbindir: String,
    /// Library installation directory for supported builders.
    ///
    /// Defaults to `/usr/lib`, or `/usr/lib32` for the lib32 build variant.
    #[serde(default)]
    pub libdir: String,
    /// Library helper executable installation directory for supported builders.
    ///
    /// Defaults to the effective `libdir`.
    #[serde(default)]
    pub libexecdir: String,
    /// System configuration directory for supported builders (default: /etc).
    #[serde(default)]
    pub sysconfdir: String,
    /// Variable state directory for supported builders (default: /var).
    #[serde(default)]
    pub localstatedir: String,
    /// Shared variable state directory for supported builders (default: /var/lib).
    #[serde(default)]
    pub sharedstatedir: String,
    /// Header installation directory for supported builders (default: /usr/include).
    #[serde(default)]
    pub includedir: String,
    /// Data root installation directory for supported builders (default: /usr/share).
    #[serde(default)]
    pub datarootdir: String,
    /// Architecture-independent data installation directory for supported builders.
    ///
    /// Defaults to the effective `datarootdir`.
    #[serde(default)]
    pub datadir: String,
    /// Manual page installation directory for supported builders (default: /usr/share/man).
    #[serde(default)]
    pub mandir: String,
    /// Info page installation directory for supported builders (default: /usr/share/info).
    #[serde(default)]
    pub infodir: String,

    /// Subdirectory within extracted source to use as the actual source root.
    /// Useful for monorepos like llvm-project where you want to build just one component.
    #[serde(default)]
    pub source_subdir: String,
    /// Build directory relative to source root (e.g. "build")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_dir: Option<String>,
    /// Binary package type when using BuildType::Bin (e.g. "deb")
    #[serde(default)]
    pub binary_type: String,
    /// Internal runtime marker used to adjust builder behavior for the lib32 variant.
    #[serde(skip)]
    pub lib32_variant: bool,
    /// Internal runtime marker containing the absolute path to the native host helper build dir.
    #[serde(skip)]
    pub host_build_dir: Option<String>,
}

impl Default for BuildFlags {
    fn default() -> Self {
        BuildFlags {
            cflags: Vec::new(),
            replace_cflags: Vec::new(),
            cflags_lib32: Vec::new(),
            replace_cflags_lib32: Vec::new(),
            cxxflags: Vec::new(),
            replace_cxxflags: Vec::new(),
            cxxflags_lib32: Vec::new(),
            replace_cxxflags_lib32: Vec::new(),
            ldflags: Vec::new(),
            fuse_ld: String::new(),
            replace_ldflags: Vec::new(),
            ltoflags: Vec::new(),
            rustltoflags: Vec::new(),
            replace_ltoflags: Vec::new(),
            keep: Vec::new(),
            split_docs: false,
            doc_dirs: Vec::new(),
            use_lto: default_use_lto(),
            no_flags: false,
            no_strip: false,
            no_delete_static: false,
            no_compress_man: false,
            skip_tests: false,
            build_32: false,
            lib32_only: false,
            host_build: false,
            configure: Vec::new(),
            configure_arch: BTreeMap::new(),
            config_settings: Vec::new(),
            configure_lib32: Vec::new(),
            configure_file: String::new(),
            tool_dir: String::new(),
            cc: default_cc(),
            cxx: default_cxx(),
            ar: default_ar(),
            ranlib: String::new(),
            strip: String::new(),
            ld: String::new(),
            nm: String::new(),
            objcopy: String::new(),
            objdump: String::new(),
            readelf: String::new(),
            cpp: String::new(),
            libc: String::new(),
            rootfs: default_rootfs(),
            post_configure: Vec::new(),
            post_configure_lib32: Vec::new(),
            post_compile: Vec::new(),
            post_compile_lib32: Vec::new(),
            post_install: Vec::new(),
            post_install_lib32: Vec::new(),
            makefile_commands: Vec::new(),
            makefile_install_commands: Vec::new(),
            prefix: default_prefix(),
            chost: String::new(),
            cbuild: String::new(),
            carch: default_carch(),
            makeflags: String::new(),
            make_vars: Vec::new(),
            make_exec: String::new(),
            make_target: String::new(),
            make_targets: Vec::new(),
            make_dirs: Vec::new(),
            make_test_vars: Vec::new(),
            make_test_target: String::new(),
            make_test_targets: Vec::new(),
            make_test_dirs: Vec::new(),
            make_install_vars: Vec::new(),
            make_install_target: String::new(),
            make_install_targets: Vec::new(),
            make_install_dirs: Vec::new(),
            passthrough_env: Vec::new(),
            env_vars: Vec::new(),
            profile: default_profile(),
            target: String::new(),
            rustflags: Vec::new(),
            replace_rustflags: Vec::new(),
            cargs: Vec::new(),
            bindir: default_bindir(),
            sbindir: String::new(),
            libdir: String::new(),
            libexecdir: String::new(),
            sysconfdir: String::new(),
            localstatedir: String::new(),
            sharedstatedir: String::new(),
            includedir: String::new(),
            datarootdir: String::new(),
            datadir: String::new(),
            mandir: String::new(),
            infodir: String::new(),
            source_subdir: String::new(),
            build_dir: None,
            binary_type: String::new(),
            lib32_variant: false,
            host_build_dir: None,
        }
    }
}

fn deserialize_string_or_array<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrArray {
        String(String),
        Array(Vec<String>),
    }

    match Option::<StringOrArray>::deserialize(deserializer)? {
        Some(StringOrArray::String(s)) => Ok(s.split_whitespace().map(String::from).collect()),
        Some(StringOrArray::Array(a)) => Ok(a),
        None => Ok(Vec::new()),
    }
}

fn deserialize_string_or_array_no_split<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrArray {
        String(String),
        Array(Vec<String>),
    }

    match Option::<StringOrArray>::deserialize(deserializer)? {
        Some(StringOrArray::String(s)) => Ok(vec![s]),
        Some(StringOrArray::Array(a)) => Ok(a),
        None => Ok(Vec::new()),
    }
}

fn deserialize_string_or_array_joined<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrArray {
        String(String),
        Array(Vec<String>),
    }

    match Option::<StringOrArray>::deserialize(deserializer)? {
        Some(StringOrArray::String(s)) => Ok(s),
        Some(StringOrArray::Array(a)) => Ok(a
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")),
        None => Ok(String::new()),
    }
}

fn deserialize_boolish<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Boolish {
        Bool(bool),
        String(String),
    }

    match Option::<Boolish>::deserialize(deserializer)? {
        Some(Boolish::Bool(v)) => Ok(v),
        Some(Boolish::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "expected boolean string for lib32 flag, got '{}'",
                other
            ))),
        },
        None => Ok(false),
    }
}

pub(super) fn toml_value_as_boolish(value: &toml::Value) -> Option<bool> {
    if let Some(b) = value.as_bool() {
        return Some(b);
    }
    value
        .as_str()
        .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        })
}

pub(super) fn append_whitespace_separated(dst: &mut String, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    if dst.is_empty() {
        dst.push_str(trimmed);
    } else {
        dst.push(' ');
        dst.push_str(trimmed);
    }
}

fn default_cc() -> String {
    // Prefer clang if available (supports -print-resource-dir and other useful flags)
    if std::process::Command::new("which")
        .arg("clang")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return "clang".to_string();
    }
    "gcc".to_string()
}

fn default_use_lto() -> bool {
    true
}

fn default_ar() -> String {
    "ar".to_string()
}

fn default_rootfs() -> String {
    "/".to_string()
}

fn default_profile() -> String {
    "release".to_string()
}

fn default_bindir() -> String {
    "/usr/bin".to_string()
}

fn default_prefix() -> String {
    "/usr".to_string()
}

fn default_carch() -> String {
    std::env::consts::ARCH.to_string()
}

fn default_cxx() -> String {
    // Infer a sensible C++ compiler name from default_cc()
    let cc = default_cc();
    if cc.contains("clang") {
        "clang++".to_string()
    } else {
        "g++".to_string()
    }
}

/// Nested dependency override group for a specific output variant.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct DependencyGroup {
    /// Dependencies required for building packages.
    #[serde(default)]
    pub build: Vec<String>,
    /// Dependencies required at runtime.
    #[serde(default)]
    pub runtime: Vec<String>,
    /// Dependencies required to run package test suites.
    #[serde(default)]
    pub test: Vec<String>,
    /// Optional runtime integrations that enhance functionality when installed.
    #[serde(default)]
    pub optional: Vec<String>,
    /// Package groups associated with this package output.
    #[serde(default)]
    pub groups: Vec<String>,
}

impl DependencyGroup {
    fn to_dependencies(&self) -> Dependencies {
        Dependencies {
            build: self.build.clone(),
            runtime: self.runtime.clone(),
            test: self.test.clone(),
            optional: self.optional.clone(),
            groups: self.groups.clone(),
            lib32: None,
        }
    }
}

/// Package dependencies
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Dependencies {
    /// Dependencies required for building packages.
    #[serde(default)]
    pub build: Vec<String>,
    /// Dependencies required at runtime.
    #[serde(default)]
    pub runtime: Vec<String>,
    /// Dependencies required to run package test suites.
    #[serde(default)]
    pub test: Vec<String>,
    /// Optional runtime integrations that enhance functionality when installed.
    #[serde(default)]
    pub optional: Vec<String>,
    /// Package groups associated with this package.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Optional dependency overrides used only for the generated `lib32-*` companion package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lib32: Option<DependencyGroup>,
}

impl Dependencies {
    /// Return the top-level dependency set without any nested output-specific overrides.
    pub fn primary_dependencies(&self) -> Dependencies {
        Dependencies {
            build: self.build.clone(),
            runtime: self.runtime.clone(),
            test: self.test.clone(),
            optional: self.optional.clone(),
            groups: self.groups.clone(),
            lib32: None,
        }
    }

    /// Return the optional lib32-specific dependency override set.
    pub fn lib32_dependencies(&self) -> Option<Dependencies> {
        self.lib32.as_ref().map(DependencyGroup::to_dependencies)
    }
}
