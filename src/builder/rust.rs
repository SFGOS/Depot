//! Rust/Cargo build system

use crate::cross::CrossConfig;
use crate::package::PackageSpec;
use crate::source::hooks;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
) -> Result<()> {
    let flags = &spec.build.flags;

    // Create destdir
    fs::create_dir_all(destdir)?;

    // Isolate from parent workspace by adding empty [workspace] if not present
    // This prevents "believes it's in a workspace when it's not" errors
    let cargo_toml = src_dir.join("Cargo.toml");
    if cargo_toml.exists() {
        let contents = fs::read_to_string(&cargo_toml)
            .with_context(|| format!("Failed to read {}", cargo_toml.display()))?;
        if !contents.contains("[workspace]") {
            let isolated = format!("{}\n\n[workspace]\n", contents);
            fs::write(&cargo_toml, isolated).with_context(|| {
                format!("Failed to isolate Cargo.toml: {}", cargo_toml.display())
            })?;
        }
    }

    // Determine profile
    let is_release = flags.profile == "release";
    let profile_dir = if is_release { "release" } else { "debug" };

    // Determine target triple
    let target = if !flags.target.is_empty() {
        Some(flags.target.clone())
    } else {
        cross.map(|cc_cfg| cc_cfg.prefix.clone())
    };

    // Build environment
    let mut env_vars =
        crate::builder::standard_build_env(spec, cross, false, export_compiler_flags);

    // RUSTFLAGS
    if !flags.rustflags.is_empty() {
        crate::builder::set_env_var(&mut env_vars, "RUSTFLAGS", flags.rustflags.join(" "));
    }

    // If cross-compiling, set linker via CARGO_TARGET_*_LINKER
    if let Some(cc_cfg) = cross {
        // Convert target triple to uppercase with underscores for env var
        let target_env = target
            .as_ref()
            .unwrap_or(&cc_cfg.prefix)
            .to_uppercase()
            .replace('-', "_");
        let linker_var = format!("CARGO_TARGET_{}_LINKER", target_env);
        crate::builder::set_env_var(&mut env_vars, &linker_var, cc_cfg.cc.clone());
        crate::builder::set_env_var(&mut env_vars, "CC", cc_cfg.cc.clone());
        crate::builder::set_env_var(&mut env_vars, "AR", cc_cfg.ar.clone());
    }

    // Set default rustup toolchain if not already set
    if std::env::var("RUSTUP_TOOLCHAIN").is_err() {
        crate::builder::set_env_var(&mut env_vars, "RUSTUP_TOOLCHAIN", "stable");
    }

    hooks::run_post_configure_commands(spec, src_dir, destdir)?;

    // Run cargo build
    crate::log_info!(
        "Running cargo build ({})...",
        if is_release { "release" } else { "debug" }
    );
    let mut cargo_cmd = Command::new("cargo");
    cargo_cmd.current_dir(src_dir);
    cargo_cmd.arg("build");

    if is_release {
        cargo_cmd.arg("--release");
    }

    // Add target if specified
    if let Some(ref t) = target {
        cargo_cmd.arg("--target").arg(t);
    }

    // Add additional cargo args
    for arg in &flags.cargs {
        cargo_cmd.arg(arg);
    }

    // Set environment
    crate::builder::prepare_command(&mut cargo_cmd, &env_vars);

    let status = cargo_cmd
        .status()
        .with_context(|| format!("Failed to run cargo build for {}", spec.package.name))?;
    if !status.success() {
        anyhow::bail!("cargo build failed");
    }

    // Run post-compile hooks
    hooks::run_post_compile_commands(spec, src_dir, destdir)?;

    // Install binaries to destdir
    crate::log_info!("Installing binaries to DESTDIR...");

    // Determine target directory
    let target_dir = if let Some(ref t) = target {
        src_dir.join("target").join(t).join(profile_dir)
    } else {
        src_dir.join("target").join(profile_dir)
    };

    // Use bindir from flags (default: /usr/bin)
    let bin_dir = destdir.join(flags.bindir.trim_start_matches('/'));
    fs::create_dir_all(&bin_dir)?;

    // Find and copy executable files
    if target_dir.exists() {
        for entry in fs::read_dir(&target_dir)
            .with_context(|| format!("Failed to read target directory: {}", target_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();

            // Skip directories and non-executable files
            if path.is_dir() {
                continue;
            }

            // Check if it's an executable (no extension on Linux, or special check)
            let file_name = path.file_name().unwrap().to_string_lossy();

            // Skip common non-binary files
            if file_name.ends_with(".d")
                || file_name.ends_with(".rlib")
                || file_name.ends_with(".rmeta")
                || file_name.contains(".so")
                || file_name.starts_with("lib")
            {
                continue;
            }

            // Check if file is executable
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if path
                    .metadata()
                    .ok()
                    .filter(|m| m.permissions().mode() & 0o111 != 0 && m.is_file())
                    .is_some()
                {
                    let dest = bin_dir.join(&*file_name);
                    crate::log_info!("  Installing: {}", file_name);
                    fs::copy(&path, &dest).with_context(|| {
                        format!("Failed to copy {} to {}", path.display(), dest.display())
                    })?;

                    // Preserve executable permission
                    let mut perms = fs::metadata(&dest)?.permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&dest, perms)?;
                }
            }
        }
    }

    // Run post-install hooks
    hooks::run_post_install_commands(spec, src_dir, destdir)?;

    Ok(())
}
