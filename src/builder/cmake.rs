//! CMake build system

use crate::builder::BuildHelperContext;
use crate::builder::state::{BuildStep, StateTracker};
use crate::cross::CrossConfig;
use crate::fakeroot;
use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
    host_build_dir: Option<&Path>,
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
    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);
    if let Some(host_dir) = host_build_dir {
        crate::builder::set_env_var(
            &mut env_vars,
            crate::builder::DEPOT_BUILD_HOST_DIR_ENV,
            host_dir.to_string_lossy().into_owned(),
        );
    }

    // Extract prefix from configure flags (cmake-style -DCMAKE_INSTALL_PREFIX=)
    let prefix = effective_cmake_install_prefix(flags);

    // Generate toolchain file if cross-compiling
    let toolchain_file = if let Some(cc_cfg) = cross {
        Some(cc_cfg.generate_cmake_toolchain(&build_dir)?)
    } else {
        None
    };

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
        for arg in cmake_install_dir_args(flags, prefix) {
            cmake_cmd.arg(arg);
        }
        for arg in cmake_lib32_target_args(flags, cross) {
            cmake_cmd.arg(arg);
        }
        for arg in cmake_depot_sysroot_args(flags, depot_rootfs_from_env(&env_vars)) {
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
        for arg in crate::builder::static_build_args_for(crate::package::BuildType::CMake, flags)? {
            cmake_cmd.arg(arg);
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

        if spec.should_skip_automatic_tests() {
            if flags.skip_tests {
                crate::log_info!("Skipping tests: disabled by build.flags.skip_tests");
            } else {
                crate::log_info!(
                    "Skipping tests: automatic tests are disabled for multilib builds"
                );
            }
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
        // Run cmake install with internal fakeroot if not root
        crate::log_info!(
            "Running cmake install{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with internal fakeroot)"
            }
        );

        let install_targets =
            phase_targets(&flags.make_install_target, &flags.make_install_targets);
        let install_destdir =
            crate::builder::install_destdir_path(&build_dir, destdir, flags.lib32_variant);
        if flags.lib32_variant {
            if install_destdir.exists() {
                fs::remove_dir_all(&install_destdir).with_context(|| {
                    format!(
                        "Failed to clean temporary lib32 install dir: {}",
                        install_destdir.display()
                    )
                })?;
            }
            fs::create_dir_all(&install_destdir).with_context(|| {
                format!(
                    "Failed to create temporary lib32 install dir: {}",
                    install_destdir.display()
                )
            })?;
        }

        let mut install_cmd = fakeroot::wrap_install_command("cmake", &install_destdir);
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
            install_destdir.to_string_lossy().into_owned(),
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

        if flags.lib32_variant {
            crate::builder::stage_lib32_install_tree(&install_destdir, destdir)?;
            crate::source::hooks::run_post_install_commands_in_dir(spec, &build_dir, destdir)?;
        } else {
            crate::source::hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        }
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping cmake install and post-install hooks (already done)");
    }

    Ok(())
}

