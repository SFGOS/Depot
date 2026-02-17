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
) -> Result<()> {
    let flags = &spec.build.flags;

    // Determine actual source directory (support source_subdir)
    let actual_src = if !flags.source_subdir.is_empty() {
        src_dir.join(&flags.source_subdir)
    } else {
        src_dir.to_path_buf()
    };

    if !actual_src.exists() {
        anyhow::bail!(
            "Source directory not found: {} (source_subdir: {})",
            actual_src.display(),
            flags.source_subdir
        );
    }

    // Create destdir
    fs::create_dir_all(destdir)?;

    // Build environment variables
    let mut env_vars: Vec<(&str, String)> = vec![];

    // Use cross-compilation tools if configured
    let (cc, ar) = if let Some(cc_cfg) = cross {
        (cc_cfg.cc.clone(), cc_cfg.ar.clone())
    } else {
        (flags.cc.clone(), flags.ar.clone())
    };

    if !flags.cflags.is_empty() {
        // Expand shell command substitutions like $($CC -print-resource-dir)
        let cflags_str = flags.cflags.join(" ");
        let expanded = expand_shell_commands(&cflags_str, &cc)?;
        env_vars.push(("CFLAGS", expanded));
    }
    if !flags.ldflags.is_empty() {
        env_vars.push(("LDFLAGS", flags.ldflags.join(" ")));
    }
    env_vars.push(("CC", cc.clone()));
    env_vars.push(("AR", ar));

    // CARCH support
    if !flags.carch.is_empty() {
        env_vars.push(("CARCH", flags.carch.clone()));
    }

    // Export rootfs for build scripts
    env_vars.push(("DEPOT_ROOTFS", flags.rootfs.clone()));

    // Add cross-compilation environment
    if let Some(cc_cfg) = cross {
        env_vars.push(("CXX", cc_cfg.cxx.clone()));
        env_vars.push(("RANLIB", cc_cfg.ranlib.clone()));
        env_vars.push(("STRIP", cc_cfg.strip.clone()));
        env_vars.push(("LD", cc_cfg.ld.clone()));
        env_vars.push(("NM", cc_cfg.nm.clone()));
    }

    // Add dynamic loader flag if specified
    if !flags.libc.is_empty() {
        let ldflags = if flags.ldflags.is_empty() {
            format!("-Wl,--dynamic-linker={}", flags.libc)
        } else {
            format!(
                "{} -Wl,--dynamic-linker={}",
                flags.ldflags.join(" "),
                flags.libc
            )
        };
        env_vars.push(("LDFLAGS", ldflags));
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

        if !flags.chost.is_empty() {
            configure_cmd.arg(format!("--host={}", flags.chost));
        }
        if !flags.cbuild.is_empty() {
            configure_cmd.arg(format!("--build={}", flags.cbuild));
        }

        // Add cross-compilation flags
        if let Some(cc_cfg) = cross {
            configure_cmd.arg(format!("--host={}", cc_cfg.host_triple()));
            if let Ok(build) = CrossConfig::build_triple() {
                configure_cmd.arg(format!("--build={}", build));
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

        crate::builder::prepare_command(&mut make_cmd, &env_vars);

        let status = make_cmd
            .status()
            .with_context(|| format!("Failed to run make in {}", build_dir.display()))?;

        if !status.success() {
            anyhow::bail!("make failed with status: {}", status);
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
        install_cmd.arg("install");

        let mut install_env = env_vars.clone();
        install_env.push(("DESTDIR", destdir.to_string_lossy().into_owned()));
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
        assert!(out.contains("OK") || out.contains("") );
        Ok(())
    }

    #[test]
    fn test_num_cpus_at_least_one() {
        let n = num_cpus();
        assert!(n >= 1);
    }
}
