//! Build system abstraction

mod autotools;
mod bin;
mod cmake;
mod custom;
mod makefile;
mod meson;
mod python;
mod rust;
pub mod state;

use crate::cross::CrossConfig;
use crate::package::{BuildType, PackageSpec};
use anyhow::Result;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

pub type EnvVars = Vec<(String, String)>;

pub fn set_env_var(env_vars: &mut EnvVars, key: &str, value: impl Into<String>) {
    let value = value.into();
    if let Some((_, existing)) = env_vars.iter_mut().find(|(k, _)| k == key) {
        *existing = value;
    } else {
        env_vars.push((key.to_string(), value));
    }
}

pub fn standard_build_env(
    spec: &PackageSpec,
    cross: Option<&CrossConfig>,
    include_compiler_env: bool,
    export_compiler_flags: bool,
) -> EnvVars {
    let flags = &spec.build.flags;
    let mut env_vars: EnvVars = Vec::new();
    let export_compiler_flags = export_compiler_flags && !flags.no_flags;

    if include_compiler_env && export_compiler_flags {
        if !flags.cflags.is_empty() {
            set_env_var(&mut env_vars, "CFLAGS", flags.cflags.join(" "));
        }
        if !flags.cxxflags.is_empty() {
            set_env_var(&mut env_vars, "CXXFLAGS", flags.cxxflags.join(" "));
        }

        let ldflags = if !flags.ldflags.is_empty() || !flags.libc.is_empty() {
            if flags.libc.is_empty() {
                flags.ldflags.join(" ")
            } else if flags.ldflags.is_empty() {
                format!("-Wl,--dynamic-linker={}", flags.libc)
            } else {
                format!(
                    "{} -Wl,--dynamic-linker={}",
                    flags.ldflags.join(" "),
                    flags.libc
                )
            }
        } else {
            String::new()
        };
        if !ldflags.is_empty() {
            set_env_var(&mut env_vars, "LDFLAGS", ldflags);
        }
    }

    if !flags.chost.is_empty() {
        set_env_var(&mut env_vars, "CHOST", flags.chost.clone());
    }
    if !flags.cbuild.is_empty() {
        set_env_var(&mut env_vars, "CBUILD", flags.cbuild.clone());
    }
    if !flags.carch.is_empty() {
        set_env_var(&mut env_vars, "CARCH", flags.carch.clone());
    }
    if !flags.prefix.is_empty() {
        set_env_var(&mut env_vars, "PREFIX", flags.prefix.clone());
    }

    set_env_var(&mut env_vars, "DEPOT_ROOTFS", flags.rootfs.clone());
    set_env_var(
        &mut env_vars,
        "DEPOT_SPECDIR",
        spec.spec_dir.to_string_lossy().into_owned(),
    );

    if include_compiler_env {
        if let Some(cc_cfg) = cross {
            set_env_var(&mut env_vars, "CC", cc_cfg.cc.clone());
            set_env_var(&mut env_vars, "CXX", cc_cfg.cxx.clone());
            set_env_var(&mut env_vars, "AR", cc_cfg.ar.clone());
            set_env_var(&mut env_vars, "RANLIB", cc_cfg.ranlib.clone());
            set_env_var(&mut env_vars, "STRIP", cc_cfg.strip.clone());
            set_env_var(&mut env_vars, "LD", cc_cfg.ld.clone());
            set_env_var(&mut env_vars, "NM", cc_cfg.nm.clone());
            set_env_var(&mut env_vars, "CROSS_PREFIX", cc_cfg.prefix.clone());
            set_env_var(
                &mut env_vars,
                "CROSS_COMPILE",
                format!("{}-", cc_cfg.prefix),
            );
        } else {
            set_env_var(&mut env_vars, "CC", flags.cc.clone());
            set_env_var(&mut env_vars, "CXX", flags.cxx.clone());
            set_env_var(&mut env_vars, "AR", flags.ar.clone());
        }
    }

    for key in &flags.passthrough_env {
        let key = key.trim();
        if key.is_empty() || key.contains('=') {
            continue;
        }
        if env_vars.iter().any(|(existing, _)| existing == key) {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            set_env_var(&mut env_vars, key, value);
        }
    }

    env_vars
}

