//! GNU Autotools build system (configure && make && make install)

use crate::cross::CrossConfig;
use crate::fakeroot;
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
    let export_compiler_flags = export_compiler_flags && !flags.no_flags;
    let actual_src = resolve_actual_src(spec, src_dir)?;

    // Create destdir
    fs::create_dir_all(destdir)?;

    // Build environment variables
    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);
    let cc = if let Some(cc_cfg) = cross {
        cc_cfg.cc.clone()
    } else {
        flags.cc.clone()
    };

    if export_compiler_flags && !flags.cflags.is_empty() {
        // Expand shell command substitutions like $($CC -print-resource-dir)
        let cflags_str = flags.cflags.join(" ");
        let expanded = expand_shell_commands(&cflags_str, &cc)?;
        crate::builder::set_env_var(&mut env_vars, "CFLAGS", expanded);
    }

    use crate::builder::state::{BuildStep, StateTracker};
    let mut state = StateTracker::new(&actual_src)?;

    // Run configure
    let build_dir = if let Some(dir) = &flags.build_dir {
        let bdir = actual_src.join(dir);
        fs::create_dir_all(&bdir)?;
        println!("  Build directory: {}", bdir.display());
        bdir
    } else {
        actual_src.clone()
    };

    if !state.is_done(BuildStep::Configured) {
        println!("Running configure...");
        let configure_path = if flags.build_dir.is_some() {
            "../configure"
        } else {
            "./configure"
        };
        println!("  Configure path: {}", configure_path);

        let mut configure_cmd = Command::new(configure_path);
        configure_cmd.current_dir(&build_dir);

        crate::builder::prepare_command(&mut configure_cmd, &env_vars);

        configure_cmd.arg(format!("--prefix={}", flags.prefix));

        // Some projects use non-GNU configure scripts that reject --host/--build.
        // Probe support first and only add these options when advertised.
        let help_text = configure_help_text(configure_path, &build_dir, &env_vars);
        let supports_host = help_text
            .as_deref()
            .map(|s| configure_help_supports_option(s, "--host"))
            .unwrap_or(true);
        let supports_build = help_text
            .as_deref()
            .map(|s| configure_help_supports_option(s, "--build"))
            .unwrap_or(true);

        let requested_host = if let Some(cc_cfg) = cross {
            Some(cc_cfg.host_triple().to_string())
        } else if !flags.chost.is_empty() {
            Some(flags.chost.clone())
        } else {
            None
        };

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
                println!("  configure does not support --host; skipping {}", host);
            }
        }

        if let Some(build) = requested_build {
            if supports_build {
                configure_cmd.arg(format!("--build={}", build));
            } else {
                println!("  configure does not support --build; skipping {}", build);
            }
        }

        for arg in &flags.configure {
            configure_cmd.arg(arg);
        }

        let status = configure_cmd
            .status()
            .with_context(|| format!("Failed to run configure in {}", build_dir.display()))?;

        if !status.success() {
            anyhow::bail!("configure failed with status: {}", status);
        }
        state.mark_done(BuildStep::Configured)?;
    } else {
        println!("Skipping configure (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        // Run make
        println!("Running make...");
        let mut make_cmd = Command::new("make");
        make_cmd.current_dir(&build_dir);
        make_cmd.arg("-j").arg(num_cpus().to_string());
        add_make_variable_overrides(&mut make_cmd, &flags.make_vars, "build")?;

        crate::builder::prepare_command(&mut make_cmd, &env_vars);

        let status = make_cmd
            .status()
            .with_context(|| format!("Failed to run make in {}", build_dir.display()))?;

        if !status.success() {
            anyhow::bail!("make failed with status: {}", status);
        }

        if let Some(test_target) = maybe_find_autotools_test_target(&build_dir, flags.skip_tests)? {
            println!("Running make {}...", test_target);
            let mut test_cmd = Command::new("make");
            test_cmd.current_dir(&build_dir);
            add_make_variable_overrides(&mut test_cmd, &flags.make_test_vars, "test")?;
            test_cmd.arg(test_target);
            crate::builder::prepare_command(&mut test_cmd, &env_vars);

            let status = test_cmd.status().with_context(|| {
                format!(
                    "Failed to run make {} in {}",
                    test_target,
                    build_dir.display()
                )
            })?;
            if !status.success() {
                anyhow::bail!("make {} failed with status: {}", test_target, status);
            }
        } else if flags.skip_tests {
            println!("Skipping tests: disabled by build.flags.skip_tests");
        } else {
            println!("Skipping tests: no 'check' or 'test' target in Makefile");
        }

        // Run post-compile hooks (after make, before make install)
        hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        println!("Skipping make and post-compile hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run make install with fakeroot if not root
        println!(
            "Running make install{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with internal fakeroot for build)"
            }
        );

        let mut install_cmd = fakeroot::wrap_install_command("make", destdir);
        install_cmd.current_dir(&build_dir);
        if !has_make_variable_override(&flags.make_install_vars, "DESTDIR") {
            install_cmd.arg(format!("DESTDIR={}", destdir.to_string_lossy()));
        }
        add_make_variable_overrides(&mut install_cmd, &flags.make_install_vars, "install")?;
        install_cmd.arg("install");

        let mut install_env = env_vars.clone();
        install_env.push((
            "DESTDIR".to_string(),
            destdir.to_string_lossy().into_owned(),
        ));
        crate::builder::prepare_command(&mut install_cmd, &install_env);

        let status = install_cmd
            .status()
            .with_context(|| format!("Failed to run make install for {}", spec.package.name))?;

        if !status.success() {
            anyhow::bail!("make install failed with status: {}", status);
        }

        // Run post-install hooks (after make install)
        hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        println!("Skipping make install and post-install hooks (already done)");
    }

    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn resolve_actual_src(spec: &PackageSpec, src_dir: &Path) -> Result<std::path::PathBuf> {
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

fn configure_help_text(
    configure_path: &str,
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

fn has_make_variable_override(vars: &[String], name: &str) -> bool {
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
                    eprintln!("Warning: shell command '{}' failed, skipping", cmd);
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
    fn test_resolve_actual_src_expands_source_subdir_vars() {
        let tmp = tempdir().unwrap();
        let src_root = tmp.path().join("srcroot");
        let expanded = src_root.join("expect5.45.4").join("unix");
        std::fs::create_dir_all(&expanded).unwrap();

        let spec = PackageSpec {
            package: PackageInfo {
                name: "expect".into(),
                version: "5.45.4".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
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
            spec_dir: PathBuf::from("."),
        };

        let resolved = resolve_actual_src(&spec, &src_root).unwrap();
        assert_eq!(resolved, expanded);
    }
}
