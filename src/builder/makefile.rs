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
    export_compiler_flags: bool,
) -> Result<()> {
    let mut state = StateTracker::new_with_namespace(
        src_dir,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;
    let flags = &spec.build.flags;

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
        crate::log_info!("Skipping makefile build commands (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run install commands with fakeroot
        crate::log_info!(
            "Running makefile install commands{}...",
            if crate::fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        for cmd_str in &spec.build.flags.makefile_install_commands {
            let cmd_str = spec.expand_vars(cmd_str);
            crate::log_info!("  Executing: {}", cmd_str);

            // We need to run each command under fakeroot
            let mut cmd = crate::fakeroot::wrap_install_command("sh", destdir);
            cmd.arg("-c").arg(&cmd_str);
            cmd.current_dir(src_dir);

            let mut install_env = env_vars.clone();
            install_env.push((
                "DESTDIR".to_string(),
                destdir.to_string_lossy().into_owned(),
            ));
            crate::builder::prepare_tool_command(&mut cmd, &install_env);

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
        crate::log_info!("Skipping makefile install commands (already done)");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo};
    use crate::test_support::TestEnv;
    use std::fs;
    use tempfile::tempdir;

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, contents: &str) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, contents)?;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
        Ok(())
    }

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
        let tmp_tools = tempdir()?;
        let src_path = tmp_src.path();
        let dest_path = tmp_dest.path();
        let tools_path = tmp_tools.path();

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

        let mut spec = mk_spec("test-make", "1.0");
        spec.build.flags.makefile_commands = vec![
            "echo 'building' > built.txt".into(),
            "echo 'more build' >> built.txt".into(),
        ];
        spec.build.flags.makefile_install_commands = vec![
            "mkdir -p $DESTDIR/usr/bin".into(),
            "cp built.txt $DESTDIR/usr/bin/installed.txt".into(),
        ];

        build(&spec, src_path, dest_path, None, true)?;

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
