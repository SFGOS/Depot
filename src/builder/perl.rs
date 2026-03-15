//! Perl MakeMaker build system (`perl Makefile.PL && make && make test && make install`)

use crate::builder::autotools;
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
) -> Result<()> {
    let flags = &spec.build.flags;
    if flags.build_dir.is_some() {
        anyhow::bail!("build.flags.build_dir is not supported for build.type = 'perl'");
    }

    let actual_src = autotools::resolve_actual_src(spec, src_dir)?;
    let make_exec = autotools::resolve_make_exec(&flags.make_exec);
    let export_compiler_flags = export_compiler_flags && !flags.no_flags;

    fs::create_dir_all(destdir)?;

    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);
    crate::builder::set_env_var(&mut env_vars, "PERL_MM_USE_DEFAULT", "1");
    if !flags.rootfs.is_empty() && flags.rootfs != "/" {
        crate::builder::set_env_var(
            &mut env_vars,
            "PKG_CONFIG_SYSROOT_DIR",
            flags.rootfs.clone(),
        );
    }

    let mut state = StateTracker::new_with_namespace(
        &actual_src,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;

    if !state.is_done(BuildStep::Configured) {
        let configure_script = resolve_perl_configure_script(spec, &actual_src);
        crate::log_info!("Running perl {}...", configure_script.display());

        let mut configure_cmd =
            Command::new(resolved_command_path("perl").unwrap_or_else(|| PathBuf::from("perl")));
        configure_cmd.current_dir(&actual_src);
        configure_cmd.arg(&configure_script);
        if !has_assignment_prefix(&flags.configure, "INSTALLDIRS") {
            configure_cmd.arg("INSTALLDIRS=vendor");
        }
        for arg in &flags.configure {
            configure_cmd.arg(spec.expand_vars(arg));
        }
        crate::builder::prepare_tool_command(&mut configure_cmd, &env_vars);

        let status = command_status_with_sh_fallback(&mut configure_cmd).with_context(|| {
            format!(
                "Failed to run perl {} in {}",
                configure_script.display(),
                actual_src.display()
            )
        })?;
        if !status.success() {
            anyhow::bail!("perl Makefile.PL failed with status: {}", status);
        }

        hooks::run_post_configure_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    } else {
        crate::log_info!("Skipping perl configure (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        let build_dirs =
            autotools::resolve_make_dirs(&actual_src, &flags.make_dirs, "build.flags.make_dirs")?;
        let build_targets = autotools::phase_targets(&flags.make_target, &flags.make_targets, None);

        for build_dir in build_dirs {
            crate::log_info!("Running {} in {}...", make_exec, build_dir.display());
            let mut make_cmd = Command::new(make_exec);
            make_cmd.current_dir(&build_dir);
            make_cmd.arg("-j").arg(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
                    .to_string(),
            );
            autotools::add_make_variable_overrides_if_supported(
                &mut make_cmd,
                make_exec,
                &flags.make_vars,
                "build",
            )?;
            for target in &build_targets {
                make_cmd.arg(target);
            }
            crate::builder::prepare_tool_command(&mut make_cmd, &env_vars);

            let status = command_status_with_sh_fallback(&mut make_cmd).with_context(|| {
                format!("Failed to run {} in {}", make_exec, build_dir.display())
            })?;
            if !status.success() {
                anyhow::bail!(
                    "{} failed with status: {} (dir: {})",
                    make_exec,
                    status,
                    build_dir.display()
                );
            }
        }

        if flags.skip_tests {
            crate::log_info!("Skipping tests: disabled by build.flags.skip_tests");
        } else {
            let test_dirs = autotools::resolve_make_dirs(
                &actual_src,
                &flags.make_test_dirs,
                "build.flags.make_test_dirs",
            )?;
            let test_targets = autotools::phase_targets(
                &flags.make_test_target,
                &flags.make_test_targets,
                Some("test"),
            );
            for test_dir in test_dirs {
                crate::log_info!(
                    "Running {} {} in {}...",
                    make_exec,
                    test_targets.join(" "),
                    test_dir.display()
                );
                let mut test_cmd = Command::new(make_exec);
                test_cmd.current_dir(&test_dir);
                autotools::add_make_variable_overrides_if_supported(
                    &mut test_cmd,
                    make_exec,
                    &flags.make_test_vars,
                    "test",
                )?;
                for target in &test_targets {
                    test_cmd.arg(target);
                }
                crate::builder::prepare_tool_command(&mut test_cmd, &env_vars);

                let status = command_status_with_sh_fallback(&mut test_cmd).with_context(|| {
                    format!(
                        "Failed to run {} {} in {}",
                        make_exec,
                        test_targets.join(" "),
                        test_dir.display()
                    )
                })?;
                if !status.success() {
                    anyhow::bail!(
                        "{} {} failed with status: {} (dir: {})",
                        make_exec,
                        test_targets.join(" "),
                        status,
                        test_dir.display()
                    );
                }
            }
        }

        hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        crate::log_info!("Skipping perl build and tests (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        let install_dirs = autotools::resolve_make_dirs(
            &actual_src,
            &flags.make_install_dirs,
            "build.flags.make_install_dirs",
        )?;
        let install_targets = autotools::phase_targets(
            &flags.make_install_target,
            &flags.make_install_targets,
            Some("install"),
        );

        for install_dir in install_dirs {
            crate::log_info!(
                "Running {} {}{}...",
                make_exec,
                install_targets.join(" "),
                if fakeroot::is_root() {
                    ""
                } else {
                    " (with internal fakeroot for build)"
                }
            );
            let mut install_cmd = fakeroot::wrap_install_command(make_exec, destdir);
            install_cmd.current_dir(&install_dir);
            if autotools::make_exec_supports_make_assignments(make_exec)
                && !autotools::has_make_variable_override(&flags.make_install_vars, "DESTDIR")
            {
                install_cmd.arg(format!("DESTDIR={}", destdir.to_string_lossy()));
            }
            autotools::add_make_variable_overrides_if_supported(
                &mut install_cmd,
                make_exec,
                &flags.make_install_vars,
                "install",
            )?;
            for target in &install_targets {
                install_cmd.arg(target);
            }

            let mut install_env = env_vars.clone();
            install_env.push((
                "DESTDIR".to_string(),
                destdir.to_string_lossy().into_owned(),
            ));
            crate::builder::prepare_tool_command(&mut install_cmd, &install_env);

            let status = command_status_with_sh_fallback(&mut install_cmd).with_context(|| {
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

        hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping perl install (already done)");
    }

    Ok(())
}

fn resolve_perl_configure_script(spec: &PackageSpec, actual_src: &Path) -> PathBuf {
    let configured = spec.expand_vars(&spec.build.flags.configure_file);
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        return actual_src.join("Makefile.PL");
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        actual_src.join(path)
    }
}

fn command_status_with_sh_fallback(cmd: &mut Command) -> std::io::Result<std::process::ExitStatus> {
    match crate::interrupts::command_status(cmd) {
        Ok(status) => Ok(status),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let Some(script) = resolved_script_path(cmd) else {
                return Err(err);
            };
            let contents = fs::read(&script);
            let is_script = contents.ok().is_some_and(|bytes| bytes.starts_with(b"#!"));
            if !is_script {
                return Err(err);
            }

            let mut fallback = Command::new("sh");
            fallback.arg(&script);
            fallback.args(cmd.get_args());
            if let Some(dir) = cmd.get_current_dir() {
                fallback.current_dir(dir);
            }
            fallback.env_clear();
            for (key, value) in cmd.get_envs() {
                match value {
                    Some(value) => {
                        fallback.env(key, value);
                    }
                    None => {
                        fallback.env_remove(key);
                    }
                }
            }
            crate::interrupts::command_status(&mut fallback)
        }
        Err(err) => Err(err),
    }
}

fn resolved_script_path(cmd: &Command) -> Option<PathBuf> {
    let program = Path::new(cmd.get_program());
    if program.components().count() > 1 || program.is_absolute() {
        return Some(program.to_path_buf());
    }

    let path_value = command_path_env(cmd)?;
    std::env::split_paths(&path_value)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn resolved_command_path(program: &str) -> Option<PathBuf> {
    let path_value = std::env::var_os("PATH")?;
    std::env::split_paths(&path_value)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn command_path_env(cmd: &Command) -> Option<std::ffi::OsString> {
    cmd.get_envs()
        .find_map(|(key, value)| (key == "PATH").then_some(value))
        .flatten()
        .map(|value| value.to_os_string())
        .or_else(|| std::env::var_os("PATH"))
}

fn has_assignment_prefix(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| {
        let trimmed = arg.trim();
        trimmed == name || trimmed.starts_with(&format!("{name}="))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo};
    use crate::test_support::TestEnv;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn mk_spec(name: &str, version: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: name.into(),
                version: version.into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "h".into(),
                sha256: "s".into(),
                extract_dir: "e".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Perl,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    fn write_executable(path: &Path, content: &str) -> Result<()> {
        fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
        Ok(())
    }

    #[test]
    fn perl_build_runs_makefile_pl_make_test_and_install() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;
        let tmp_tools = tempdir()?;
        let src_path = tmp_src.path();
        let dest_path = tmp_dest.path();
        let tools_path = tmp_tools.path();

        fs::write(src_path.join("Makefile.PL"), "print qq(configure\\n);")?;

        write_executable(
            &tools_path.join("perl"),
            r#"#!/bin/sh
printf '%s\n' "$@" > perl-args.log
touch Makefile
touch configured.txt
"#,
        )?;
        write_executable(
            &tools_path.join("make"),
            r#"#!/bin/sh
printf '%s\n' "$*" >> make-invocations.log
destdir="${DESTDIR:-}"
target=""
for arg in "$@"; do
    case "$arg" in
        DESTDIR=*) destdir="${arg#DESTDIR=}" ;;
        test|install) target="$arg" ;;
    esac
done
if [ -z "$target" ]; then
    target="build"
fi
case "$target" in
    build)
        echo built > built.txt
        ;;
    test)
        echo tested > tested.txt
        ;;
    install)
        [ -n "$destdir" ] || exit 11
        mkdir -p "$destdir/usr/lib/perl5/vendor_perl"
        cp built.txt "$destdir/usr/lib/perl5/vendor_perl/installed.txt"
        ;;
esac
"#,
        )?;
        write_executable(
            &tools_path.join("fakeroot"),
            r#"#!/bin/sh
if [ "$1" = "--" ]; then
    shift
fi
exec "$@"
"#,
        )?;

        let mut env = TestEnv::new();
        let old_path = std::env::var("PATH").unwrap_or_default();
        env.set_var("PATH", format!("{}:{}", tools_path.display(), old_path));

        let mut spec = mk_spec("perl-test", "1.0");
        spec.build.flags.make_exec = tools_path.join("make").to_string_lossy().into_owned();
        spec.build.flags.configure = vec!["CCFLAGS=-fPIC".into()];

        let build_result = build(&spec, src_path, dest_path, None, true);

        build_result?;

        let perl_args = fs::read_to_string(src_path.join("perl-args.log"))?;
        assert!(perl_args.contains("Makefile.PL"));
        assert!(perl_args.contains("INSTALLDIRS=vendor"));
        assert!(perl_args.contains("CCFLAGS=-fPIC"));
        assert!(src_path.join("built.txt").exists());
        assert!(src_path.join("tested.txt").exists());
        assert!(
            dest_path
                .join("usr/lib/perl5/vendor_perl/installed.txt")
                .exists()
        );

        let state_tracker = StateTracker::new(src_path)?;
        assert!(state_tracker.is_done(BuildStep::Configured));
        assert!(state_tracker.is_done(BuildStep::PostCompileDone));
        assert!(state_tracker.is_done(BuildStep::PostInstallDone));

        Ok(())
    }

    #[test]
    fn perl_build_rejects_build_dir() {
        let tmp_src = tempdir().unwrap();
        let tmp_dest = tempdir().unwrap();
        let mut spec = mk_spec("perl-test", "1.0");
        spec.build.flags.build_dir = Some("build".into());

        let err = build(&spec, tmp_src.path(), tmp_dest.path(), None, true)
            .expect_err("perl build should reject build_dir");
        assert!(
            err.to_string()
                .contains("build.flags.build_dir is not supported")
        );
    }
}
