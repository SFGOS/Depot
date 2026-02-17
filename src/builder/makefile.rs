use crate::builder::state::{BuildStep, StateTracker};
use crate::cross::CrossConfig;
use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
) -> Result<()> {
    let mut state = StateTracker::new(src_dir)?;
    let flags = &spec.build.flags;

    // Use cross-compilation tools if configured, otherwise use flags from spec
    // Note: Makefile builds might rely on CC/CXX/AR being set, or might use make flags.
    // We set them in env vars.
    let (cc, ar) = if let Some(cc_cfg) = cross {
        (cc_cfg.cc.as_str(), cc_cfg.ar.as_str())
    } else {
        (flags.cc.as_str(), flags.ar.as_str())
    };

    let mut env_vars: Vec<(&str, String)> = vec![("CC", cc.to_string()), ("AR", ar.to_string())];

    let cflags;
    if !flags.cflags.is_empty() {
        cflags = flags.cflags.join(" ");
        env_vars.push(("CFLAGS", cflags));
    }

    let ldflags;
    if !flags.ldflags.is_empty() {
        ldflags = flags.ldflags.join(" ");
        env_vars.push(("LDFLAGS", ldflags));
    }

    // CARCH support
    if !flags.carch.is_empty() {
        env_vars.push(("CARCH", flags.carch.clone()));
    }

    // Export PKG_CONFIG_SYSROOT_DIR for pkg-config and a general DEPOT_ROOTFS
    if !flags.rootfs.is_empty() && flags.rootfs != "/" {
        env_vars.push(("PKG_CONFIG_SYSROOT_DIR", flags.rootfs.clone()));
    }
    env_vars.push(("DEPOT_ROOTFS", flags.rootfs.clone()));

    if !state.is_done(BuildStep::PostCompileDone) {
        println!("Running makefile build commands...");

        for cmd_str in &spec.build.flags.makefile_commands {
            let cmd_str = spec.expand_vars(cmd_str);
            println!("  Executing: {}", cmd_str);

            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&cmd_str);
            cmd.current_dir(src_dir);
            crate::builder::prepare_command(&mut cmd, &env_vars);

            let status = cmd
                .status()
                .with_context(|| format!("Failed to run build command: {}", cmd_str))?;
            if !status.success() {
                anyhow::bail!("Build command failed: {}", cmd_str);
            }
        }

        crate::source::hooks::run_post_compile_commands(spec, src_dir, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        println!("Skipping makefile build commands (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run install commands with fakeroot
        println!(
            "Running makefile install commands{}...",
            if crate::fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        for cmd_str in &spec.build.flags.makefile_install_commands {
            let cmd_str = spec.expand_vars(cmd_str);
            println!("  Executing: {}", cmd_str);

            // We need to run each command under fakeroot
            let mut cmd = crate::fakeroot::wrap_install_command("sh", destdir);
            cmd.arg("-c").arg(&cmd_str);
            cmd.current_dir(src_dir);

            let mut install_env = env_vars.clone();
            install_env.push(("DESTDIR", destdir.to_string_lossy().into_owned()));
            crate::builder::prepare_command(&mut cmd, &install_env);

            let status = cmd
                .status()
                .with_context(|| format!("Failed to run install command: {}", cmd_str))?;
            if !status.success() {
                anyhow::bail!("Install command failed: {}", cmd_str);
            }
        }

        crate::source::hooks::run_post_install_commands(spec, src_dir, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        println!("Skipping makefile install commands (already done)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo};
    use std::fs;
    use tempfile::tempdir;

    fn mk_spec(name: &str, version: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: name.into(),
                version: version.into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: "MIT".into(),
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
            }],
            build: Build {
                build_type: BuildType::Makefile,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            spec_dir: std::path::PathBuf::from("."),
        }
    }

    #[test]
    fn test_makefile_build_runs_commands() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;
        let src_path = tmp_src.path();
        let dest_path = tmp_dest.path();

        let mut spec = mk_spec("test-make", "1.0");
        spec.build.flags.makefile_commands = vec![
            "echo 'building' > built.txt".into(),
            "echo 'more build' >> built.txt".into(),
        ];
        spec.build.flags.makefile_install_commands = vec![
            "mkdir -p $DESTDIR/usr/bin".into(),
            "cp built.txt $DESTDIR/usr/bin/installed.txt".into(),
        ];

        build(&spec, src_path, dest_path, None)?;

        // Verify build step
        let built_file = src_path.join("built.txt");
        assert!(built_file.exists());
        let content = fs::read_to_string(&built_file)?;
        assert!(content.contains("building"));
        assert!(content.contains("more build"));

        // Verify install step
        let installed_file = dest_path.join("usr/bin/installed.txt");
        assert!(installed_file.exists());
        let installed_content = fs::read_to_string(&installed_file)?;
        assert_eq!(content, installed_content);

        // Verify state persistence
        let state_tracker = StateTracker::new(src_path)?;
        assert!(state_tracker.is_done(BuildStep::PostCompileDone));
        assert!(state_tracker.is_done(BuildStep::PostInstallDone));

        Ok(())
    }
}
