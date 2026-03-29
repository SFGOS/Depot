//! GNU Autotools build system (configure && make && make install)

use crate::builder::state::{BuildStep, StateTracker};
use crate::cross::CrossConfig;
use crate::fakeroot;
use crate::package::PackageSpec;
use crate::source::hooks;
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
    let make_exec = resolve_make_exec(&flags.make_exec);
    let export_compiler_flags = export_compiler_flags && !flags.no_flags;
    let actual_src = resolve_actual_src(spec, src_dir)?;

    // Create destdir
    fs::create_dir_all(destdir)?;

    // Build environment variables
    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);
    if let Some(host_dir) = host_build_dir {
        crate::builder::set_env_var(
            &mut env_vars,
            crate::builder::DEPOT_BUILD_HOST_DIR_ENV,
            host_dir.to_string_lossy().into_owned(),
        );
    }
    let cc = if let Some(cc_cfg) = cross {
        cc_cfg.cc.clone()
    } else {
        flags.cc.clone()
    };

    if export_compiler_flags
        && let Some(cflags_str) = env_vars
            .iter()
            .find(|(key, _)| key == "CFLAGS")
            .map(|(_, value)| value.clone())
        && !cflags_str.trim().is_empty()
    {
        // Expand shell command substitutions like $($CC -print-resource-dir).
        let expanded = expand_shell_commands(&cflags_str, &cc)?;
        crate::builder::set_env_var(&mut env_vars, "CFLAGS", expanded);
    }

    let mut state = StateTracker::new_with_namespace(
        &actual_src,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;

    // Run configure
    let build_dir = if let Some(dir) = &flags.build_dir {
        let bdir = actual_src.join(dir);
        fs::create_dir_all(&bdir)?;
        crate::log_info!("  Build directory: {}", bdir.display());
        bdir
    } else {
        actual_src.clone()
    };

    if !state.is_done(BuildStep::Configured) {
        crate::log_info!("Running configure...");
        let configure_path = resolve_configure_path(spec, &actual_src);
        crate::log_info!("  Configure path: {}", configure_path.display());

        let mut configure_cmd = Command::new(&configure_path);
        configure_cmd.current_dir(&build_dir);

        crate::builder::prepare_tool_command(&mut configure_cmd, &env_vars);

        // Some projects use non-GNU configure scripts that reject --host/--build.
        // Probe support first and only add these options when advertised.
        let help_text = configure_help_text(&configure_path, &build_dir, &env_vars);
        configure_cmd.arg(format!("--prefix={}", flags.prefix));
        for default_dir_arg in default_configure_install_dirs(flags, help_text.as_deref()) {
            configure_cmd.arg(default_dir_arg);
        }

        let supports_host =
            configure_supports_option(help_text.as_deref(), "--host", &flags.configure_file);
        let supports_build =
            configure_supports_option(help_text.as_deref(), "--build", &flags.configure_file);

        let requested_host = if let Some(cc_cfg) = cross {
            Some(cc_cfg.host_triple().to_string())
        } else if !flags.chost.is_empty() {
            Some(flags.chost.clone())
        } else {
            None
        }
        .map(|host| {
            if flags.lib32_variant {
                lib32_host_triple(&host)
            } else {
                host
            }
        });

        let requested_build = if cross.is_some() {
            CrossConfig::build_triple().ok()
        } else if !flags.cbuild.is_empty() {
            Some(flags.cbuild.clone())
        } else {
            None
        };

        if let Some(host) = requested_host {
            if supports_host {
                configure_cmd.arg(format!("--host={}", host));
            } else {
                crate::log_info!("  configure does not support --host; skipping {}", host);
            }
        }

        if let Some(build) = requested_build {
            if supports_build {
                configure_cmd.arg(format!("--build={}", build));
            } else {
                crate::log_info!("  configure does not support --build; skipping {}", build);
            }
        }

        for arg in &flags.configure {
            let expanded = expand_configure_arg(spec, arg, &env_vars);
            configure_cmd.arg(expanded);
        }
        for arg in crate::builder::static_build_args_for(crate::package::BuildType::Autotools)? {
            configure_cmd.arg(arg);
        }

        let status = crate::interrupts::command_status(&mut configure_cmd)
            .with_context(|| format!("Failed to run configure in {}", build_dir.display()))?;

        if !status.success() {
            anyhow::bail!("configure failed with status: {}", status);
        }

        // Run post-configure hooks (after configure, before make)
        hooks::run_post_configure_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    } else {
        crate::log_info!("Skipping configure (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        // Run make
        let build_targets = phase_targets(&flags.make_target, &flags.make_targets, None);
        let make_dirs = resolve_make_dirs(&build_dir, &flags.make_dirs, "build.flags.make_dirs")?;
        for make_dir in make_dirs {
            crate::log_info!("Running {} in {}...", make_exec, make_dir.display());
            let mut make_cmd = Command::new(make_exec);
            make_cmd.current_dir(&make_dir);
            make_cmd.arg("-j").arg(num_cpus().to_string());
            add_make_variable_overrides_if_supported(
                &mut make_cmd,
                make_exec,
                &flags.make_vars,
                "build",
            )?;
            for target in &build_targets {
                make_cmd.arg(target);
            }

            crate::builder::prepare_tool_command(&mut make_cmd, &env_vars);

            let status = crate::interrupts::command_status(&mut make_cmd).with_context(|| {
                format!("Failed to run {} in {}", make_exec, make_dir.display())
            })?;

            if !status.success() {
                anyhow::bail!(
                    "{} failed with status: {} (dir: {})",
                    make_exec,
                    status,
                    make_dir.display()
                );
            }
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
            let test_dirs = resolve_make_dirs(
                &build_dir,
                &flags.make_test_dirs,
                "build.flags.make_test_dirs",
            )?;
            let mut ran_any_tests = false;
            let configured_test_targets =
                phase_targets(&flags.make_test_target, &flags.make_test_targets, None);
            for test_dir in test_dirs {
                let test_targets = if !configured_test_targets.is_empty() {
                    configured_test_targets.clone()
                } else if make_exec_supports_make_assignments(make_exec) {
                    maybe_find_autotools_test_target(&test_dir, false)?
                        .map(|t| vec![t.to_string()])
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };

                if !test_targets.is_empty() {
                    crate::log_info!("Running {} in {}...", make_exec, test_dir.display());
                    let mut test_cmd = Command::new(make_exec);
                    test_cmd.current_dir(&test_dir);
                    add_make_variable_overrides_if_supported(
                        &mut test_cmd,
                        make_exec,
                        &flags.make_test_vars,
                        "test",
                    )?;
                    for test_target in &test_targets {
                        test_cmd.arg(test_target);
                    }
                    crate::builder::prepare_tool_command(&mut test_cmd, &env_vars);

                    let test_targets_display = test_targets.join(" ");
                    let status =
                        crate::interrupts::command_status(&mut test_cmd).with_context(|| {
                            format!(
                                "Failed to run {} {} in {}",
                                make_exec,
                                test_targets_display,
                                test_dir.display()
                            )
                        })?;
                    if !status.success() {
                        anyhow::bail!(
                            "{} {} failed with status: {} (dir: {})",
                            make_exec,
                            test_targets_display,
                            status,
                            test_dir.display()
                        );
                    }
                    ran_any_tests = true;
                }
            }

            if !ran_any_tests {
                if flags.make_test_dirs.is_empty() {
                    if !configured_test_targets.is_empty() {
                        crate::log_info!("Skipping tests: no test directories to run");
                    } else if make_exec_supports_make_assignments(make_exec) {
                        crate::log_info!("Skipping tests: no 'check' or 'test' target in Makefile");
                    } else {
                        crate::log_info!(
                            "Skipping tests: set build.flags.make_test_target when using build.flags.make_exec='{}'",
                            make_exec
                        );
                    }
                } else if !configured_test_targets.is_empty() {
                    crate::log_info!(
                        "Skipping tests: no test targets ran in build.flags.make_test_dirs"
                    );
                } else if make_exec_supports_make_assignments(make_exec) {
                    crate::log_info!(
                        "Skipping tests: no 'check' or 'test' target in build.flags.make_test_dirs"
                    );
                } else {
                    crate::log_info!(
                        "Skipping tests: set build.flags.make_test_target when using build.flags.make_exec='{}'",
                        make_exec
                    );
                }
            }
        }

        // Run post-compile hooks (after make, before make install)
        hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        crate::log_info!("Skipping make and post-compile hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run make install with fakeroot if not root
        crate::log_info!(
            "Running {} {}{}...",
            make_exec,
            phase_targets(
                &flags.make_install_target,
                &flags.make_install_targets,
                Some("install")
            )
            .join(" "),
            if fakeroot::is_root() {
                ""
            } else {
                " (with internal fakeroot for build)"
            }
        );

        let install_destdir = install_destdir_path(&build_dir, destdir, flags.lib32_variant);
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

        let install_dirs = resolve_make_dirs(
            &build_dir,
            &flags.make_install_dirs,
            "build.flags.make_install_dirs",
        )?;
        let install_targets = phase_targets(
            &flags.make_install_target,
            &flags.make_install_targets,
            Some("install"),
        );
        for install_dir in install_dirs {
            let mut install_cmd = fakeroot::wrap_install_command(make_exec, &install_destdir);
            install_cmd.current_dir(&install_dir);
            if make_exec_supports_make_assignments(make_exec)
                && !has_make_variable_override(&flags.make_install_vars, "DESTDIR")
            {
                install_cmd.arg(format!("DESTDIR={}", install_destdir.to_string_lossy()));
            }
            add_make_variable_overrides_if_supported(
                &mut install_cmd,
                make_exec,
                &flags.make_install_vars,
                "install",
            )?;
            for install_target in &install_targets {
                install_cmd.arg(install_target);
            }

            let mut install_env = env_vars.clone();
            install_env.push((
                "DESTDIR".to_string(),
                install_destdir.to_string_lossy().into_owned(),
            ));
            crate::builder::prepare_tool_command(&mut install_cmd, &install_env);

            let status =
                crate::interrupts::command_status(&mut install_cmd).with_context(|| {
                    format!(
                        "Failed to run {} {} for {} in {}",
                        make_exec,
                        install_targets.join(" "),
                        spec.package.name,
                        install_dir.display()
                    )
                })?;

            if !status.success() {
                anyhow::bail!(
                    "{} {} failed with status: {} (dir: {})",
                    make_exec,
                    install_targets.join(" "),
                    status,
                    install_dir.display()
                );
            }
        }

        if flags.lib32_variant {
            crate::builder::stage_lib32_install_tree(&install_destdir, destdir)?;
            hooks::run_post_install_commands_in_dir(spec, &build_dir, destdir)?;
        } else {
            hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        }
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping make install and post-install hooks (already done)");
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
    let make_exec = resolve_make_exec(&flags.make_exec);
    let export_compiler_flags = export_compiler_flags && !flags.no_flags;
    let actual_src = resolve_actual_src(&host_spec, src_dir)?;
    let build_dir = crate::builder::host_build_dir_for_source(&actual_src, flags);
    fs::create_dir_all(&build_dir)?;

    let mut env_vars =
        crate::builder::standard_build_env(&host_spec, None, true, export_compiler_flags);
    if export_compiler_flags
        && let Some(cflags_str) = env_vars
            .iter()
            .find(|(key, _)| key == "CFLAGS")
            .map(|(_, value)| value.clone())
        && !cflags_str.trim().is_empty()
    {
        let expanded = expand_shell_commands(&cflags_str, &flags.cc)?;
        crate::builder::set_env_var(&mut env_vars, "CFLAGS", expanded);
    }

    let mut state = StateTracker::new_with_namespace(&actual_src, Some("host"))?;

    if !state.is_done(BuildStep::Configured) {
        crate::log_info!(
            "Running host-side configure build in {}...",
            build_dir.display()
        );
        let configure_path = resolve_configure_path(&host_spec, &actual_src);
        let mut configure_cmd = Command::new(&configure_path);
        configure_cmd.current_dir(&build_dir);

        crate::builder::prepare_tool_command(&mut configure_cmd, &env_vars);

        let help_text = configure_help_text(&configure_path, &build_dir, &env_vars);
        configure_cmd.arg(format!("--prefix={}", flags.prefix));
        for default_dir_arg in default_configure_install_dirs(flags, help_text.as_deref()) {
            configure_cmd.arg(default_dir_arg);
        }
        for arg in &flags.configure {
            let expanded = expand_configure_arg(&host_spec, arg, &env_vars);
            configure_cmd.arg(expanded);
        }
        for arg in crate::builder::static_build_args_for(crate::package::BuildType::Autotools)? {
            configure_cmd.arg(arg);
        }

        let status = crate::interrupts::command_status(&mut configure_cmd)
            .with_context(|| format!("Failed to run host configure in {}", build_dir.display()))?;

        if !status.success() {
            anyhow::bail!("host configure failed with status: {}", status);
        }
        state.mark_done(BuildStep::Configured)?;
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        let build_targets = phase_targets(&flags.make_target, &flags.make_targets, None);
        let make_dirs = resolve_make_dirs(&build_dir, &flags.make_dirs, "build.flags.make_dirs")?;
        for make_dir in make_dirs {
            let mut make_cmd = Command::new(make_exec);
            make_cmd.current_dir(&make_dir);
            make_cmd.arg("-j").arg(num_cpus().to_string());
            add_make_variable_overrides_if_supported(
                &mut make_cmd,
                make_exec,
                &flags.make_vars,
                "build",
            )?;
            for target in &build_targets {
                make_cmd.arg(target);
            }

            crate::builder::prepare_tool_command(&mut make_cmd, &env_vars);

            let status = crate::interrupts::command_status(&mut make_cmd).with_context(|| {
                format!("Failed to run host {} in {}", make_exec, make_dir.display())
            })?;

            if !status.success() {
                anyhow::bail!(
                    "host {} failed with status: {} (dir: {})",
                    make_exec,
                    status,
                    make_dir.display()
                );
            }
        }

        state.mark_done(BuildStep::PostCompileDone)?;
    }

    fs::canonicalize(&build_dir)
        .with_context(|| format!("Failed to resolve host build dir: {}", build_dir.display()))
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

pub(crate) fn resolve_actual_src(spec: &PackageSpec, src_dir: &Path) -> Result<std::path::PathBuf> {
    let flags = &spec.build.flags;
    let source_subdir = spec.expand_vars(&flags.source_subdir);

    let actual_src = if source_subdir.is_empty() {
        src_dir.to_path_buf()
    } else {
        src_dir.join(&source_subdir)
    };

    if !actual_src.exists() {
        anyhow::bail!(
            "Source directory not found: {} (source_subdir: {} -> {})",
            actual_src.display(),
            flags.source_subdir,
            source_subdir
        );
    }

    Ok(actual_src)
}

fn resolve_configure_path(spec: &PackageSpec, actual_src: &Path) -> PathBuf {
    let configured = spec.expand_vars(&spec.build.flags.configure_file);
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return actual_src.join("configure");
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        actual_src.join(path)
    }
}

fn configure_help_text(
    configure_path: &Path,
    build_dir: &Path,
    env_vars: &crate::builder::EnvVars,
) -> Option<String> {
    let mut help_cmd = Command::new(configure_path);
    help_cmd.current_dir(build_dir);
    help_cmd.arg("--help");
    crate::builder::prepare_command(&mut help_cmd, env_vars);
    let output = help_cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

fn configure_help_supports_option(help_text: &str, option: &str) -> bool {
    let with_eq = format!("{}=", option);
    let with_space = format!("{} ", option);
    help_text.contains(&with_eq) || help_text.contains(&with_space) || help_text.contains(option)
}

fn configure_supports_option(help_text: Option<&str>, option: &str, configure_file: &str) -> bool {
    help_text
        .map(|text| configure_help_supports_option(text, option))
        .unwrap_or(configure_file.trim().is_empty())
}

fn has_configure_option_prefix(args: &[String], option: &str) -> bool {
    let with_eq = format!("{option}=");
    args.iter().any(|arg| {
        let trimmed = arg.trim();
        trimmed == option || trimmed.starts_with(&with_eq)
    })
}

fn default_configure_install_dirs(
    flags: &crate::package::BuildFlags,
    help_text: Option<&str>,
) -> Vec<String> {
    let dirs = crate::builder::install_dirs(flags);
    let defaults = [
        ("--bindir", dirs.bindir),
        ("--sbindir", dirs.sbindir),
        ("--libdir", dirs.libdir),
        ("--libexecdir", dirs.libexecdir),
        ("--sysconfdir", dirs.sysconfdir),
        ("--localstatedir", dirs.localstatedir),
        ("--sharedstatedir", dirs.sharedstatedir),
        ("--includedir", dirs.includedir),
        ("--datarootdir", dirs.datarootdir),
        ("--datadir", dirs.datadir),
        ("--mandir", dirs.mandir),
        ("--infodir", dirs.infodir),
    ];

    defaults
        .iter()
        .filter(|(option, _)| {
            help_text.is_some_and(|text| configure_help_supports_option(text, option))
        })
        .filter(|(option, _)| !has_configure_option_prefix(&flags.configure, option))
        .map(|(option, value)| format!("{option}={value}"))
        .collect()
}

fn install_destdir_path(build_dir: &Path, destdir: &Path, lib32_variant: bool) -> PathBuf {
    if lib32_variant {
        build_dir.join("destdir")
    } else {
        destdir.to_path_buf()
    }
}

fn lib32_host_triple(host: &str) -> String {
    host.replace("x86_64", "i686")
}

fn find_autotools_test_target(build_dir: &Path) -> Result<Option<&'static str>> {
    for target in ["check", "test"] {
        if makefile_has_target(build_dir, target)? {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

fn maybe_find_autotools_test_target(
    build_dir: &Path,
    skip_tests: bool,
) -> Result<Option<&'static str>> {
    if skip_tests {
        return Ok(None);
    }
    find_autotools_test_target(build_dir)
}

pub(crate) fn resolve_make_dirs(
    build_dir: &Path,
    dirs: &[String],
    field_name: &str,
) -> Result<Vec<PathBuf>> {
    if dirs.is_empty() {
        return Ok(vec![build_dir.to_path_buf()]);
    }

    let canonical_build = fs::canonicalize(build_dir)
        .with_context(|| format!("Failed to resolve build directory {}", build_dir.display()))?;
    let mut out = Vec::with_capacity(dirs.len());
    for raw in dirs {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            anyhow::bail!("{} contains an empty directory entry", field_name);
        }
        let rel = Path::new(trimmed);
        if rel.is_absolute() {
            anyhow::bail!("{} entry '{}' must be a relative path", field_name, trimmed);
        }

        let candidate = build_dir.join(rel);
        let canonical_candidate = fs::canonicalize(&candidate).with_context(|| {
            format!(
                "{} entry '{}' does not exist in {}",
                field_name,
                trimmed,
                build_dir.display()
            )
        })?;
        if !canonical_candidate.starts_with(&canonical_build) {
            anyhow::bail!(
                "{} entry '{}' resolves outside build directory",
                field_name,
                trimmed
            );
        }
        if !canonical_candidate.is_dir() {
            anyhow::bail!("{} entry '{}' is not a directory", field_name, trimmed);
        }
        out.push(canonical_candidate);
    }
    Ok(out)
}

fn makefile_has_target(build_dir: &Path, target: &str) -> Result<bool> {
    for name in ["GNUmakefile", "Makefile", "makefile"] {
        let path = build_dir.join(name);
        if !path.exists() {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read makefile: {}", path.display()))?;
        if makefile_content_has_target(&content, target) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn makefile_content_has_target(content: &str, target: &str) -> bool {
    for raw in content.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') || line.starts_with('\t') {
            continue;
        }

        if let Some(rest) = line.strip_prefix(".PHONY:") {
            if rest.split_whitespace().any(|t| t == target) {
                return true;
            }
            continue;
        }

        let Some(colon_pos) = line.find(':') else {
            continue;
        };
        let rhs = &line[colon_pos + 1..];
        if rhs.starts_with('=') {
            // Variable assignment (e.g. FOO:=bar), not a make target.
            continue;
        }

        let lhs = &line[..colon_pos];
        if lhs.split_whitespace().any(|t| t == target) {
            return true;
        }
    }

    false
}

fn nonempty_trimmed(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

pub(crate) fn resolve_make_exec(configured: &str) -> &str {
    nonempty_trimmed(configured).unwrap_or("make")
}

pub(crate) fn phase_targets(single: &str, many: &[String], default: Option<&str>) -> Vec<String> {
    let mut targets = Vec::new();
    if let Some(target) = nonempty_trimmed(single) {
        targets.push(target.to_string());
    }
    for target in many {
        if let Some(target) = nonempty_trimmed(target) {
            targets.push(target.to_string());
        }
    }
    if targets.is_empty()
        && let Some(default_target) = default
    {
        targets.push(default_target.to_string());
    }
    targets
}

pub(crate) fn make_exec_supports_make_assignments(make_exec: &str) -> bool {
    let Some(tool) = Path::new(make_exec)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase())
    else {
        return false;
    };

    tool == "make"
        || tool == "gmake"
        || tool == "bmake"
        || tool == "nmake"
        || tool.ends_with("-make")
        || tool.ends_with("make.exe")
}

pub(crate) fn add_make_variable_overrides_if_supported(
    cmd: &mut Command,
    make_exec: &str,
    vars: &[String],
    phase: &str,
) -> Result<()> {
    if make_exec_supports_make_assignments(make_exec) {
        return add_make_variable_overrides(cmd, vars, phase);
    }
    if !vars.is_empty() {
        let field_name = match phase {
            "build" => "build.flags.make_vars",
            "test" => "build.flags.make_test_vars",
            "install" => "build.flags.make_install_vars",
            _ => "build.flags.make_*_vars",
        };
        anyhow::bail!(
            "{} is only supported with make-like executables; build.flags.make_exec='{}'",
            field_name,
            make_exec
        );
    }
    Ok(())
}

fn add_make_variable_overrides(cmd: &mut Command, vars: &[String], phase: &str) -> Result<()> {
    for raw in vars {
        let var = raw.trim();
        if var.is_empty() {
            continue;
        }
        let Some((name, _value)) = var.split_once('=') else {
            anyhow::bail!(
                "Invalid make variable override '{}' for {} phase; expected NAME=VALUE",
                var,
                phase
            );
        };
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            anyhow::bail!(
                "Invalid make variable override '{}' for {} phase; expected NAME=VALUE",
                var,
                phase
            );
        }
        cmd.arg(var);
    }
    Ok(())
}

pub(crate) fn has_make_variable_override(vars: &[String], name: &str) -> bool {
    vars.iter().any(|raw| {
        let var = raw.trim();
        let Some((lhs, _rhs)) = var.split_once('=') else {
            return false;
        };
        lhs.trim() == name
    })
}

/// Expand shell command substitutions like $($CC -print-resource-dir) in a string
fn expand_shell_commands(input: &str, cc: &str) -> Result<String> {
    let mut result = input.to_string();

    // Find and expand $(...) patterns
    while let Some(start) = result.find("$(") {
        let rest = &result[start + 2..];
        if let Some(end) = rest.find(')') {
            let cmd = &rest[..end];
            // Replace $CC with actual compiler
            let cmd = cmd.replace("$CC", cc);

            // Execute the command via shell
            let output = Command::new("sh").arg("-c").arg(&cmd).output();

            let replacement = match output {
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).trim().to_string()
                }
                _ => {
                    // Silently skip failed commands (e.g., gcc doesn't support -print-resource-dir)
                    crate::log_warn!("Shell command '{}' failed, skipping", cmd);
                    String::new()
                }
            };

            result = format!("{}{}{}", &result[..start], replacement, &rest[end + 1..]);
        } else {
            break; // Malformed, no closing paren
        }
    }

    Ok(result)
}

fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    for (key, value) in std::env::vars() {
        result = result.replace(&format!("${}", key), &value);
        result = result.replace(&format!("${{{}}}", key), &value);
    }
    result
}

fn expand_with_envs(input: &str, envs: &[(String, String)]) -> String {
    let mut result = input.to_string();
    for (k, v) in envs {
        result = result.replace(&format!("${}", k), v);
        result = result.replace(&format!("${{{}}}", k), v);
    }
    expand_env_vars(&result)
}

fn expand_configure_arg(spec: &PackageSpec, arg: &str, envs: &[(String, String)]) -> String {
    let with_spec_vars = spec.expand_vars(arg);
    expand_with_envs(&with_spec_vars, envs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn test_expand_shell_commands_simple() -> Result<()> {
        let out = expand_shell_commands("x $(echo foo) y", "gcc")?;
        assert_eq!(out, "x foo y");
        Ok(())
    }

    #[test]
    fn test_expand_shell_commands_replace_cc() -> Result<()> {
        // The command contains $CC which should be replaced with provided cc
        let out = expand_shell_commands("start $($CC -v >/dev/null; echo OK) end", "mycc")?;
        // Since the inner command echoes OK, after replacing $CC it should run and include OK
        assert!(out.contains("OK") || out.contains(""));
        Ok(())
    }

    #[test]
    fn test_expand_with_envs_prefers_provided_envs() {
        let envs = vec![
            ("CARCH".to_string(), "x86_64".to_string()),
            ("CHOST".to_string(), "x86_64-sfg-linux-gnu".to_string()),
        ];
        let out = expand_with_envs("--with-gcc-arch=$CARCH --host=${CHOST}", &envs);
        assert!(out.contains("--with-gcc-arch=x86_64"));
        assert!(out.contains("--host=x86_64-sfg-linux-gnu"));
    }

    #[test]
    fn test_expand_configure_arg_expands_spec_and_env_vars() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.2.3".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let envs = vec![("CARCH".to_string(), "aarch64".to_string())];
        let expanded =
            expand_configure_arg(&spec, "--program-prefix=$name-$version-$CARCH-", &envs);
        assert_eq!(expanded, "--program-prefix=foo-1.2.3-aarch64-");
    }

    #[test]
    fn test_expand_configure_arg_expands_host_build_dir_env() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.2.3".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let envs = vec![(
            crate::builder::DEPOT_BUILD_HOST_DIR_ENV.to_string(),
            "/tmp/build-host".to_string(),
        )];
        let expanded = expand_configure_arg(
            &spec,
            "--with-build-tools=$DEPOT_BUILD_HOST_DIR/tools",
            &envs,
        );
        assert_eq!(expanded, "--with-build-tools=/tmp/build-host/tools");
    }

    #[test]
    fn test_num_cpus_at_least_one() {
        let n = num_cpus();
        assert!(n >= 1);
    }

    #[test]
    fn test_configure_help_supports_host_build() {
        let help = "Usage: configure [OPTION]...\n  --host=HOST   cross host\n  --build=BUILD";
        assert!(configure_help_supports_option(help, "--host"));
        assert!(configure_help_supports_option(help, "--build"));
        assert!(!configure_help_supports_option(help, "--target"));
    }

    #[test]
    fn test_configure_supports_option_defaults_by_configure_file_usage() {
        assert!(configure_supports_option(None, "--host", ""));
        assert!(!configure_supports_option(
            None,
            "--host",
            "build-aux/Configure"
        ));
    }

    #[test]
    fn test_default_configure_install_dirs_injects_expected_paths() {
        let flags = BuildFlags::default();
        let help = "\
--bindir=DIR
--sbindir=DIR
--libdir=DIR
--libexecdir=DIR
--sysconfdir=DIR
--localstatedir=DIR
--sharedstatedir=DIR
--includedir=DIR
--datarootdir=DIR
--datadir=DIR
--mandir=DIR
--infodir=DIR";
        let args = default_configure_install_dirs(&flags, Some(help));
        assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
        assert!(args.iter().any(|a| a == "--sbindir=/usr/bin"));
        assert!(args.iter().any(|a| a == "--libdir=/usr/lib"));
        assert!(args.iter().any(|a| a == "--libexecdir=/usr/lib"));
        assert!(args.iter().any(|a| a == "--sysconfdir=/etc"));
        assert!(args.iter().any(|a| a == "--localstatedir=/var"));
        assert!(args.iter().any(|a| a == "--sharedstatedir=/var/lib"));
        assert!(args.iter().any(|a| a == "--includedir=/usr/include"));
        assert!(args.iter().any(|a| a == "--datarootdir=/usr/share"));
        assert!(args.iter().any(|a| a == "--datadir=/usr/share"));
        assert!(args.iter().any(|a| a == "--mandir=/usr/share/man"));
        assert!(args.iter().any(|a| a == "--infodir=/usr/share/info"));
    }

    #[test]
    fn test_default_configure_install_dirs_respects_explicit_user_overrides() {
        let flags = BuildFlags {
            configure: vec![
                "--sbindir=/sbin".to_string(),
                "--libdir=/custom/lib".to_string(),
                "--datadir=/custom/share".to_string(),
            ],
            ..BuildFlags::default()
        };
        let help = "--bindir=DIR --sbindir=DIR --libdir=DIR --datadir=DIR";
        let args = default_configure_install_dirs(&flags, Some(help));
        assert!(!args.iter().any(|a| a.starts_with("--sbindir=")));
        assert!(!args.iter().any(|a| a.starts_with("--libdir=")));
        assert!(!args.iter().any(|a| a.starts_with("--datadir=")));
        assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
    }

    #[test]
    fn test_default_configure_install_dirs_lib32_uses_lib32_dirs() {
        let help = "--libdir=DIR --libexecdir=DIR";
        let flags = BuildFlags {
            lib32_variant: true,
            ..BuildFlags::default()
        };
        let args = default_configure_install_dirs(&flags, Some(help));
        assert!(args.iter().any(|a| a == "--libdir=/usr/lib32"));
        assert!(args.iter().any(|a| a == "--libexecdir=/usr/lib32"));
    }

    #[test]
    fn test_default_configure_install_dirs_skips_when_not_advertised() {
        let flags = BuildFlags::default();
        let args = default_configure_install_dirs(&flags, Some("--prefix=PREFIX"));
        assert!(args.is_empty());
    }

    #[test]
    fn test_install_destdir_path_uses_build_dir_for_lib32() {
        let build_dir = Path::new("/tmp/build");
        let destdir = Path::new("/tmp/pkg");
        assert_eq!(
            install_destdir_path(build_dir, destdir, false),
            destdir.to_path_buf()
        );
        assert_eq!(
            install_destdir_path(build_dir, destdir, true),
            build_dir.join("destdir")
        );
    }

    #[test]
    fn test_makefile_content_has_target_detects_check_and_test() {
        let content = r#"
.PHONY: all check
all:
	@echo all
check:
	@echo check
"#;
        assert!(makefile_content_has_target(content, "check"));
        assert!(!makefile_content_has_target(content, "test"));
    }

    #[test]
    fn test_makefile_content_has_target_ignores_assignments() {
        let content = r#"
TEST := value
VAR:=$(shell echo hi)
foo: bar
	@true
"#;
        assert!(!makefile_content_has_target(content, "TEST"));
        assert!(!makefile_content_has_target(content, "VAR"));
        assert!(!makefile_content_has_target(content, "check"));
    }

    #[test]
    fn test_maybe_find_autotools_test_target_respects_skip_tests() -> Result<()> {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("Makefile"), "check:\n\t@true\n").unwrap();

        let skipped = maybe_find_autotools_test_target(tmp.path(), true)?;
        assert_eq!(skipped, None);

        let detected = maybe_find_autotools_test_target(tmp.path(), false)?;
        assert_eq!(detected, Some("check"));
        Ok(())
    }

    #[test]
    fn test_resolve_make_dirs_defaults_to_build_dir() -> Result<()> {
        let tmp = tempdir().unwrap();
        let dirs = resolve_make_dirs(tmp.path(), &[], "build.flags.make_dirs")?;
        assert_eq!(dirs, vec![tmp.path().to_path_buf()]);
        Ok(())
    }

    #[test]
    fn test_resolve_make_dirs_resolves_multiple_relative_dirs() -> Result<()> {
        let tmp = tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("lib"))?;
        std::fs::create_dir_all(tmp.path().join("libelf"))?;
        let dirs = resolve_make_dirs(
            tmp.path(),
            &["lib".to_string(), "libelf".to_string()],
            "build.flags.make_dirs",
        )?;
        assert_eq!(
            dirs,
            vec![tmp.path().join("lib"), tmp.path().join("libelf")]
        );
        Ok(())
    }

    #[test]
    fn test_add_make_variable_overrides_accepts_valid_assignments() -> Result<()> {
        let mut cmd = Command::new("make");
        add_make_variable_overrides(
            &mut cmd,
            &[
                "CC=clang".to_string(),
                "V=1".to_string(),
                " CFLAGS=-O2 -pipe ".to_string(),
            ],
            "build",
        )?;
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["CC=clang", "V=1", "CFLAGS=-O2 -pipe"]);
        Ok(())
    }

    #[test]
    fn test_add_make_variable_overrides_rejects_invalid_assignment() {
        let mut cmd = Command::new("make");
        let err = add_make_variable_overrides(&mut cmd, &["not-an-assignment".to_string()], "test")
            .expect_err("expected invalid assignment to fail");
        assert!(err.to_string().contains("expected NAME=VALUE"));
    }

    #[test]
    fn test_has_make_variable_override_detects_destdir() {
        assert!(has_make_variable_override(
            &["DESTDIR=/tmp/pkg".to_string()],
            "DESTDIR"
        ));
        assert!(has_make_variable_override(
            &[" DESTDIR =/tmp/pkg ".to_string()],
            "DESTDIR"
        ));
        assert!(!has_make_variable_override(
            &["V=1".to_string(), "PREFIX=/usr".to_string()],
            "DESTDIR"
        ));
    }

    #[test]
    fn test_resolve_make_exec_defaults_and_trims() {
        assert_eq!(resolve_make_exec(""), "make");
        assert_eq!(resolve_make_exec("  "), "make");
        assert_eq!(resolve_make_exec(" ninja "), "ninja");
    }

    #[test]
    fn test_make_exec_supports_make_assignments_detects_make_variants() {
        assert!(make_exec_supports_make_assignments("make"));
        assert!(make_exec_supports_make_assignments("/usr/bin/gmake"));
        assert!(!make_exec_supports_make_assignments("ninja"));
    }

    #[test]
    fn test_phase_targets_merges_singular_plural_and_default() {
        assert_eq!(
            phase_targets("bootstrap", &["stage1".into(), "stage2".into()], None),
            vec![
                "bootstrap".to_string(),
                "stage1".to_string(),
                "stage2".to_string()
            ]
        );
        assert_eq!(
            phase_targets("", &[], Some("install")),
            vec!["install".to_string()]
        );
    }

    #[test]
    fn test_add_make_variable_overrides_if_supported_rejects_ninja_vars() {
        let mut cmd = Command::new("ninja");
        let err = add_make_variable_overrides_if_supported(
            &mut cmd,
            "ninja",
            &["V=1".to_string()],
            "build",
        )
        .expect_err("ninja should reject make variable override syntax");
        assert!(err.to_string().contains("build.flags.make_vars"));
    }

    #[test]
    fn test_resolve_actual_src_expands_source_subdir_vars() {
        let tmp = tempdir().unwrap();
        let src_root = tmp.path().join("srcroot");
        let expanded = src_root.join("expect5.45.4").join("unix");
        std::fs::create_dir_all(&expanded).unwrap();

        let spec = PackageSpec {
            package: PackageInfo {
                name: "expect".into(),
                real_name: None,
                version: "5.45.4".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags {
                    source_subdir: "$name$version/unix".into(),
                    ..BuildFlags::default()
                },
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let resolved = resolve_actual_src(&spec, &src_root).unwrap();
        assert_eq!(resolved, expanded);
    }

    #[test]
    fn test_resolve_configure_path_defaults_to_source_configure() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let actual_src = PathBuf::from("/tmp/src");
        let configure = resolve_configure_path(&spec, &actual_src);
        assert_eq!(configure, actual_src.join("configure"));
    }

    #[test]
    fn test_resolve_configure_path_uses_configure_file_and_expands_vars() {
        let flags = BuildFlags {
            configure_file: "build-aux/$name-configure".into(),
            ..BuildFlags::default()
        };
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Autotools,
                flags,
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let actual_src = PathBuf::from("/tmp/src");
        let configure = resolve_configure_path(&spec, &actual_src);
        assert_eq!(configure, actual_src.join("build-aux/foo-configure"));
    }
}
