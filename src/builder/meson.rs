//! Meson build system

use crate::cross::CrossConfig;
use crate::fakeroot;
use crate::package::PackageSpec;
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
) -> Result<()> {
    let flags = &spec.build.flags;

    // Determine actual source directory (support source_subdir)
    let actual_src = resolve_actual_src(spec, src_dir)?;

    let build_dir = resolve_build_dir(&actual_src, flags);

    // Create directories
    fs::create_dir_all(&build_dir)?;
    fs::create_dir_all(destdir)?;

    // Environment variables
    let env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);

    // Generate cross file if cross-compiling
    let cross_file = if let Some(cc_cfg) = cross {
        Some(cc_cfg.generate_meson_cross_file(&build_dir)?)
    } else {
        None
    };

    use crate::builder::state::{BuildStep, StateTracker};
    let mut state = StateTracker::new_with_namespace(
        &actual_src,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;

    // Run meson setup
    if !state.is_done(BuildStep::Configured) {
        crate::log_info!("Running meson setup...");
        let mut meson_cmd = Command::new("meson");
        meson_cmd.current_dir(&actual_src);
        meson_cmd.arg("setup");
        meson_cmd.arg(&build_dir);

        for arg in meson_setup_args(flags, cross_file.as_deref(), &env_vars) {
            meson_cmd.arg(arg);
        }

        crate::builder::prepare_tool_command(&mut meson_cmd, &env_vars);

        let status = meson_cmd.status().context("Failed to run meson setup")?;
        if !status.success() {
            anyhow::bail!("meson setup failed");
        }

        crate::source::hooks::run_post_configure_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    } else {
        crate::log_info!("Skipping meson setup (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        // Run ninja build
        crate::log_info!("Running ninja...");
        let mut ninja_cmd = Command::new("ninja");
        ninja_cmd.current_dir(&build_dir);
        ninja_cmd.arg("-j").arg(num_cpus().to_string());

        crate::builder::prepare_tool_command(&mut ninja_cmd, &env_vars);

        let status = ninja_cmd
            .status()
            .with_context(|| format!("Failed to run ninja for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("ninja build failed");
        }

        crate::source::hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        crate::log_info!("Skipping ninja build and post-compile hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run meson install with fakeroot if not root
        crate::log_info!(
            "Running meson install{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        let mut install_cmd = fakeroot::wrap_install_command("meson", destdir);
        install_cmd.arg("install");
        install_cmd.arg("-C").arg(&build_dir);

        let mut install_env = env_vars.clone();
        install_env.push((
            "DESTDIR".to_string(),
            destdir.to_string_lossy().into_owned(),
        ));
        crate::builder::prepare_tool_command(&mut install_cmd, &install_env);

        let status = install_cmd
            .status()
            .with_context(|| format!("Failed to run meson install for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("meson install failed");
        }

        crate::source::hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping meson install and post-install hooks (already done)");
    }

    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn resolve_build_dir(actual_src: &Path, flags: &crate::package::BuildFlags) -> PathBuf {
    if let Some(dir) = flags
        .build_dir
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        actual_src.join(dir)
    } else {
        actual_src.join("builddir")
    }
}

fn has_option(configure: &[String], long: &str) -> bool {
    let prefix = format!("{long}=");
    for arg in configure {
        if arg == long || arg.starts_with(&prefix) {
            return true;
        }
    }
    false
}

fn meson_setup_args(
    flags: &crate::package::BuildFlags,
    cross_file: Option<&Path>,
    env_vars: &[(String, String)],
) -> Vec<String> {
    let mut args = Vec::new();
    let dirs = crate::builder::install_dirs(flags);

    if !has_option(&flags.configure, "--prefix") {
        args.push(format!("--prefix={}", flags.prefix));
    }
    for (option, value) in [
        ("--bindir", dirs.bindir),
        ("--sbindir", dirs.sbindir),
        ("--libdir", dirs.libdir),
        ("--libexecdir", dirs.libexecdir),
        ("--sysconfdir", dirs.sysconfdir),
        ("--localstatedir", dirs.localstatedir),
        ("--sharedstatedir", dirs.sharedstatedir),
        ("--includedir", dirs.includedir),
        ("--datadir", dirs.datadir),
        ("--mandir", dirs.mandir),
        ("--infodir", dirs.infodir),
    ] {
        if !has_option(&flags.configure, option) {
            args.push(format!("{option}={value}"));
        }
    }
    if !has_option(&flags.configure, "--buildtype") {
        args.push("--buildtype=release".to_string());
    }

    if let Some(cf) = cross_file {
        args.push(format!("--cross-file={}", cf.display()));
    }

    // Append user flags last so they can override defaults when Meson allows it.
    for arg in &flags.configure {
        args.push(expand_with_envs(arg, env_vars));
    }

    args
}

/// Expand environment variables in a string (e.g., $DEPOT_SYSROOT)
fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    // Simple expansion for $VAR and ${VAR} patterns using process environment only
    for (key, value) in std::env::vars() {
        result = result.replace(&format!("${key}"), &value);
        result = result.replace(&format!("${{{key}}}"), &value);
    }
    result
}

/// Expand using a provided set of env vars (used to expand flags before spawning child).
fn expand_with_envs(input: &str, envs: &[(String, String)]) -> String {
    let mut result = input.to_string();
    for (k, v) in envs {
        result = result.replace(&format!("${k}"), v);
        result = result.replace(&format!("${{{k}}}"), v);
    }
    expand_env_vars(&result)
}

/// Resolve `source_subdir` with multiple fallbacks:
/// - empty -> use `src_dir`
/// - absolute path -> use if exists
/// - `src_dir/<sub>` -> use if exists
/// - `spec.spec_dir/<sub>` -> use if exists
/// - bare relative path (cwd)
fn resolve_actual_src(spec: &crate::package::PackageSpec, src_dir: &Path) -> Result<PathBuf> {
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
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, Source};
    use tempfile::tempdir;

    #[test]
    fn test_num_cpus_at_least_one() {
        let n = num_cpus();
        assert!(n >= 1);
    }

    #[test]
    fn test_meson_setup_args_include_configure_flags() {
        let mut flags = BuildFlags {
            prefix: "/usr".to_string(),
            ..BuildFlags::default()
        };
        flags.configure = vec!["-Dmanpages=false".to_string()];

        let args = meson_setup_args(&flags, None, &[]);
        assert!(args.iter().any(|a| a == "-Dmanpages=false"));
        assert!(args.iter().any(|a| a == "--prefix=/usr"));
        assert!(args.iter().any(|a| a == "--buildtype=release"));
    }

    #[test]
    fn test_meson_setup_args_include_install_dirs() {
        let args = meson_setup_args(&BuildFlags::default(), None, &[]);
        assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
        assert!(args.iter().any(|a| a == "--sbindir=/usr/bin"));
        assert!(args.iter().any(|a| a == "--libdir=/usr/lib"));
        assert!(args.iter().any(|a| a == "--libexecdir=/usr/lib"));
        assert!(args.iter().any(|a| a == "--sysconfdir=/etc"));
        assert!(args.iter().any(|a| a == "--localstatedir=/var"));
        assert!(args.iter().any(|a| a == "--sharedstatedir=/var/lib"));
        assert!(args.iter().any(|a| a == "--includedir=/usr/include"));
        assert!(args.iter().any(|a| a == "--datadir=/usr/share"));
        assert!(args.iter().any(|a| a == "--mandir=/usr/share/man"));
        assert!(args.iter().any(|a| a == "--infodir=/usr/share/info"));
    }

    #[test]
    fn test_meson_setup_args_derive_dirs_from_datarootdir() {
        let flags = BuildFlags {
            datarootdir: "/opt/share-root".to_string(),
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert!(!args.iter().any(|a| a.starts_with("--datarootdir=")));
        assert!(args.iter().any(|a| a == "--datadir=/opt/share-root"));
        assert!(args.iter().any(|a| a == "--mandir=/opt/share-root/man"));
        assert!(args.iter().any(|a| a == "--infodir=/opt/share-root/info"));
    }

    #[test]
    fn test_meson_setup_args_honor_explicit_prefix() {
        let flags = BuildFlags {
            prefix: "/usr".to_string(),
            configure: vec!["--prefix=/opt".to_string()],
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert_eq!(args.iter().filter(|a| a.starts_with("--prefix")).count(), 1);
        assert!(args.iter().any(|a| a == "--prefix=/opt"));
    }

    #[test]
    fn test_meson_setup_args_honor_explicit_install_dirs() {
        let flags = BuildFlags {
            configure: vec![
                "--sbindir=/sbin".to_string(),
                "--libdir=/custom/lib".to_string(),
                "--datadir=/custom/share".to_string(),
            ],
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert!(!args.iter().any(|a| a == "--sbindir=/usr/bin"));
        assert!(!args.iter().any(|a| a == "--libdir=/usr/lib"));
        assert!(!args.iter().any(|a| a == "--datadir=/usr/share"));
        assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
    }

    #[test]
    fn test_resolve_build_dir_uses_flag() {
        let flags = BuildFlags {
            build_dir: Some("build".to_string()),
            ..BuildFlags::default()
        };
        let src = Path::new("/tmp/src");
        assert_eq!(
            resolve_build_dir(src, &flags),
            PathBuf::from("/tmp/src/build")
        );
    }

    #[test]
    fn test_resolve_actual_src_uses_source_subdir_under_source() -> Result<()> {
        let src = tempdir()?;
        let spec_dir = tempdir()?;
        fs::create_dir_all(src.path().join("sub"))?;

        let spec = PackageSpec {
            package: PackageInfo {
                name: "pkg".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "u".into(),
                sha256: "s".into(),
                extract_dir: "e".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Meson,
                flags: BuildFlags {
                    source_subdir: "sub".into(),
                    ..BuildFlags::default()
                },
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: spec_dir.path().to_path_buf(),
        };

        let resolved = resolve_actual_src(&spec, src.path())?;
        assert_eq!(resolved, src.path().join("sub"));
        Ok(())
    }
}
