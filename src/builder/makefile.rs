use crate::builder::state::{BuildStep, StateTracker};
use crate::cross::CrossConfig;
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
    _host_build_dir: Option<&Path>,
) -> Result<()> {
    let mut state = StateTracker::new_with_namespace(
        src_dir,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;
    let flags = &spec.build.flags;
    fs::create_dir_all(destdir)
        .with_context(|| format!("Failed to create DESTDIR: {}", destdir.display()))?;

    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);

    // Export PKG_CONFIG_SYSROOT_DIR for pkg-config
    if !flags.rootfs.is_empty() && flags.rootfs != "/" {
        crate::builder::set_env_var(
            &mut env_vars,
            "PKG_CONFIG_SYSROOT_DIR",
            flags.rootfs.clone(),
        );
    }

    if !state.is_done(BuildStep::Configured) {
        crate::source::hooks::run_post_configure_commands(spec, src_dir, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    } else {
        crate::log_info!("Skipping post-configure hooks (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        crate::log_info!("Running makefile build commands...");

        for cmd_str in &spec.build.flags.makefile_commands {
            let cmd_str = spec.expand_vars(cmd_str);
            crate::log_info!("  Executing: {}", cmd_str);

            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&cmd_str);
            cmd.current_dir(src_dir);
            crate::builder::prepare_tool_command(&mut cmd, &env_vars);

            let status = crate::interrupts::command_status(&mut cmd)
                .with_context(|| format!("Failed to run build command: {}", cmd_str))?;
            if !status.success() {
                anyhow::bail!("Build command failed: {}", cmd_str);
            }
        }

        crate::source::hooks::run_post_compile_commands(spec, src_dir, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        crate::log_info!("Skipping makefile build commands (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run install commands with internal fakeroot
        crate::log_info!(
            "Running makefile install commands{}...",
            if crate::fakeroot::is_root() {
                ""
            } else {
                " (with internal fakeroot)"
            }
        );

        let install_destdir =
            crate::builder::install_destdir_path(src_dir, destdir, flags.lib32_variant);
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

        for cmd_str in &spec.build.flags.makefile_install_commands {
            let cmd_str = spec.expand_vars(cmd_str);
            crate::log_info!("  Executing: {}", cmd_str);

            // We need to run each command under internal fakeroot
            let mut cmd = crate::fakeroot::wrap_install_command("sh", &install_destdir);
            cmd.arg("-c").arg(&cmd_str);
            cmd.current_dir(src_dir);

            let mut install_env = env_vars.clone();
            install_env.push((
                "DESTDIR".to_string(),
                install_destdir.to_string_lossy().into_owned(),
            ));
            crate::builder::prepare_tool_command(&mut cmd, &install_env);

            let status = crate::interrupts::command_status(&mut cmd)
                .with_context(|| format!("Failed to run install command: {}", cmd_str))?;
            if !status.success() {
                anyhow::bail!("Install command failed: {}", cmd_str);
            }
        }

        if flags.lib32_variant {
            crate::builder::stage_lib32_install_tree(&install_destdir, destdir)?;
            crate::source::hooks::run_post_install_commands_in_dir(spec, src_dir, destdir)?;
        } else {
            crate::source::hooks::run_post_install_commands(spec, src_dir, destdir)?;
        }
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping makefile install commands (already done)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo};
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use tempfile::tempdir;

    fn mk_spec(name: &str, version: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: name.into(),
                real_name: None,
                version: version.into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
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
                build_type: BuildType::Makefile,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
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

        build(&spec, src_path, dest_path, None, true, None)?;

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

    #[test]
    fn test_makefile_install_preserves_staged_hardlinks() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;
        let src_path = tmp_src.path();
        let dest_path = tmp_dest.path();

        let mut spec = mk_spec("uutils-like", "1.0");
        spec.build.flags.makefile_install_commands = vec![
            "mkdir -p \"$DESTDIR/usr/bin\"".into(),
            "printf 'multicall' > \"$DESTDIR/usr/bin/uutils\"".into(),
            "ln \"$DESTDIR/usr/bin/uutils\" \"$DESTDIR/usr/bin/ls\"".into(),
        ];

        build(&spec, src_path, dest_path, None, true, None)?;

        let uutils = dest_path.join("usr/bin/uutils").metadata()?;
        let ls = dest_path.join("usr/bin/ls").metadata()?;
        assert_eq!(uutils.ino(), ls.ino());
        assert_eq!(uutils.nlink(), 2);
        assert_eq!(ls.nlink(), 2);

        Ok(())
    }

    #[test]
    fn test_makefile_lib32_install_relocates_usr_lib_without_copying_other_paths() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;
        let src_path = tmp_src.path();
        let dest_path = tmp_dest.path();

        let mut spec = mk_spec("lib32-test-make", "1.0");
        spec.build.flags.lib32_variant = true;
        spec.build.flags.makefile_install_commands = vec![
            "mkdir -p \"$DESTDIR/usr/lib\" \"$DESTDIR/usr/bin\"".into(),
            "printf 'lib32' > \"$DESTDIR/usr/lib/libfoo.so.1\"".into(),
            "printf 'bin' > \"$DESTDIR/usr/bin/foo\"".into(),
        ];

        build(&spec, src_path, dest_path, None, true, None)?;

        assert_eq!(
            fs::read_to_string(dest_path.join("usr/lib32/libfoo.so.1"))?,
            "lib32"
        );
        assert!(!dest_path.join("usr/bin/foo").exists());
        assert!(!dest_path.join("usr/lib").exists());

        Ok(())
    }
}
