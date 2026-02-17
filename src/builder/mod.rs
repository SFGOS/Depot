//! Build system abstraction

mod autotools;
mod bin;
mod cmake;
mod custom;
mod makefile;
mod meson;
mod rust;
pub mod state;

use crate::cross::CrossConfig;
use crate::package::{BuildType, PackageSpec};
use anyhow::Result;
use std::path::Path;
use std::process::Command;

/// Prepare a Command with a hermetic environment and some essential variables preserved.
pub fn prepare_command(cmd: &mut Command, env_vars: &[(&str, String)]) {
    cmd.env_clear();
    // Preserve essential environment variables
    for var in &["PATH", "LANG", "HOME", "SHELL", "DESTDIR", "DEPOT_ROOTFS"] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    // Set requested environment variables
    for (key, val) in env_vars {
        cmd.env(key, val);
    }
}

/// Build a package using the appropriate build system
pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
) -> Result<()> {
    if let Some(cc) = cross {
        println!(
            "Cross-compiling for {} with {:?}...",
            cc.prefix, spec.build.build_type
        );
    } else {
        println!("Building with {:?}...", spec.build.build_type);
    }

    // Clean destdir to prevent stale files/directories (e.g., directories where symlinks should be)
    if destdir.exists() {
        std::fs::remove_dir_all(destdir)?;
    }

    match spec.build.build_type {
        BuildType::Autotools => autotools::build(spec, src_dir, destdir, cross),
        BuildType::CMake => cmake::build(spec, src_dir, destdir, cross),
        BuildType::Meson => meson::build(spec, src_dir, destdir, cross),
        BuildType::Custom => custom::build(spec, src_dir, destdir, cross),
        BuildType::Rust => rust::build(spec, src_dir, destdir, cross),
        BuildType::Bin => bin::build(spec, src_dir, destdir, cross),
        BuildType::Makefile => makefile::build(spec, src_dir, destdir, cross),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepare_command() {
        let mut cmd = Command::new("ls");
        // Set an env var that should be cleared
        cmd.env("FORBIDDEN", "value");
        // Set PATH manually in the current process to ensure it's picked up if it exists
        unsafe {
            std::env::set_var("PATH", "/usr/bin");
            std::env::set_var("HOME", "/home/test");
            std::env::set_var("DEPOT_ROOTFS", "/my/rootfs");
        }

        prepare_command(&mut cmd, &[("MYVAR", "myval".to_string())]);

        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert!(envs.get(std::ffi::OsStr::new("PATH")).is_some());
        assert!(envs.get(std::ffi::OsStr::new("HOME")).is_some());
        assert!(envs.get(std::ffi::OsStr::new("FORBIDDEN")).is_none());
        assert_eq!(
            envs.get(std::ffi::OsStr::new("MYVAR")),
            Some(&Some(std::ffi::OsString::from("myval").as_os_str()))
        );
        // DEPOT_ROOTFS should be preserved from the parent environment
        assert_eq!(
            envs.get(std::ffi::OsStr::new("DEPOT_ROOTFS")),
            Some(&Some(std::ffi::OsString::from("/my/rootfs").as_os_str()))
        );
    }

    #[test]
    fn test_prepare_command_preserves_destdir() {
        let mut cmd = std::process::Command::new("ls");
        unsafe {
            std::env::set_var("DESTDIR", "/tmp/dest");
        }
        prepare_command(&mut cmd, &[]);
        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("DESTDIR")),
            Some(&Some(std::ffi::OsString::from("/tmp/dest").as_os_str()))
        );
    }
}