pub(crate) fn ensure_host_build(
    spec: &PackageSpec,
    src_dir: &Path,
    export_compiler_flags: bool,
) -> Result<PathBuf> {
    let host_spec = crate::builder::host_build_spec(spec);
    let flags = &host_spec.build.flags;

    let actual_src = resolve_actual_src(&host_spec, src_dir)?;
    let build_dir = crate::builder::host_build_dir_for_source(&actual_src, flags);

    fs::create_dir_all(&build_dir)?;

    let env_vars =
        crate::builder::standard_build_env(&host_spec, None, true, export_compiler_flags);
    let prefix = effective_cmake_install_prefix(flags);

    let mut state = StateTracker::new_with_namespace(&actual_src, Some("host"))?;

    if !state.is_done(BuildStep::Configured) {
        crate::log_info!(
            "Running host-side cmake configure in {}...",
            build_dir.display()
        );
        let mut cmake_cmd = Command::new("cmake");
        cmake_cmd.current_dir(&build_dir);
        cmake_cmd.arg("-S").arg(&actual_src);
        cmake_cmd.arg("-B").arg(&build_dir);
        cmake_cmd.arg(format!("-DCMAKE_INSTALL_PREFIX={}", prefix));
        cmake_cmd.arg("-DCMAKE_BUILD_TYPE=Release");
        for arg in cmake_install_dir_args(flags, prefix) {
            cmake_cmd.arg(arg);
        }

        let make_exec_override = flags.make_exec.trim();
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

        for flag in &flags.configure {
            let expanded = expand_with_envs(flag, &env_vars);
            cmake_cmd.arg(&expanded);
        }
        for arg in crate::builder::static_build_args_for(crate::package::BuildType::CMake, flags)? {
            cmake_cmd.arg(arg);
        }

        crate::builder::prepare_tool_command(&mut cmake_cmd, &env_vars);

        let status = crate::interrupts::command_status(&mut cmake_cmd)
            .context("Failed to run host cmake")?;
        if !status.success() {
            anyhow::bail!("host cmake configure failed");
        }

        state.mark_done(BuildStep::Configured)?;
    }

    if !state.is_done(BuildStep::PostCompileDone) {
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
            .with_context(|| format!("Failed to run host cmake build for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("host cmake build failed");
        }

        state.mark_done(BuildStep::PostCompileDone)?;
    }

    fs::canonicalize(&build_dir)
        .with_context(|| format!("Failed to resolve host build dir: {}", build_dir.display()))
}

pub(crate) fn run_helper_configure(
    context: &BuildHelperContext,
    source_dir: Option<&Path>,
    build_dir: Option<&Path>,
    cross: Option<&CrossConfig>,
    env_vars: &[(String, String)],
    extra_args: &[String],
) -> Result<()> {
    let flags = context.build_flags();
    let source_dir = source_dir
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir().context("Failed to determine current directory")?);
    let build_dir = build_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        flags
            .build_dir
            .as_ref()
            .map(|dir| source_dir.join(dir))
            .unwrap_or_else(|| source_dir.join("build"))
    });
    let prefix = effective_cmake_install_prefix(&flags);

    fs::create_dir_all(&build_dir)
        .with_context(|| format!("Failed to create build directory: {}", build_dir.display()))?;

    let toolchain_file = if let Some(cc_cfg) = cross {
        Some(cc_cfg.generate_cmake_toolchain(&build_dir)?)
    } else {
        None
    };

    let mut cmake_cmd = Command::new("cmake");
    cmake_cmd.current_dir(&build_dir);
    cmake_cmd.arg("-S").arg(&source_dir);
    cmake_cmd.arg("-B").arg(&build_dir);
    cmake_cmd.arg(format!("-DCMAKE_INSTALL_PREFIX={prefix}"));
    cmake_cmd.arg("-DCMAKE_BUILD_TYPE=Release");
    for arg in cmake_install_dir_args(&flags, prefix) {
        cmake_cmd.arg(arg);
    }
    for arg in cmake_lib32_target_args(&flags, cross) {
        cmake_cmd.arg(arg);
    }
    for arg in cmake_depot_sysroot_args(&flags, depot_rootfs_from_env(env_vars)) {
        cmake_cmd.arg(arg);
    }
    if let Some(toolchain_file) = &toolchain_file {
        cmake_cmd.arg(format!(
            "-DCMAKE_TOOLCHAIN_FILE={}",
            toolchain_file.display()
        ));
    }

    let make_exec_override = flags.make_exec.trim();
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

    for flag in &flags.configure {
        cmake_cmd.arg(expand_with_envs(&context.expand_vars(flag), env_vars));
    }
    for arg in crate::builder::static_build_args_for(crate::package::BuildType::CMake, &flags)? {
        cmake_cmd.arg(arg);
    }
    for arg in extra_args {
        cmake_cmd.arg(expand_with_envs(&context.expand_vars(arg), env_vars));
    }

    crate::builder::prepare_tool_command(&mut cmake_cmd, &env_vars.to_vec());

    let status = crate::interrupts::command_status(&mut cmake_cmd)
        .context("Failed to run helper cmake configure")?;
    if !status.success() {
        anyhow::bail!("cmake configure failed");
    }

    Ok(())
}

