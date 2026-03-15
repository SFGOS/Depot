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
    let make_exec_override = flags.make_exec.trim();

    // Determine actual source directory (support source_subdir)
    let actual_src = resolve_actual_src(spec, src_dir)?;

    let build_dir = if let Some(dir) = &flags.build_dir {
        actual_src.join(dir)
    } else {
        actual_src.join("build")
    };

    // Create directories
    fs::create_dir_all(&build_dir)?;
    fs::create_dir_all(destdir)?;

    // Environment variables
    let env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);

    // Extract prefix from configure flags (cmake-style -DCMAKE_INSTALL_PREFIX=)
    let prefix = cmake_cache_entry_value(&flags.configure, "CMAKE_INSTALL_PREFIX")
        .unwrap_or(flags.prefix.as_str());

    // Generate toolchain file if cross-compiling
    let toolchain_file = if let Some(cc_cfg) = cross {
        Some(cc_cfg.generate_cmake_toolchain(&build_dir)?)
    } else {
        None
    };

    use crate::builder::state::{BuildStep, StateTracker};
    let mut state = StateTracker::new_with_namespace(
        &actual_src,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;

    // Run cmake configure
    if !state.is_done(BuildStep::Configured) {
        crate::log_info!("Running cmake configure...");
        let mut cmake_cmd = Command::new("cmake");
        cmake_cmd.current_dir(&build_dir);
        cmake_cmd.arg("-S").arg(&actual_src);
        cmake_cmd.arg("-B").arg(&build_dir);
        cmake_cmd.arg(format!("-DCMAKE_INSTALL_PREFIX={}", prefix));
        cmake_cmd.arg("-DCMAKE_BUILD_TYPE=Release");
        for arg in cmake_install_dir_args(flags) {
            cmake_cmd.arg(arg);
        }
        for arg in cmake_lib32_target_args(flags, cross) {
            cmake_cmd.arg(arg);
        }

        // Add toolchain file for cross-compilation
        if let Some(ref tf) = toolchain_file {
            cmake_cmd.arg(format!("-DCMAKE_TOOLCHAIN_FILE={}", tf.display()));
        }

        if !make_exec_override.is_empty() {
            if !cmake_configure_flags_specify_generator(&flags.configure)
                && let Some(generator) = cmake_generator_for_make_exec(make_exec_override)
            {
                cmake_cmd.arg("-G").arg(generator);
            }
            if !cmake_configure_flags_set_make_program(&flags.configure) {
                cmake_cmd.arg(format!("-DCMAKE_MAKE_PROGRAM={make_exec_override}"));
            }
        }

        // Add custom configure flags from spec (supports cross-compilation overrides).
        // Expand using env-vars we will supply to the child process first so patterns
        // like `$CXX` (coming from flags.cxx) are substituted.
        for flag in &flags.configure {
            let expanded = expand_with_envs(flag, &env_vars);
            cmake_cmd.arg(&expanded);
        }

        crate::builder::prepare_tool_command(&mut cmake_cmd, &env_vars);

        let status =
            crate::interrupts::command_status(&mut cmake_cmd).context("Failed to run cmake")?;
        if !status.success() {
            anyhow::bail!("cmake configure failed");
        }

        crate::source::hooks::run_post_configure_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    } else {
        crate::log_info!("Skipping cmake configure (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        // Run cmake build
        crate::log_info!("Running cmake build...");
        let build_targets = phase_targets(&flags.make_target, &flags.make_targets);
        let mut build_cmd = Command::new("cmake");
        build_cmd.arg("--build").arg(&build_dir);
        build_cmd.arg("-j").arg(num_cpus().to_string());
        if !build_targets.is_empty() {
            build_cmd.arg("--target");
            for target in &build_targets {
                build_cmd.arg(target);
            }
        }

        crate::builder::prepare_tool_command(&mut build_cmd, &env_vars);

        let status = crate::interrupts::command_status(&mut build_cmd)
            .with_context(|| format!("Failed to run cmake build for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("cmake build failed");
        }

        if flags.skip_tests {
            crate::log_info!("Skipping tests: disabled by build.flags.skip_tests");
        } else {
            let test_targets = phase_targets(&flags.make_test_target, &flags.make_test_targets);
            if !cmake_uses_default_ctest(flags) {
                let joined = test_targets.join(" ");
                crate::log_info!("Running cmake test target(s): {}...", joined);
                let mut test_cmd = Command::new("cmake");
                test_cmd.arg("--build").arg(&build_dir);
                test_cmd.arg("--target");
                for target in &test_targets {
                    test_cmd.arg(target);
                }
                crate::builder::prepare_tool_command(&mut test_cmd, &env_vars);

                let status =
                    crate::interrupts::command_status(&mut test_cmd).with_context(|| {
                        format!(
                            "Failed to run cmake build target(s) '{}' for {}",
                            joined, spec.package.name
                        )
                    })?;
                if !status.success() {
                    anyhow::bail!("cmake test target(s) '{}' failed", joined);
                }
            } else {
                crate::log_info!("Running ctest...");
                let mut test_cmd = Command::new("ctest");
                test_cmd.current_dir(&build_dir);
                test_cmd.arg("--test-dir").arg(&build_dir);
                test_cmd.arg("-j").arg(num_cpus().to_string());
                test_cmd.arg("--output-on-failure");
                crate::builder::prepare_tool_command(&mut test_cmd, &env_vars);

                let status = crate::interrupts::command_status(&mut test_cmd)
                    .with_context(|| format!("Failed to run ctest for {}", spec.package.name))?;
                if !status.success() {
                    anyhow::bail!("ctest failed");
                }
            }
        }

        crate::source::hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        crate::log_info!("Skipping cmake build and post-compile hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run cmake install with fakeroot if not root
        crate::log_info!(
            "Running cmake install{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        let install_targets =
            phase_targets(&flags.make_install_target, &flags.make_install_targets);
        let mut install_cmd = fakeroot::wrap_install_command("cmake", destdir);
        if !install_targets.is_empty() {
            install_cmd.arg("--build").arg(&build_dir);
            install_cmd.arg("--target");
            for target in &install_targets {
                install_cmd.arg(target);
            }
        } else {
            install_cmd.arg("--install").arg(&build_dir);
        }

        let mut install_env = env_vars.clone();
        install_env.push((
            "DESTDIR".to_string(),
            destdir.to_string_lossy().into_owned(),
        ));
        crate::builder::prepare_tool_command(&mut install_cmd, &install_env);

        let status = crate::interrupts::command_status(&mut install_cmd)
            .with_context(|| format!("Failed to run cmake install for {}", spec.package.name))?;
        if !status.success() {
            if !install_targets.is_empty() {
                anyhow::bail!(
                    "cmake install target(s) '{}' failed",
                    install_targets.join(" ")
                );
            }
            anyhow::bail!("cmake install failed");
        }

        crate::source::hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping cmake install and post-install hooks (already done)");
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

fn nonempty_trimmed(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn phase_targets(single: &str, many: &[String]) -> Vec<String> {
    let mut targets = Vec::new();
    if let Some(target) = nonempty_trimmed(single) {
        targets.push(target.to_string());
    }
    for target in many {
        if let Some(target) = nonempty_trimmed(target) {
            targets.push(target.to_string());
        }
    }
    targets
}

fn cmake_uses_default_ctest(flags: &crate::package::BuildFlags) -> bool {
    phase_targets(&flags.make_test_target, &flags.make_test_targets).is_empty()
}

fn cmake_generator_for_make_exec(make_exec: &str) -> Option<&'static str> {
    let tool = Path::new(make_exec)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase())?;
    if tool.contains("ninja") {
        Some("Ninja")
    } else if tool == "make"
        || tool == "gmake"
        || tool == "bmake"
        || tool == "nmake"
        || tool.ends_with("-make")
        || tool.ends_with("make.exe")
    {
        Some("Unix Makefiles")
    } else {
        None
    }
}

fn cmake_configure_flags_specify_generator(flags: &[String]) -> bool {
    flags.iter().any(|flag| {
        let trimmed = flag.trim();
        trimmed == "-G"
            || trimmed.starts_with("-G")
            || trimmed == "--generator"
            || trimmed.starts_with("--generator=")
    })
}

fn cmake_configure_flags_set_make_program(flags: &[String]) -> bool {
    cmake_cache_entry_value(flags, "CMAKE_MAKE_PROGRAM").is_some()
}

fn cmake_cache_entry_value<'a>(flags: &'a [String], variable: &str) -> Option<&'a str> {
    let plain_prefix = format!("-D{variable}=");
    let typed_prefix = format!("-D{variable}:");
    flags.iter().find_map(|flag| {
        let trimmed = flag.trim();
        if let Some(value) = trimmed.strip_prefix(&plain_prefix) {
            return Some(value);
        }
        trimmed
            .strip_prefix(&typed_prefix)
            .and_then(|rest| rest.split_once('=').map(|(_, value)| value))
    })
}

fn cmake_install_dir_args(flags: &crate::package::BuildFlags) -> Vec<String> {
    let prefix = cmake_cache_entry_value(&flags.configure, "CMAKE_INSTALL_PREFIX")
        .unwrap_or(flags.prefix.as_str());
    let dirs = crate::builder::install_dirs(flags);
    let defaults = [
        (
            "CMAKE_INSTALL_BINDIR",
            cmake_install_dir_value(prefix, &dirs.bindir),
        ),
        (
            "CMAKE_INSTALL_SBINDIR",
            cmake_install_dir_value(prefix, &dirs.sbindir),
        ),
        (
            "CMAKE_INSTALL_LIBDIR",
            cmake_install_dir_value(prefix, &dirs.libdir),
        ),
        (
            "CMAKE_INSTALL_LIBEXECDIR",
            cmake_install_dir_value(prefix, &dirs.libexecdir),
        ),
        (
            "CMAKE_INSTALL_SYSCONFDIR",
            cmake_install_dir_value(prefix, &dirs.sysconfdir),
        ),
        (
            "CMAKE_INSTALL_LOCALSTATEDIR",
            cmake_install_dir_value(prefix, &dirs.localstatedir),
        ),
        (
            "CMAKE_INSTALL_SHAREDSTATEDIR",
            cmake_install_dir_value(prefix, &dirs.sharedstatedir),
        ),
        (
            "CMAKE_INSTALL_INCLUDEDIR",
            cmake_install_dir_value(prefix, &dirs.includedir),
        ),
        (
            "CMAKE_INSTALL_DATAROOTDIR",
            cmake_install_dir_value(prefix, &dirs.datarootdir),
        ),
        (
            "CMAKE_INSTALL_DATADIR",
            cmake_install_dir_value(prefix, &dirs.datadir),
        ),
        (
            "CMAKE_INSTALL_MANDIR",
            cmake_install_dir_value(prefix, &dirs.mandir),
        ),
        (
            "CMAKE_INSTALL_INFODIR",
            cmake_install_dir_value(prefix, &dirs.infodir),
        ),
    ];

    defaults
        .into_iter()
        .filter(|(variable, _)| cmake_cache_entry_value(&flags.configure, variable).is_none())
        .map(|(variable, value)| format!("-D{variable}={value}"))
        .collect()
}

fn cmake_lib32_target_args(
    flags: &crate::package::BuildFlags,
    cross: Option<&CrossConfig>,
) -> Vec<String> {
    if !flags.lib32_variant {
        return Vec::new();
    }

    let target = match lib32_target_triple(flags, cross) {
        Some(target) => target,
        None => return Vec::new(),
    };
    let arch = crate::cross::target_arch_from_triple(&target);
    let defaults = [
        ("CMAKE_SYSTEM_PROCESSOR", arch.to_string()),
        ("CMAKE_C_COMPILER_TARGET", target.clone()),
        ("CMAKE_CXX_COMPILER_TARGET", target.clone()),
        ("CMAKE_ASM_COMPILER_TARGET", target),
    ];

    defaults
        .into_iter()
        .filter(|(variable, _)| cmake_cache_entry_value(&flags.configure, variable).is_none())
        .map(|(variable, value)| format!("-D{variable}={value}"))
        .collect()
}

fn lib32_target_triple(
    flags: &crate::package::BuildFlags,
    cross: Option<&CrossConfig>,
) -> Option<String> {
    let host = if let Some(cc_cfg) = cross {
        Some(cc_cfg.host_triple().to_string())
    } else if !flags.chost.trim().is_empty() {
        Some(flags.chost.trim().to_string())
    } else {
        let detected = CrossConfig::build_triple();
        if let Err(err) = &detected {
            crate::log_warn!(
                "Failed to detect native build triple for lib32 CMake target flags: {}",
                err
            );
        }
        detected.ok()
    };

    host.map(|host| crate::cross::lib32_target_triple(&host))
}

fn cmake_install_dir_value(prefix: &str, value: &str) -> String {
    let trimmed_prefix = prefix.trim();
    let trimmed_value = value.trim();
    if trimmed_prefix.is_empty() || trimmed_value.is_empty() {
        return trimmed_value.to_string();
    }

    let prefix_path = Path::new(trimmed_prefix);
    let value_path = Path::new(trimmed_value);
    if !prefix_path.is_absolute() || !value_path.is_absolute() {
        return trimmed_value.to_string();
    }

    match value_path.strip_prefix(prefix_path) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => trimmed_value.to_string(),
    }
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
    use crate::test_support::TestEnv;
    use tempfile::tempdir;

    #[test]
    fn test_expand_env_vars_replaces_vars() {
        // Set a test env var
        let mut env = TestEnv::new();
        env.set_var("DEPOT_TEST_FOO", "bar");
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
    fn test_phase_targets_merges_singular_and_plural() {
        assert_eq!(
            phase_targets("bootstrap", &["stage1".into(), "stage2".into()]),
            vec![
                "bootstrap".to_string(),
                "stage1".to_string(),
                "stage2".to_string()
            ]
        );
        assert!(phase_targets("", &[]).is_empty());
    }

    #[test]
    fn test_cmake_uses_default_ctest_without_explicit_targets() {
        assert!(cmake_uses_default_ctest(&BuildFlags::default()));

        let explicit_single = BuildFlags {
            make_test_target: "test".into(),
            ..BuildFlags::default()
        };
        assert!(!cmake_uses_default_ctest(&explicit_single));

        let explicit_many = BuildFlags {
            make_test_targets: vec!["check".into()],
            ..BuildFlags::default()
        };
        assert!(!cmake_uses_default_ctest(&explicit_many));
    }

    #[test]
    fn test_cmake_generator_for_make_exec_detects_ninja_and_make() {
        assert_eq!(cmake_generator_for_make_exec("ninja"), Some("Ninja"));
        assert_eq!(
            cmake_generator_for_make_exec("/usr/bin/gmake"),
            Some("Unix Makefiles")
        );
        assert_eq!(cmake_generator_for_make_exec("samurai"), None);
    }

    #[test]
    fn test_cmake_configure_flag_detectors() {
        assert!(cmake_configure_flags_specify_generator(&[
            "-G".to_string(),
            "Ninja".to_string()
        ]));
        assert!(cmake_configure_flags_specify_generator(&[
            "--generator=Unix Makefiles".to_string()
        ]));
        assert!(!cmake_configure_flags_specify_generator(&[
            "-DCMAKE_BUILD_TYPE=Release".to_string()
        ]));

        assert!(cmake_configure_flags_set_make_program(&[
            "-DCMAKE_MAKE_PROGRAM=/usr/bin/ninja".to_string()
        ]));
        assert!(!cmake_configure_flags_set_make_program(&[
            "-DCMAKE_C_COMPILER=clang".to_string()
        ]));
    }

    #[test]
    fn test_cmake_cache_entry_value_supports_plain_and_typed_entries() {
        let flags = vec![
            "-DCMAKE_INSTALL_PREFIX=/usr".to_string(),
            "-DCMAKE_INSTALL_LIBDIR:PATH=/usr/lib64".to_string(),
        ];

        assert_eq!(
            cmake_cache_entry_value(&flags, "CMAKE_INSTALL_PREFIX"),
            Some("/usr")
        );
        assert_eq!(
            cmake_cache_entry_value(&flags, "CMAKE_INSTALL_LIBDIR"),
            Some("/usr/lib64")
        );
        assert_eq!(
            cmake_cache_entry_value(&flags, "CMAKE_INSTALL_BINDIR"),
            None
        );
    }

    #[test]
    fn test_cmake_install_dir_args_include_defaults() {
        let args = cmake_install_dir_args(&BuildFlags::default());
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_BINDIR=bin"));
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_SBINDIR=bin"));
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_LIBDIR=lib"));
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_LIBEXECDIR=lib"));
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_SYSCONFDIR=/etc"));
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_INSTALL_LOCALSTATEDIR=/var")
        );
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_INSTALL_SHAREDSTATEDIR=/var/lib")
        );
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_INSTALL_INCLUDEDIR=include")
        );
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_INSTALL_DATAROOTDIR=share")
        );
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_DATADIR=share"));
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_MANDIR=share/man"));
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_INSTALL_INFODIR=share/info")
        );
    }

    #[test]
    fn test_cmake_lib32_target_args_include_compiler_target_defaults() {
        let flags = BuildFlags {
            lib32_variant: true,
            chost: "x86_64-sfg-linux-gnu".into(),
            ..BuildFlags::default()
        };

        let args = cmake_lib32_target_args(&flags, None);
        assert!(args.iter().any(|a| a == "-DCMAKE_SYSTEM_PROCESSOR=i686"));
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_C_COMPILER_TARGET=i686-sfg-linux-gnu")
        );
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_CXX_COMPILER_TARGET=i686-sfg-linux-gnu")
        );
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_ASM_COMPILER_TARGET=i686-sfg-linux-gnu")
        );
    }

    #[test]
    fn test_cmake_lib32_target_args_respect_explicit_overrides() {
        let flags = BuildFlags {
            lib32_variant: true,
            chost: "x86_64-sfg-linux-gnu".into(),
            configure: vec![
                "-DCMAKE_C_COMPILER_TARGET=i686-custom-linux-gnu".into(),
                "-DCMAKE_SYSTEM_PROCESSOR=i686".into(),
            ],
            ..BuildFlags::default()
        };

        let args = cmake_lib32_target_args(&flags, None);
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("-DCMAKE_C_COMPILER_TARGET="))
        );
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("-DCMAKE_SYSTEM_PROCESSOR="))
        );
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_CXX_COMPILER_TARGET=i686-sfg-linux-gnu")
        );
    }

    #[test]
    fn test_cmake_install_dir_args_respect_explicit_user_overrides() {
        let flags = BuildFlags {
            configure: vec![
                "-DCMAKE_INSTALL_SBINDIR=/sbin".to_string(),
                "-DCMAKE_INSTALL_LIBDIR:PATH=/custom/lib".to_string(),
                "-DCMAKE_INSTALL_DATADIR=/custom/share".to_string(),
            ],
            ..BuildFlags::default()
        };

        let args = cmake_install_dir_args(&flags);
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("-DCMAKE_INSTALL_SBINDIR="))
        );
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("-DCMAKE_INSTALL_LIBDIR="))
        );
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("-DCMAKE_INSTALL_DATADIR="))
        );
        assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_BINDIR=bin"));
    }

    #[test]
    fn test_cmake_install_dir_value_makes_prefix_children_relative() {
        assert_eq!(cmake_install_dir_value("/usr", "/usr/include"), "include");
        assert_eq!(cmake_install_dir_value("/", "/usr/include"), "usr/include");
        assert_eq!(
            cmake_install_dir_value("/opt/depot", "/opt/depot/lib64"),
            "lib64"
        );
        assert_eq!(cmake_install_dir_value("/usr", "/etc"), "/etc");
        assert_eq!(cmake_install_dir_value("/usr", "include"), "include");
    }

    #[test]
    fn resolve_actual_src_prefers_srcdir_then_specdir_and_handles_absolute() {
        let tmp = tempdir().unwrap();
        let src_root = tmp.path().join("srcroot");
        let spec_dir = tmp.path().join("specdir");
        let external = tmp.path().join("external");
        let expanded = src_root.join("x-1.0").join("sub");
        std::fs::create_dir_all(src_root.join("sub")).unwrap();
        std::fs::create_dir_all(&expanded).unwrap();
        // create directories for candidates
        std::fs::create_dir_all(spec_dir.join("../llvm")).unwrap();
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
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
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
