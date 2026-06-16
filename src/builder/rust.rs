//! Rust/Cargo build system

use crate::cross::CrossConfig;
use crate::package::PackageSpec;
use crate::source::hooks;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
    _host_build_dir: Option<&Path>,
) -> Result<()> {
    let flags = &spec.build.flags;
    let actual_src = resolve_actual_src(spec, src_dir)?;

    // Create destdir
    fs::create_dir_all(destdir)?;

    // Isolate from parent workspace by adding empty [workspace] if not present
    // This prevents "believes it's in a workspace when it's not" errors
    let cargo_toml = actual_src.join("Cargo.toml");
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
    let rustflags = crate::builder::effective_rustflags(flags);
    if !rustflags.is_empty() {
        crate::builder::set_env_var(&mut env_vars, "RUSTFLAGS", rustflags.join(" "));
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

    hooks::run_post_configure_commands(spec, &actual_src, destdir)?;

    // Run cargo build
    crate::log_info!(
        "Running cargo build ({})...",
        if is_release { "release" } else { "debug" }
    );
    let mut cargo_cmd = Command::new("cargo");
    cargo_cmd.current_dir(&actual_src);
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
    crate::builder::prepare_tool_command(&mut cargo_cmd, &env_vars);

    let status = crate::interrupts::command_status(&mut cargo_cmd)
        .with_context(|| format!("Failed to run cargo build for {}", spec.package.name))?;
    if !status.success() {
        anyhow::bail!("cargo build failed");
    }

    // Run post-compile hooks
    hooks::run_post_compile_commands(spec, &actual_src, destdir)?;

    // Install binaries to destdir
    crate::log_info!("Installing binaries to DESTDIR...");

    // Determine target directory
    let target_dir = if let Some(ref t) = target {
        actual_src.join("target").join(t).join(profile_dir)
    } else {
        actual_src.join("target").join(profile_dir)
    };

    // Use bindir from flags (default: /usr/bin)
    let bin_dir = destdir.join(flags.bindir.trim_start_matches('/'));
    fs::create_dir_all(&bin_dir)?;

    // Find and copy executable files
    let mut hardlink_tracker = crate::fs_copy::HardlinkCopyTracker::new();
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
                    hardlink_tracker.copy_file(&path, &dest)?;

                    // Preserve executable permission
                    let mut perms = fs::metadata(&dest)?.permissions();
                    perms.set_mode(0o755);
                    fs::set_permissions(&dest, perms)?;
                }
            }
        }
    }

    // Run post-install hooks
    hooks::run_post_install_commands(spec, &actual_src, destdir)?;

    Ok(())
}

fn resolve_actual_src(spec: &PackageSpec, src_dir: &Path) -> Result<PathBuf> {
    let source_subdir = spec.expand_vars(&spec.build.flags.source_subdir);
    if source_subdir.is_empty() {
        return Ok(src_dir.to_path_buf());
    }

    let candidate = Path::new(&source_subdir);
    if candidate.is_absolute() {
        if candidate.exists() {
            return Ok(candidate.to_path_buf());
        }
        bail!(
            "Source directory not found: {} (source_subdir: {} -> {})",
            candidate.display(),
            spec.build.flags.source_subdir,
            source_subdir
        );
    }

    let under_src = src_dir.join(&source_subdir);
    if under_src.exists() {
        return Ok(under_src);
    }

    let under_spec = spec.spec_dir.join(&source_subdir);
    if under_spec.exists() {
        return Ok(under_spec);
    }

    if candidate.exists() {
        return Ok(candidate.to_path_buf());
    }

    bail!(
        "Source directory not found: {} (expanded from '{}'; tried src_dir, spec_dir, and absolute path)",
        source_subdir,
        spec.build.flags.source_subdir
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, PackageInfo};
    use tempfile::tempdir;

    fn test_spec(spec_dir: PathBuf, source_subdir: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "red".into(),
                real_name: None,
                version: "1.0.2".into(),
                revision: 1,
                description: String::new(),
                homepage: String::new(),
                abi_breaking: false,
                built_against: Vec::new(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Rust,
                flags: BuildFlags {
                    source_subdir: source_subdir.into(),
                    ..BuildFlags::default()
                },
            },
            dependencies: Default::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir,
        }
    }

    #[test]
    fn resolve_actual_src_supports_source_subdir() {
        let tmp = tempdir().unwrap();
        let src_dir = tmp.path().join("source");
        let nested = src_dir.join("red-1.0.2");
        fs::create_dir_all(&nested).unwrap();

        let spec = test_spec(tmp.path().join("spec"), "$name-$version");
        assert_eq!(resolve_actual_src(&spec, &src_dir).unwrap(), nested);
    }

    #[test]
    fn resolve_actual_src_rejects_missing_source_subdir() {
        let tmp = tempdir().unwrap();
        let src_dir = tmp.path().join("source");
        fs::create_dir_all(&src_dir).unwrap();

        let spec = test_spec(tmp.path().join("spec"), "missing");
        let error = resolve_actual_src(&spec, &src_dir).unwrap_err();
        assert!(error.to_string().contains("Source directory not found"));
    }
}