pub(crate) fn run_helper_install(
    context: &BuildHelperContext,
    build_dir: Option<&Path>,
    env_vars: &[(String, String)],
    extra_args: &[String],
) -> Result<()> {
    let flags = context.build_flags();
    let source_dir = helper_source_dir();
    let build_dir = build_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        flags
            .build_dir
            .as_ref()
            .map(|dir| source_dir.join(dir))
            .unwrap_or_else(|| source_dir.join("build"))
    });
    let destdir = std::env::var("DESTDIR").context("DESTDIR must be set for cmake_install")?;
    let install_targets = phase_targets(&flags.make_install_target, &flags.make_install_targets);

    let mut install_cmd = fakeroot::wrap_install_command("cmake", Path::new(&destdir));
    if install_targets.is_empty() {
        install_cmd.arg("--install").arg(&build_dir);
    } else {
        install_cmd.arg("--build").arg(&build_dir);
        install_cmd.arg("--target");
        for target in &install_targets {
            install_cmd.arg(target);
        }
    }
    for arg in extra_args {
        install_cmd.arg(context.expand_vars(arg));
    }

    let mut install_env = env_vars.to_vec();
    crate::builder::set_env_var(&mut install_env, "DESTDIR", destdir);
    crate::builder::prepare_tool_command(&mut install_cmd, &install_env);

    let status = crate::interrupts::command_status(&mut install_cmd)
        .context("Failed to run helper cmake install")?;
    if !status.success() {
        if install_targets.is_empty() {
            anyhow::bail!("cmake install failed");
        }
        anyhow::bail!(
            "cmake install target(s) '{}' failed",
            install_targets.join(" ")
        );
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

fn effective_cmake_install_prefix(flags: &crate::package::BuildFlags) -> &str {
    cmake_cache_entry_value(&flags.configure, "CMAKE_INSTALL_PREFIX")
        .unwrap_or(flags.prefix.as_str())
}

fn cmake_dir_value_for_prefix(prefix: &str, value: String) -> String {
    let prefix_path = Path::new(prefix);
    let value_path = Path::new(&value);

    if prefix_path.is_absolute()
        && value_path.is_absolute()
        && let Ok(relative) = value_path.strip_prefix(prefix_path)
    {
        let relative = relative.to_string_lossy().replace('\\', "/");
        return if relative.is_empty() {
            ".".to_string()
        } else {
            relative
        };
    }

    value
}

fn cmake_install_dir_args(flags: &crate::package::BuildFlags, prefix: &str) -> Vec<String> {
    let dirs = crate::builder::install_dirs(flags);
    let defaults = [
        (
            "CMAKE_INSTALL_BINDIR",
            cmake_dir_value_for_prefix(prefix, dirs.bindir),
        ),
        (
            "CMAKE_INSTALL_SBINDIR",
            cmake_dir_value_for_prefix(prefix, dirs.sbindir),
        ),
        (
            "CMAKE_INSTALL_LIBDIR",
            cmake_dir_value_for_prefix(prefix, dirs.libdir),
        ),
        (
            "CMAKE_INSTALL_LIBEXECDIR",
            cmake_dir_value_for_prefix(prefix, dirs.libexecdir),
        ),
        ("CMAKE_INSTALL_SYSCONFDIR", dirs.sysconfdir),
        ("CMAKE_INSTALL_LOCALSTATEDIR", dirs.localstatedir),
        ("CMAKE_INSTALL_SHAREDSTATEDIR", dirs.sharedstatedir),
        (
            "CMAKE_INSTALL_INCLUDEDIR",
            cmake_dir_value_for_prefix(prefix, dirs.includedir),
        ),
        (
            "CMAKE_INSTALL_DATAROOTDIR",
            cmake_dir_value_for_prefix(prefix, dirs.datarootdir),
        ),
        (
            "CMAKE_INSTALL_DATADIR",
            cmake_dir_value_for_prefix(prefix, dirs.datadir),
        ),
        (
            "CMAKE_INSTALL_MANDIR",
            cmake_dir_value_for_prefix(prefix, dirs.mandir),
        ),
        (
            "CMAKE_INSTALL_INFODIR",
            cmake_dir_value_for_prefix(prefix, dirs.infodir),
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

fn cmake_depot_sysroot_args(flags: &crate::package::BuildFlags, depot_rootfs: &str) -> Vec<String> {
    let depot_rootfs = depot_rootfs.trim();
    if depot_rootfs.is_empty() || depot_rootfs == "/" {
        return Vec::new();
    }

    let defaults = [
        ("CMAKE_SYSROOT", depot_rootfs.to_string()),
        ("CMAKE_FIND_ROOT_PATH_MODE_PROGRAM", "NEVER".to_string()),
        ("CMAKE_FIND_ROOT_PATH_MODE_LIBRARY", "ONLY".to_string()),
        ("CMAKE_FIND_ROOT_PATH_MODE_INCLUDE", "ONLY".to_string()),
        ("CMAKE_FIND_ROOT_PATH_MODE_PACKAGE", "ONLY".to_string()),
    ];

    defaults
        .into_iter()
        .filter(|(variable, _)| cmake_cache_entry_value(&flags.configure, variable).is_none())
        .map(|(variable, value)| format!("-D{variable}={value}"))
        .collect()
}

fn depot_rootfs_from_env(env_vars: &[(String, String)]) -> &str {
    env_vars
        .iter()
        .find_map(|(key, value)| (key == "DEPOT_ROOTFS").then_some(value.as_str()))
        .unwrap_or("/")
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

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn helper_source_dir() -> PathBuf {
    std::env::var(crate::builder::DEPOT_BUILD_HELPER_SOURCE_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
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
mod tests;
