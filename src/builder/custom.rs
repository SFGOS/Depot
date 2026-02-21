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
    export_compiler_flags: bool,
) -> Result<()> {
    let flags = &spec.build.flags;

    // Create destdir
    fs::create_dir_all(destdir)?;

    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);

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
        println!("Using build.sh from spec dir: {}", spec_build.display());
    }

    if !build_script.exists() {
        anyhow::bail!(
            "Custom build type requires build.sh in source directory: {}",
            src_dir.display()
        );
    }

    use crate::builder::state::{BuildStep, StateTracker};
    let mut state = StateTracker::new(src_dir)?;

    if !state.is_done(BuildStep::PostInstallDone) {
        println!(
            "Running custom build script{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        let build_dir = if let Some(dir) = &flags.build_dir {
            let bdir = src_dir.join(dir);
            fs::create_dir_all(&bdir)?;
            bdir
        } else {
            src_dir.to_path_buf()
        };

        // Use POSIX `sh` (more likely to be available in minimal/chroot environments)
        let mut cmd = fakeroot::wrap_install_command("sh", destdir);
        cmd.current_dir(&build_dir);

        // Ensure build script path is absolute for when we are in a sub-build-dir
        let abs_build_script = if build_script.is_absolute() {
            build_script.clone()
        } else {
            std::env::current_dir()?.join(&build_script)
        };
        cmd.arg(&abs_build_script);

        crate::builder::set_env_var(
            &mut env_vars,
            "DESTDIR",
            destdir.to_string_lossy().into_owned(),
        );

        crate::builder::prepare_command(&mut cmd, &env_vars);

        // Run the command and include the OS error on spawn failures for clearer diagnostics
        let status = cmd.status().map_err(|e| {
            anyhow::anyhow!(
                "Failed to run build script {}: {}",
                build_script.display(),
                e
            )
        })?;

        if !status.success() {
            anyhow::bail!("Custom build script failed with status: {}", status);
        }
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        println!("Skipping custom build script (already done)");
    }

    Ok(())
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
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            spec_dir: std::path::PathBuf::from("."),
        }
    }

    #[test]
    fn test_build_errors_without_build_sh() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;

        let spec = mk_spec("custom-no-build", "1.0");

        let res = build(&spec, tmp_src.path(), tmp_dest.path(), None, true);
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
        let _ = build(&spec, tmp_src.path(), tmp_dest.path(), None, true)?;
        // If we reached here, build() succeeded and build.sh was copied into src
        assert!(tmp_src.path().join("build.sh").exists());
        Ok(())
    }
}
