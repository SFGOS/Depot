//! Meson build system

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
) -> Result<()> {
    let flags = &spec.build.flags;
    let build_dir = src_dir.join("builddir");

    // Create directories
    fs::create_dir_all(&build_dir)?;
    fs::create_dir_all(destdir)?;

    // Environment variables
    let mut env_vars: Vec<(&str, String)> = vec![];

    // Use cross-compilation tools if configured
    let cc = if let Some(cc_cfg) = cross {
        cc_cfg.cc.clone()
    } else {
        flags.cc.clone()
    };

    if !flags.cflags.is_empty() {
        env_vars.push(("CFLAGS", flags.cflags.join(" ")));
    }
    if !flags.chost.is_empty() {
        env_vars.push(("CHOST", flags.chost.clone()));
    }
    if !flags.cbuild.is_empty() {
        env_vars.push(("CBUILD", flags.cbuild.clone()));
    }
    if !flags.ldflags.is_empty() {
        let ldflags = if flags.libc.is_empty() {
            flags.ldflags.join(" ")
        } else {
            format!(
                "{} -Wl,--dynamic-linker={}",
                flags.ldflags.join(" "),
                flags.libc
            )
        };
        env_vars.push(("LDFLAGS", ldflags));
    }
    env_vars.push(("CC", cc));

    // Add cross-compilation env
    if let Some(cc_cfg) = cross {
        env_vars.push(("CXX", cc_cfg.cxx.clone()));
        env_vars.push(("AR", cc_cfg.ar.clone()));
    }

    // Extract prefix from configure flags
    let prefix = flags
        .configure
        .iter()
        .find(|s| s.starts_with("--prefix="))
        .map(|s| s.trim_start_matches("--prefix="))
        .unwrap_or(&flags.prefix);

    // Generate cross file if cross-compiling
    let cross_file = if let Some(cc_cfg) = cross {
        Some(cc_cfg.generate_meson_cross_file(&build_dir)?)
    } else {
        None
    };

    // Run meson setup
    println!("Running meson setup...");
    let mut meson_cmd = Command::new("meson");
    meson_cmd.current_dir(src_dir);
    meson_cmd.arg("setup");
    meson_cmd.arg(&build_dir);
    meson_cmd.arg(format!("--prefix={}", prefix));
    meson_cmd.arg("--buildtype=release");

    // Add cross file for cross-compilation
    if let Some(ref cf) = cross_file {
        meson_cmd.arg(format!("--cross-file={}", cf.display()));
    }

    crate::builder::prepare_command(&mut meson_cmd, &env_vars);

    let status = meson_cmd.status().context("Failed to run meson setup")?;
    if !status.success() {
        anyhow::bail!("meson setup failed");
    }

    // Run ninja build
    println!("Running ninja...");
    let mut ninja_cmd = Command::new("ninja");
    ninja_cmd.current_dir(&build_dir);
    ninja_cmd.arg("-j").arg(num_cpus().to_string());

    crate::builder::prepare_command(&mut ninja_cmd, &env_vars);

    let status = ninja_cmd
        .status()
        .with_context(|| format!("Failed to run ninja for {}", spec.package.name))?;
    if !status.success() {
        anyhow::bail!("ninja build failed");
    }

    // Run meson install with fakeroot if not root
    println!(
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
    install_env.push(("DESTDIR", destdir.to_string_lossy().into_owned()));
    crate::builder::prepare_command(&mut install_cmd, &install_env);

    let status = install_cmd
        .status()
        .with_context(|| format!("Failed to run meson install for {}", spec.package.name))?;
    if !status.success() {
        anyhow::bail!("meson install failed");
    }

    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
