//! Custom build scripts

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
    _host_build_dir: Option<&Path>,
) -> Result<()> {
    let flags = &spec.build.flags;
    let build_dir = if let Some(dir) = &flags.build_dir {
        let bdir = src_dir.join(dir);
        fs::create_dir_all(&bdir)?;
        bdir
    } else {
        src_dir.to_path_buf()
    };
    let install_destdir =
        crate::builder::install_destdir_path(&build_dir, destdir, flags.lib32_variant);

    // Create destdir
    fs::create_dir_all(destdir)?;
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

    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);
    let shell_helpers = crate::shell_helpers::ShellHelpers::new(&install_destdir)?;
    shell_helpers.apply_to_env_vars(&mut env_vars);

    // For custom builds, look for a build.sh script in the source directory
    let build_script = src_dir.join("build.sh");

    // If the extracted source doesn't include build.sh but the spec directory does,
    // copy it into the source dir (this makes `depot install <local-spec>` behave
    // like the spec's build.sh being part of the package when appropriate).
    let spec_build = spec.spec_dir.join("build.sh");
    if !build_script.exists() && spec_build.exists() {
        fs::create_dir_all(src_dir)?;
        fs::copy(&spec_build, &build_script).with_context(|| {
            format!(
                "Failed to copy build.sh from spec dir: {}",
                spec_build.display()
            )
        })?;
        // Ensure executable bit
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&build_script)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&build_script, perms)?;
        }
        crate::log_info!("Using build.sh from spec dir: {}", spec_build.display());
    }

    if !build_script.exists() {
        anyhow::bail!(
            "Custom build type requires build.sh in source directory: {}",
            src_dir.display()
        );
    }

    use crate::builder::state::{BuildStep, StateTracker};
    let mut state = StateTracker::new_with_namespace(
        src_dir,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;

    if !state.is_done(BuildStep::Configured) {
        hooks::run_post_configure_commands(spec, src_dir, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    } else {
        crate::log_info!("Skipping post-configure hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        crate::log_info!(
            "Running custom build script{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        crate::builder::set_env_var(
            &mut env_vars,
            "DESTDIR",
            install_destdir.to_string_lossy().into_owned(),
        );
        crate::builder::set_env_var(
            &mut env_vars,
            "DEPOT_PRIMARY_DESTDIR",
            install_destdir.to_string_lossy().into_owned(),
        );
        add_output_destdir_envs(spec, &install_destdir, &mut env_vars);

        // Ensure build script path is absolute for when we are in a sub-build-dir
        let abs_build_script = if build_script.is_absolute() {
            build_script.clone()
        } else {
            std::env::current_dir()?.join(&build_script)
        };

        // Use POSIX `sh` (more likely to be available in minimal/chroot environments)
        let mut cmd = if custom_function_mode_enabled(&abs_build_script)? {
            crate::log_info!(
                "Using custom build.sh function mode (per-output install functions enabled)"
            );
            build_function_mode_command(spec, &install_destdir, &abs_build_script)?
        } else {
            let mut cmd = fakeroot::wrap_install_command("sh", &install_destdir);
            let wrapper = crate::shell_helpers::wrap_shell_command(". \"$1\"");
            // Run custom scripts through `sh -c` so helper commands like `haul`
            // work even when the helper scripts live on a `noexec` mount.
            cmd.arg("-eu")
                .arg("-c")
                .arg(wrapper)
                .arg("sh")
                .arg(&abs_build_script);
            cmd
        };
        cmd.current_dir(&build_dir);

        crate::builder::prepare_tool_command(&mut cmd, &env_vars);

        // Run the command and include the OS error on spawn failures for clearer diagnostics
        let status = crate::interrupts::command_status(&mut cmd).map_err(|e| {
            anyhow::anyhow!(
                "Failed to run build script {}: {}",
                build_script.display(),
                e
            )
        })?;

        if !status.success() {
            anyhow::bail!("Custom build script failed with status: {}", status);
        }
        if flags.lib32_variant {
            crate::builder::stage_lib32_install_tree(&install_destdir, destdir)?;
        }
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping custom build script (already done)");
    }

    Ok(())
}

fn add_output_destdir_envs(
    spec: &PackageSpec,
    destdir: &Path,
    env_vars: &mut crate::builder::EnvVars,
) {
    for out in spec.outputs() {
        let out_destdir = if out.name == spec.package.name {
            destdir.to_path_buf()
        } else {
            crate::staging::output_staging_dir(destdir, &out.name)
        };
        let suffix = crate::shell_helpers::shell_ident_suffix(&out.name);
        crate::builder::set_env_var(
            env_vars,
            &format!("DEPOT_SUBDESTDIR_{suffix}"),
            out_destdir.to_string_lossy().into_owned(),
        );
    }
}

fn custom_function_mode_enabled(build_script: &Path) -> Result<bool> {
    let contents = fs::read_to_string(build_script)
        .with_context(|| format!("Failed to read build script: {}", build_script.display()))?;
    Ok(contents.contains("depot_build()")
        || contents.contains("depot_install()")
        || contents.contains("depot_install_")
        || contents.contains("install_"))
}

fn build_function_mode_command(
    spec: &PackageSpec,
    destdir: &Path,
    build_script: &Path,
) -> Result<Command> {
    let mut wrapper = crate::shell_helpers::wrap_shell_command("");
    wrapper.push_str("\nset -eu\n");
    wrapper.push_str("depot_has_function() {\n");
    wrapper.push_str("    case \"$(type \"$1\" 2>/dev/null || :)\" in\n");
    wrapper.push_str("        *function*) return 0 ;;\n");
    wrapper.push_str("        *) return 1 ;;\n");
    wrapper.push_str("    esac\n");
    wrapper.push_str("}\n");
    wrapper.push_str("depot_build_ran=0\n");
    wrapper.push_str(". \"$1\"\n");
    wrapper.push_str("if depot_has_function depot_build; then depot_build; depot_build_ran=1;\n");
    wrapper.push_str("elif depot_has_function build; then build; depot_build_ran=1;\n");
    wrapper.push_str("fi\n");

    let primary = &spec.package.name;
    for out in spec.outputs() {
        let out_destdir = if out.name == *primary {
            destdir.to_path_buf()
        } else {
            crate::staging::output_staging_dir(destdir, &out.name)
        };
        let fn_suffix = shell_fn_suffix(&out.name);
        let q_name = sh_single_quote(&out.name);
        let q_dest = sh_single_quote(&out_destdir.to_string_lossy());

        wrapper.push_str(&format!(
            "DEPOT_OUTPUT_NAME='{q_name}'; DEPOT_OUTPUT_DESTDIR='{q_dest}'; DESTDIR=\"$DEPOT_OUTPUT_DESTDIR\"; export DEPOT_OUTPUT_NAME DEPOT_OUTPUT_DESTDIR DESTDIR\n"
        ));
        wrapper.push_str("depot_output_installed=0\n");
        wrapper.push_str(&format!(
            "if depot_has_function depot_install_{fn_suffix}; then depot_install_{fn_suffix}; depot_output_installed=1;\n"
        ));
        wrapper.push_str(&format!(
            "elif depot_has_function install_{fn_suffix}; then install_{fn_suffix}; depot_output_installed=1;\n"
        ));
        if out.name == *primary {
            wrapper
                .push_str("elif depot_has_function depot_install; then depot_install; depot_output_installed=1;\n");
            wrapper.push_str(
                "elif depot_has_function install; then install; depot_output_installed=1;\n",
            );
            wrapper.push_str("elif [ \"$depot_build_ran\" = 1 ]; then depot_output_installed=1;\n");
        }
        wrapper.push_str("fi\n");
        wrapper.push_str("if [ \"$depot_output_installed\" != 1 ]; then\n");
        wrapper.push_str(
            "    echo \"depot: missing install handler for output '$DEPOT_OUTPUT_NAME' in custom function mode\" >&2\n",
        );
        wrapper.push_str("    exit 1\n");
        wrapper.push_str("fi\n");
    }

    let mut cmd = fakeroot::wrap_install_command("sh", destdir);
    cmd.arg("-c").arg(wrapper).arg("sh").arg(build_script);
    Ok(cmd)
}

fn shell_fn_suffix(pkg_name: &str) -> String {
    let mut out = String::with_capacity(pkg_name.len().max(1));
    for ch in pkg_name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    if out.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn sh_single_quote(s: &str) -> String {
    s.replace('\'', "'\"'\"'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo};
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
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        }
    }

    #[test]
    fn test_build_errors_without_build_sh() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;

        let spec = mk_spec("custom-no-build", "1.0");

        let res = build(&spec, tmp_src.path(), tmp_dest.path(), None, true, None);
        assert!(res.is_err());
        Ok(())
    }

    #[test]
    fn test_build_uses_build_sh_from_spec_dir() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;
        let spec_dir = tempdir()?;

        // write a no-op build.sh into spec_dir
        let build_sh = spec_dir.path().join("build.sh");
        std::fs::write(&build_sh, "#!/bin/sh\nexit 0\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&build_sh)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&build_sh, perms)?;
        }

        let mut spec = mk_spec("custom-from-spec", "1.0");
        spec.spec_dir = spec_dir.path().to_path_buf();

        // src_dir is empty; build() should copy build.sh from spec_dir and run it (no-op)
        build(&spec, tmp_src.path(), tmp_dest.path(), None, true, None)?;
        // If we reached here, build() succeeded and build.sh was copied into src
        assert!(tmp_src.path().join("build.sh").exists());
        Ok(())
    }

    #[test]
    fn test_build_function_mode_uses_per_output_destdirs() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;

        let build_sh = tmp_src.path().join("build.sh");
        std::fs::write(
            &build_sh,
            r#"#!/bin/sh
depot_build() {
  :
}
depot_install() {
  mkdir -p "$DESTDIR/usr/share"
  echo primary > "$DESTDIR/usr/share/primary.txt"
}
depot_install_dev_pkg() {
  mkdir -p "$DESTDIR/usr/include"
  echo header > "$DESTDIR/usr/include/dev.h"
}
"#,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&build_sh)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&build_sh, perms)?;
        }

        let mut spec = mk_spec("demo", "1.0");
        spec.packages.push(crate::package::PackageInfo {
            name: "dev-pkg".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            license: vec!["MIT".into()],
        });

        build(&spec, tmp_src.path(), tmp_dest.path(), None, true, None)?;

        assert!(tmp_dest.path().join("usr/share/primary.txt").exists());
        assert!(
            tmp_dest
                .path()
                .join(".depot/outputs/dev-pkg/usr/include/dev.h")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn test_build_function_mode_errors_when_output_handler_missing() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;

        let build_sh = tmp_src.path().join("build.sh");
        std::fs::write(
            &build_sh,
            r#"#!/bin/sh
depot_build() {
  :
}
depot_install() {
  mkdir -p "$DESTDIR/usr/share"
  echo primary > "$DESTDIR/usr/share/primary.txt"
}
"#,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&build_sh)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&build_sh, perms)?;
        }

        let mut spec = mk_spec("demo", "1.0");
        spec.packages.push(crate::package::PackageInfo {
            name: "dev-pkg".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            license: vec!["MIT".into()],
        });

        let err = build(&spec, tmp_src.path(), tmp_dest.path(), None, true, None)
            .expect_err("missing per-output install handler should fail");
        assert!(err.to_string().contains("Custom build script failed"));
        Ok(())
    }

    #[test]
    fn test_build_non_function_mode_stops_on_first_command_failure() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;

        let build_sh = tmp_src.path().join("build.sh");
        std::fs::write(
            &build_sh,
            r#"#!/bin/sh
false
exit 0
"#,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&build_sh)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&build_sh, perms)?;
        }

        let spec = mk_spec("custom-fail-fast", "1.0");
        let err = build(&spec, tmp_src.path(), tmp_dest.path(), None, true, None)
            .expect_err("non-function custom scripts should fail when a command fails");
        assert!(err.to_string().contains("Custom build script failed"));
        Ok(())
    }

    #[test]
    fn test_build_lib32_stages_only_usr_lib_payload() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;
        let tmp_tools = tempdir()?;

        let build_sh = tmp_src.path().join("build.sh");
        std::fs::write(
            &build_sh,
            r#"#!/bin/sh
mkdir -p "$DESTDIR/usr/lib" "$DESTDIR/usr/share/man/man1"
printf 'lib32' > "$DESTDIR/usr/lib/libfoo.so.1"
printf 'manpage' > "$DESTDIR/usr/share/man/man1/foo.1"
"#,
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&build_sh)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&build_sh, perms)?;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let fakeroot = tmp_tools.path().join("fakeroot");
            std::fs::write(
                &fakeroot,
                r#"#!/bin/sh
if [ "$1" = "--" ]; then
    shift
fi
exec "$@"
"#,
            )?;
            let mut perms = std::fs::metadata(&fakeroot)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fakeroot, perms)?;
        }

        let mut env = crate::test_support::TestEnv::new();
        let old_path = std::env::var("PATH").unwrap_or_default();
        env.set_var(
            "PATH",
            format!("{}:{}", tmp_tools.path().display(), old_path),
        );

        let mut spec = mk_spec("custom-lib32", "1.0");
        spec.build.flags.lib32_variant = true;

        build(&spec, tmp_src.path(), tmp_dest.path(), None, true, None)?;

        assert_eq!(
            std::fs::read_to_string(tmp_dest.path().join("usr/lib32/libfoo.so.1"))?,
            "lib32"
        );
        assert!(!tmp_dest.path().join("usr/share/man/man1/foo.1").exists());
        assert!(!tmp_dest.path().join("usr/lib").exists());

        Ok(())
    }
}
