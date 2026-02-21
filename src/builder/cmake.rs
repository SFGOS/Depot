//! CMake build system

use crate::cross::CrossConfig;
use crate::fakeroot;
use crate::package::PackageSpec;
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

    // Determine actual source directory (support source_subdir)
    let actual_src = resolve_actual_src(spec, src_dir)?;

    let build_dir = actual_src.join("build");

    // Create directories
    fs::create_dir_all(&build_dir)?;
    fs::create_dir_all(destdir)?;

    // Environment variables
    let env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);

    // Extract prefix from configure flags (cmake-style -DCMAKE_INSTALL_PREFIX=)
    let prefix = flags
        .configure
        .iter()
        .find(|s| s.contains("CMAKE_INSTALL_PREFIX="))
        .and_then(|s| s.split('=').nth(1))
        .unwrap_or(&flags.prefix);

    // Generate toolchain file if cross-compiling
    let toolchain_file = if let Some(cc_cfg) = cross {
        Some(cc_cfg.generate_cmake_toolchain(&build_dir)?)
    } else {
        None
    };

    use crate::builder::state::{BuildStep, StateTracker};
    let mut state = StateTracker::new(&actual_src)?;

    // Run cmake configure
    if !state.is_done(BuildStep::Configured) {
        println!("Running cmake configure...");
        let mut cmake_cmd = Command::new("cmake");
        cmake_cmd.current_dir(&build_dir);
        cmake_cmd.arg("-S").arg(&actual_src);
        cmake_cmd.arg("-B").arg(&build_dir);
        cmake_cmd.arg(format!("-DCMAKE_INSTALL_PREFIX={}", prefix));
        cmake_cmd.arg("-DCMAKE_BUILD_TYPE=Release");

        // Add toolchain file for cross-compilation
        if let Some(ref tf) = toolchain_file {
            cmake_cmd.arg(format!("-DCMAKE_TOOLCHAIN_FILE={}", tf.display()));
        }

        // Add custom configure flags from spec (supports cross-compilation overrides).
        // Expand using env-vars we will supply to the child process first so patterns
        // like `$CXX` (coming from flags.cxx) are substituted.
        for flag in &flags.configure {
            let expanded = expand_with_envs(flag, &env_vars);
            cmake_cmd.arg(&expanded);
        }

        crate::builder::prepare_command(&mut cmake_cmd, &env_vars);

        let status = cmake_cmd.status().context("Failed to run cmake")?;
        if !status.success() {
            anyhow::bail!("cmake configure failed");
        }
        state.mark_done(BuildStep::Configured)?;
    } else {
        println!("Skipping cmake configure (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        // Run cmake build
        println!("Running cmake build...");
        let mut build_cmd = Command::new("cmake");
        build_cmd.arg("--build").arg(&build_dir);
        build_cmd.arg("-j").arg(num_cpus().to_string());

        crate::builder::prepare_command(&mut build_cmd, &env_vars);

        let status = build_cmd
            .status()
            .with_context(|| format!("Failed to run cmake build for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("cmake build failed");
        }

        // Note: CMake doesn't have a direct "after make, before install" hook as easy as autotools,
        // but we can run it here.
        crate::source::hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        println!("Skipping cmake build and post-compile hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run cmake install with fakeroot if not root
        println!(
            "Running cmake install{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        let mut install_cmd = fakeroot::wrap_install_command("cmake", destdir);
        install_cmd.arg("--install").arg(&build_dir);

        let mut install_env = env_vars.clone();
        install_env.push((
            "DESTDIR".to_string(),
            destdir.to_string_lossy().into_owned(),
        ));
        crate::builder::prepare_command(&mut install_cmd, &install_env);

        let status = install_cmd
            .status()
            .with_context(|| format!("Failed to run cmake install for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("cmake install failed");
        }

        crate::source::hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        println!("Skipping cmake install and post-install hooks (already done)");
    }

    Ok(())
}

/// Expand environment variables in a string (e.g., $DEPOT_SYSROOT)
fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    // Simple expansion for $VAR and ${VAR} patterns using process environment only
    for (key, value) in std::env::vars() {
        result = result.replace(&format!("${}", key), &value);
        result = result.replace(&format!("${{{}}}", key), &value);
    }
    result
}

/// Expand using a provided set of env vars (used to expand flags before spawning child).
fn expand_with_envs(input: &str, envs: &[(String, String)]) -> String {
    let mut result = input.to_string();
    for (k, v) in envs {
        result = result.replace(&format!("${}", k), v);
        result = result.replace(&format!("${{{}}}", k), v);
    }
    expand_env_vars(&result)
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Resolve `source_subdir` with multiple fallbacks:
/// - empty -> use `src_dir`
/// - absolute path -> use if exists
/// - `src_dir/<sub>` -> use if exists
/// - `spec.spec_dir/<sub>` -> use if exists (supports `../llvm`)
/// - bare relative path (cwd)
fn resolve_actual_src(
    spec: &crate::package::PackageSpec,
    src_dir: &Path,
) -> anyhow::Result<std::path::PathBuf> {
    let flags = &spec.build.flags;
    let source_subdir = spec.expand_vars(&flags.source_subdir);
    if source_subdir.is_empty() {
        return Ok(src_dir.to_path_buf());
    }

    let candidate = std::path::Path::new(&source_subdir);
    // 1) absolute path -> use directly
    if candidate.is_absolute() {
        if candidate.exists() {
            return Ok(candidate.to_path_buf());
        }
        anyhow::bail!(
            "Source directory not found: {} (source_subdir: {} -> {})",
            candidate.display(),
            flags.source_subdir,
            source_subdir
        );
    }

    // 2) src_dir/<source_subdir>
    let under_src = src_dir.join(&source_subdir);
    if under_src.exists() {
        return Ok(under_src);
    }

    // 3) spec_dir/<source_subdir> (useful for ../llvm relative to spec)
    let spec_path = spec.spec_dir.join(&source_subdir);
    if spec_path.exists() {
        return Ok(spec_path);
    }

    // 4) bare relative path (relative to CWD)
    if candidate.exists() {
        return Ok(candidate.to_path_buf());
    }

    // fallback error
    anyhow::bail!(
        "Source directory not found: {} (expanded from '{}'; tried src_dir, spec_dir, and absolute path)",
        source_subdir,
        flags.source_subdir
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, PackageInfo, PackageSpec};
    use tempfile::tempdir;

    #[test]
    fn test_expand_env_vars_replaces_vars() {
        // Set a test env var
        unsafe { std::env::set_var("DEPOT_TEST_FOO", "bar") };
        let input = "$DEPOT_TEST_FOO and ${DEPOT_TEST_FOO}";
        let out = expand_env_vars(input);
        assert!(out.contains("bar"));
        assert_eq!(out, "bar and bar");
    }

    #[test]
    fn test_expand_with_envs_prefers_provided_envs() {
        let envs = vec![
            ("CXX".to_string(), "my-cxx".to_string()),
            ("CC".to_string(), "my-cc".to_string()),
        ];
        let s = "-DCMAKE_C_COMPILER=$CC -DCMAKE_CXX_COMPILER=${CXX} -DROOT=$HOME";
        let out = expand_with_envs(s, &envs);
        assert!(out.contains("my-cc"));
        assert!(out.contains("my-cxx"));
        // $HOME should be expanded from process env (may be present)
    }

    #[test]
    fn test_num_cpus_at_least_one() {
        let n = num_cpus();
        assert!(n >= 1);
    }

    #[test]
    fn resolve_actual_src_prefers_srcdir_then_specdir_and_handles_absolute() {
        let tmp = tempdir().unwrap();
        let src_root = tmp.path().join("srcroot");
        let spec_dir = tmp.path().join("specdir");
        let external = tmp.path().join("external");
        let expanded = src_root.join("x-1.0").join("sub");
        std::fs::create_dir_all(&src_root.join("sub")).unwrap();
        std::fs::create_dir_all(&expanded).unwrap();
        // create directories for candidates
        std::fs::create_dir_all(&spec_dir.join("../llvm")).unwrap();
        std::fs::create_dir_all(&external).unwrap();

        let spec = PackageSpec {
            package: PackageInfo {
                name: "x".into(),
                version: "1.0".into(),
                revision: 1,
                description: "".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::CMake,
                flags: BuildFlags {
                    source_subdir: "sub".into(),
                    ..BuildFlags::default()
                },
            },
            dependencies: Default::default(),
            spec_dir: spec_dir.clone(),
        };

        // case: relative path under src_dir
        let p = resolve_actual_src(&spec, &src_root).unwrap();
        assert!(p.ends_with("sub"));

        // case: ../llvm should resolve relative to spec_dir
        let mut spec2 = spec.clone();
        spec2.build.flags.source_subdir = "../llvm".into();
        let p2 = resolve_actual_src(&spec2, &src_root).unwrap();
        assert!(p2.ends_with("llvm"));

        // case: absolute path
        let mut spec3 = spec.clone();
        spec3.build.flags.source_subdir = external.to_string_lossy().into_owned();
        let p3 = resolve_actual_src(&spec3, &src_root).unwrap();
        assert_eq!(p3, external);

        // case: variable expansion in source_subdir
        let mut spec4 = spec.clone();
        spec4.build.flags.source_subdir = "$name-$version/sub".into();
        let p4 = resolve_actual_src(&spec4, &src_root).unwrap();
        assert_eq!(p4, expanded);
    }
}