/// Prepare a Command with a hermetic environment and some essential variables preserved.
pub fn prepare_command(cmd: &mut Command, env_vars: &EnvVars) {
    cmd.env_clear();

    if let Some(path) = sanitized_build_path() {
        cmd.env("PATH", path);
    }

    // Preserve essential environment variables
    for var in &[
        "LANG",
        "HOME",
        "DESTDIR",
        "DEPOT_ROOTFS",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "RUSTC",
        "RUSTDOC",
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "FORCE_COLOR",
    ] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    // Use a deterministic POSIX shell for build tooling. Inheriting an
    // interactive shell (e.g. zsh) can make Autotools-generated scripts
    // produce non-reproducible or incompatible shell fragments.
    cmd.env("SHELL", "/bin/sh");
    // Set requested environment variables
    for (key, val) in env_vars {
        cmd.env(key, val);
    }
}

fn sanitized_build_path() -> Option<OsString> {
    use std::path::PathBuf;

    let mut parts: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|raw| std::env::split_paths(&raw).collect())
        .unwrap_or_default();

    for dir in ["/bin", "/usr/bin", "/sbin", "/usr/sbin"] {
        let path = PathBuf::from(dir);
        if path.exists() && !parts.iter().any(|p| p == &path) {
            parts.push(path);
        }
    }

    if parts.is_empty() {
        return None;
    }

    std::env::join_paths(parts).ok()
}

/// Prepare a Command for interactive tool execution with live terminal output.
pub fn prepare_tool_command(cmd: &mut Command, env_vars: &EnvVars) {
    prepare_command(cmd, env_vars);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
}

