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

    let build_dir = actual_src.join("build");

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

    // Run cmake configure
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

    // Add custom configure flags from spec (supports cross-compilation overrides)
    for flag in &flags.configure {
        // Expand environment variables in the flag
        let expanded = expand_env_vars(flag);
        cmake_cmd.arg(&expanded);
    }

    crate::builder::prepare_command(&mut cmake_cmd, &env_vars);

    let status = cmake_cmd.status().context("Failed to run cmake")?;
    if !status.success() {
        anyhow::bail!("cmake configure failed");
    }

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
    install_env.push(("DESTDIR", destdir.to_string_lossy().into_owned()));
    crate::builder::prepare_command(&mut install_cmd, &install_env);

    let status = install_cmd
        .status()
        .with_context(|| format!("Failed to run cmake install for {}", spec.package.name))?;
    if !status.success() {
        anyhow::bail!("cmake install failed");
    }

    Ok(())
}

/// Expand environment variables in a string (e.g., $DEPOT_SYSROOT)
fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    // Simple expansion for $VAR and ${VAR} patterns
    for (key, value) in std::env::vars() {
        result = result.replace(&format!("${}", key), &value);
        result = result.replace(&format!("${{{}}}", key), &value);
    }
    result
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
