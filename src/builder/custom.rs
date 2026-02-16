//! Custom build scripts

use crate::cross::CrossConfig;
use crate::fakeroot;
use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
) -> Result<()> {
    let flags = &spec.build.flags;

    // Create destdir
    fs::create_dir_all(destdir)?;

    let mut env_vars: Vec<(&str, String)> = vec![];

    // For custom builds, look for a build.sh script in the source directory
    let build_script = src_dir.join("build.sh");

    if !build_script.exists() {
        anyhow::bail!(
            "Custom build type requires build.sh in source directory: {}",
            src_dir.display()
        );
    }

    println!(
        "Running custom build script{}...",
        if fakeroot::is_root() {
            ""
        } else {
            " (with fakeroot)"
        }
    );

    let mut cmd = fakeroot::wrap_install_command("bash", destdir);
    cmd.current_dir(src_dir);
    cmd.arg(&build_script);

    if !flags.cflags.is_empty() {
        env_vars.push(("CFLAGS", flags.cflags.join(" ")));
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

    env_vars.push(("DESTDIR", destdir.to_string_lossy().into_owned()));
    env_vars.push(("PREFIX", spec.build.flags.prefix.clone()));
    env_vars.push((
        "NYAPM_SPECDIR",
        spec.spec_dir.to_string_lossy().into_owned(),
    ));

    if !flags.chost.is_empty() {
        env_vars.push(("CHOST", flags.chost.clone()));
    }
    if !flags.cbuild.is_empty() {
        env_vars.push(("CBUILD", flags.cbuild.clone()));
    }

    // Use cross-compilation tools if configured
    if let Some(cc_cfg) = cross {
        env_vars.push(("CC", cc_cfg.cc.clone()));
        env_vars.push(("CXX", cc_cfg.cxx.clone()));
        env_vars.push(("AR", cc_cfg.ar.clone()));
        env_vars.push(("RANLIB", cc_cfg.ranlib.clone()));
        env_vars.push(("STRIP", cc_cfg.strip.clone()));
        env_vars.push(("LD", cc_cfg.ld.clone()));
        env_vars.push(("NM", cc_cfg.nm.clone()));
        env_vars.push(("CROSS_PREFIX", cc_cfg.prefix.clone()));
        env_vars.push(("CROSS_COMPILE", format!("{}-", cc_cfg.prefix)));
    } else {
        env_vars.push(("CC", flags.cc.clone()));
        env_vars.push(("AR", flags.ar.clone()));
    }

    crate::builder::prepare_command(&mut cmd, &env_vars);

    let status = cmd
        .status()
        .with_context(|| format!("Failed to run build script: {}", build_script.display()))?;

    if !status.success() {
        anyhow::bail!("Custom build script failed with status: {}", status);
    }

    Ok(())
}