/// Build a package using the appropriate build system
pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
) -> Result<()> {
    if let Some(cc) = cross {
        crate::log_info!(
            "Cross-compiling for {} with {:?}...",
            cc.prefix,
            spec.build.build_type
        );
    } else {
        crate::log_info!("Building with {:?}...", spec.build.build_type);
    }

    // Clean destdir to prevent stale files/directories (e.g., directories where symlinks should be)
    if destdir.exists() {
        std::fs::remove_dir_all(destdir)?;
    }

    match spec.build.build_type {
        BuildType::Autotools => {
            autotools::build(spec, src_dir, destdir, cross, export_compiler_flags)
        }
        BuildType::CMake => cmake::build(spec, src_dir, destdir, cross, export_compiler_flags),
        BuildType::Meson => meson::build(spec, src_dir, destdir, cross, export_compiler_flags),
        BuildType::Custom => custom::build(spec, src_dir, destdir, cross, export_compiler_flags),
        BuildType::Python => python::build(spec, src_dir, destdir, cross, export_compiler_flags),
        BuildType::Rust => rust::build(spec, src_dir, destdir, cross, export_compiler_flags),
        BuildType::Bin => bin::build(spec, src_dir, destdir, cross, export_compiler_flags),
        BuildType::Meta => {
            // Metapackages are metadata-only; create an empty staging root and let
            // packaging/installation metadata carry dependencies.
            std::fs::create_dir_all(destdir)?;
            Ok(())
        }
        BuildType::Makefile => {
            makefile::build(spec, src_dir, destdir, cross, export_compiler_flags)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn mk_spec(cflags: Vec<&str>, ldflags: Vec<&str>) -> PackageSpec {
        let mut flags = BuildFlags::default();
        flags.cflags = cflags.into_iter().map(String::from).collect();
        flags.ldflags = ldflags.into_iter().map(String::from).collect();
        PackageSpec {
            package: PackageInfo {
                name: "env-test".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "https://example.test/src.tar.gz".into(),
                sha256: "abc".into(),
                extract_dir: "src".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags,
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn test_prepare_command() {
        let mut cmd = Command::new("ls");
        // Set an env var that should be cleared
        cmd.env("FORBIDDEN", "value");
        // Set PATH manually in the current process to ensure it's picked up if it exists
        unsafe {
            std::env::set_var("PATH", "/usr/bin");
            std::env::set_var("HOME", "/home/test");
            std::env::set_var("SHELL", "/bin/zsh");
            std::env::set_var("DEPOT_ROOTFS", "/my/rootfs");
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("CLICOLOR_FORCE", "1");
        }

        prepare_command(&mut cmd, &vec![("MYVAR".to_string(), "myval".to_string())]);

        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert!(envs.get(OsStr::new("PATH")).is_some());
        assert!(envs.get(OsStr::new("HOME")).is_some());
        assert!(envs.get(OsStr::new("FORBIDDEN")).is_none());
        assert_eq!(
            envs.get(OsStr::new("SHELL")),
            Some(&Some(std::ffi::OsString::from("/bin/sh").as_os_str()))
        );
        assert_eq!(
            envs.get(OsStr::new("MYVAR")),
            Some(&Some(std::ffi::OsString::from("myval").as_os_str()))
        );
        // DEPOT_ROOTFS should be preserved from the parent environment
        assert_eq!(
            envs.get(OsStr::new("DEPOT_ROOTFS")),
            Some(&Some(std::ffi::OsString::from("/my/rootfs").as_os_str()))
        );
        assert_eq!(
            envs.get(OsStr::new("TERM")),
            Some(&Some(
                std::ffi::OsString::from("xterm-256color").as_os_str()
            ))
        );
        assert_eq!(
            envs.get(OsStr::new("CLICOLOR_FORCE")),
            Some(&Some(std::ffi::OsString::from("1").as_os_str()))
        );
    }

    #[test]
    fn test_prepare_command_preserves_destdir() {
        let mut cmd = std::process::Command::new("ls");
        unsafe {
            std::env::set_var("DESTDIR", "/tmp/dest");
        }
        prepare_command(&mut cmd, &Vec::new());
        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(OsStr::new("DESTDIR")),
            Some(&Some(std::ffi::OsString::from("/tmp/dest").as_os_str()))
        );
    }

    #[test]
    fn test_prepare_command_preserves_rust_toolchain_homes() {
        let mut cmd = std::process::Command::new("ls");
        unsafe {
            std::env::set_var("CARGO_HOME", "/var/cache/cargo-home");
            std::env::set_var("RUSTUP_HOME", "/var/cache/rustup-home");
        }
        prepare_command(&mut cmd, &Vec::new());
        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(OsStr::new("CARGO_HOME")),
            Some(&Some(
                std::ffi::OsString::from("/var/cache/cargo-home").as_os_str()
            ))
        );
        assert_eq!(
            envs.get(OsStr::new("RUSTUP_HOME")),
            Some(&Some(
                std::ffi::OsString::from("/var/cache/rustup-home").as_os_str()
            ))
        );
    }

    #[test]
    fn test_standard_build_env_respects_export_compiler_flags_toggle() {
        let mut spec = mk_spec(vec!["-O2"], vec!["-Wl,--as-needed"]);
        spec.build.flags.cxxflags = vec!["-O2".into(), "-fno-exceptions".into()];

        let enabled = standard_build_env(&spec, None, true, true);
        assert!(
            enabled.iter().any(|(k, v)| k == "CFLAGS" && v == "-O2"),
            "expected CFLAGS to be exported when enabled"
        );
        assert!(
            enabled
                .iter()
                .any(|(k, v)| k == "CXXFLAGS" && v == "-O2 -fno-exceptions"),
            "expected CXXFLAGS to be exported when enabled"
        );
        assert!(
            enabled
                .iter()
                .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,--as-needed"),
            "expected LDFLAGS to be exported when enabled"
        );

        let disabled = standard_build_env(&spec, None, true, false);
        assert!(
            !disabled.iter().any(|(k, _)| k == "CFLAGS"),
            "expected CFLAGS to be omitted when disabled"
        );
        assert!(
            !disabled.iter().any(|(k, _)| k == "CXXFLAGS"),
            "expected CXXFLAGS to be omitted when disabled"
        );
        assert!(
            !disabled.iter().any(|(k, _)| k == "LDFLAGS"),
            "expected LDFLAGS to be omitted when disabled"
        );

        let mut disabled_by_spec = spec.clone();
        disabled_by_spec.build.flags.no_flags = true;
        let disabled_env = standard_build_env(&disabled_by_spec, None, true, true);
        assert!(
            !disabled_env.iter().any(|(k, _)| k == "CFLAGS"),
            "expected CFLAGS to be omitted when no_flags is set in spec"
        );
        assert!(
            !disabled_env.iter().any(|(k, _)| k == "CXXFLAGS"),
            "expected CXXFLAGS to be omitted when no_flags is set in spec"
        );
        assert!(
            !disabled_env.iter().any(|(k, _)| k == "LDFLAGS"),
            "expected LDFLAGS to be omitted when no_flags is set in spec"
        );
    }

    #[test]
    fn test_standard_build_env_exports_passthrough_env() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.passthrough_env = vec!["RUSTFLAGS".into()];

        unsafe {
            std::env::set_var("RUSTFLAGS", "-C target-cpu=native");
        }

        let env = standard_build_env(&spec, None, false, true);
        assert!(
            env.iter()
                .any(|(k, v)| k == "RUSTFLAGS" && v == "-C target-cpu=native"),
            "expected RUSTFLAGS to be copied from parent environment"
        );
    }

    #[test]
    fn test_standard_build_env_passthrough_does_not_override_default_vars() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.cc = "spec-cc".to_string();
        spec.build.flags.passthrough_env = vec!["CC".into()];

        unsafe {
            std::env::set_var("CC", "host-cc");
        }

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter().any(|(k, v)| k == "CC" && v == "spec-cc"),
            "expected default CC to take precedence over passthrough CC"
        );
    }
}
