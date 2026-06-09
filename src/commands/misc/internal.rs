use super::*;
use crate::builder::BuildHelperContext;
use std::path::{Path, PathBuf};

fn current_process_env_vars() -> Vec<(String, String)> {
    const ALLOWED_ENV_VARS: &[&str] = &[
        "AR",
        "CARCH",
        "CBUILD",
        "CC",
        "CHOST",
        "CPP",
        "CROSS_COMPILE",
        "CROSS_PREFIX",
        "CFLAGS",
        "CXX",
        "CXXFLAGS",
        crate::builder::DEPOT_BUILD_HELPER_BUILD_DIR_ENV,
        crate::builder::DEPOT_BUILD_HELPER_CONTEXT_ENV,
        crate::builder::DEPOT_BUILD_HELPER_SOURCE_DIR_ENV,
        crate::builder::DEPOT_BUILD_HOST_DIR_ENV,
        "DEPOT_ROOTFS",
        "DEPOT_SPECDIR",
        "DESTDIR",
        "LD",
        "LDFLAGS",
        "LTOFLAGS",
        "MAKEFLAGS",
        "NM",
        "PREFIX",
        "PYTHONDONTWRITEBYTECODE",
        "PYTHONNOUSERSITE",
        "RANLIB",
        "RUSTFLAGS",
        "RUSTLTOFLAGS",
        "SETUPTOOLS_USE_DISTUTILS",
        "STRIP",
        "TOOL_DIR",
    ];

    ALLOWED_ENV_VARS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_string(), value))
        })
        .collect()
}

fn current_build_helper_context() -> Result<BuildHelperContext> {
    let raw = std::env::var(crate::builder::DEPOT_BUILD_HELPER_CONTEXT_ENV).with_context(|| {
        format!(
            "{} must be set for internal build helpers",
            crate::builder::DEPOT_BUILD_HELPER_CONTEXT_ENV
        )
    })?;
    toml::from_str(&raw).context("Failed to parse build helper context")
}

fn current_helper_source_dir() -> Option<PathBuf> {
    std::env::var(crate::builder::DEPOT_BUILD_HELPER_SOURCE_DIR_ENV)
        .ok()
        .map(PathBuf::from)
}

fn current_helper_build_dir() -> Option<PathBuf> {
    std::env::var(crate::builder::DEPOT_BUILD_HELPER_BUILD_DIR_ENV)
        .ok()
        .map(PathBuf::from)
}

fn current_cross_config() -> Result<Option<crate::cross::CrossConfig>> {
    let Some(prefix) = std::env::var("CROSS_PREFIX")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    crate::cross::CrossConfig::from_prefix(&prefix)
        .map(Some)
        .with_context(|| format!("Failed to resolve cross-compilation tools for {}", prefix))
}

pub(crate) fn run_internal_command(command: InternalCommands) -> Result<()> {
    match command {
        InternalCommands::PythonBuild {
            src_dir,
            dist_dir,
            config_settings,
        } => {
            let env_vars = current_process_env_vars();
            crate::builder::python::build_wheels(&src_dir, &dist_dir, &env_vars, &config_settings)
        }
        InternalCommands::PythonInstall {
            dist_dir,
            wheels,
            prefix,
        } => {
            let env_vars = current_process_env_vars();
            let wheel_paths = if wheels.is_empty() {
                crate::builder::python::collect_wheels(&dist_dir)?
            } else {
                wheels
            };
            let destdir = std::env::var("DESTDIR")
                .context("DESTDIR must be set for internal python-install")?;
            crate::builder::python::install_wheels(
                &wheel_paths,
                Path::new(&destdir),
                &prefix,
                &env_vars,
            )
        }
        InternalCommands::Clone { repo, dest } => {
            let (base, rev) = crate::source::split_git_url(&repo).with_context(|| {
                format!("Unsupported repository URL for internal clone: {}", repo)
            })?;
            let dest = if let Some(dest) = dest {
                dest
            } else {
                let cwd =
                    std::env::current_dir().context("Failed to determine current directory")?;
                cwd.join(crate::source::git_default_checkout_dir_name(&base))
            };
            if dest.exists() {
                anyhow::bail!("Clone destination already exists: {}", dest.display());
            }
            let cache_root =
                tempfile::tempdir().context("Failed to create temporary git cache for clone")?;
            let label = dest
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("clone");
            crate::source::git_checkout(
                &base,
                &rev,
                &dest,
                &cache_root.path().join("git"),
                label,
                &[],
            )
        }
        InternalCommands::AutotoolsConfigure { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            let cross = current_cross_config()?;
            crate::builder::run_autotools_helper_configure(
                &context,
                current_helper_source_dir().as_deref(),
                current_helper_build_dir().as_deref(),
                cross.as_ref(),
                &env_vars,
                &args,
            )
        }
        InternalCommands::AutotoolsInstall { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            crate::builder::run_autotools_helper_install(
                &context,
                current_helper_build_dir().as_deref(),
                &env_vars,
                &args,
            )
        }
        InternalCommands::CmakeConfigure { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            let cross = current_cross_config()?;
            crate::builder::run_cmake_helper_configure(
                &context,
                current_helper_source_dir().as_deref(),
                current_helper_build_dir().as_deref(),
                cross.as_ref(),
                &env_vars,
                &args,
            )
        }
        InternalCommands::CmakeInstall { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            crate::builder::run_cmake_helper_install(
                &context,
                current_helper_build_dir().as_deref(),
                &env_vars,
                &args,
            )
        }
        InternalCommands::MesonConfigure { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            let cross = current_cross_config()?;
            crate::builder::run_meson_helper_configure(
                &context,
                current_helper_source_dir().as_deref(),
                current_helper_build_dir().as_deref(),
                cross.as_ref(),
                &env_vars,
                &args,
            )
        }
        InternalCommands::MesonInstall { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            crate::builder::run_meson_helper_install(
                &context,
                current_helper_build_dir().as_deref(),
                &env_vars,
                &args,
            )
        }
        InternalCommands::PerlConfigure { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            crate::builder::run_perl_helper_configure(
                &context,
                current_helper_source_dir().as_deref(),
                &env_vars,
                &args,
            )
        }
        InternalCommands::PerlInstall { args } => {
            let env_vars = current_process_env_vars();
            let context = current_build_helper_context()?;
            crate::builder::run_perl_helper_install(
                &context,
                current_helper_build_dir().as_deref(),
                &env_vars,
                &args,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::BuildFlags;
    use crate::test_support::TestEnv;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn write_executable(path: &Path, contents: &str) -> Result<()> {
        fs::write(path, contents).with_context(|| format!("Failed to write {}", path.display()))?;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)?;
        Ok(())
    }

    fn helper_context(flags: BuildFlags, spec_dir: &Path) -> BuildHelperContext {
        BuildHelperContext {
            package_name: "demo".into(),
            package_version: "1.0".into(),
            spec_dir: spec_dir.to_path_buf(),
            flags,
            lib32_variant: false,
            host_build_dir: None,
        }
    }

    #[test]
    fn meson_configure_uses_build_helper_context_defaults() -> Result<()> {
        let source = tempdir()?;
        let tools = tempdir()?;
        let log = tools.path().join("meson.log");
        let flags = BuildFlags {
            prefix: "/opt/demo".into(),
            build_dir: Some("builddir".into()),
            ..BuildFlags::default()
        };

        write_executable(
            &tools.path().join("meson"),
            &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
        )?;

        let mut env = TestEnv::new();
        env.set_var("PATH", tools.path());
        env.set_var(
            crate::builder::DEPOT_BUILD_HELPER_CONTEXT_ENV,
            toml::to_string(&helper_context(flags, source.path()))?,
        );
        env.set_var(
            crate::builder::DEPOT_BUILD_HELPER_SOURCE_DIR_ENV,
            source.path(),
        );

        run_internal_command(InternalCommands::MesonConfigure {
            args: vec!["-Dfeature=enabled".into()],
        })?;

        let output = fs::read_to_string(&log)?;
        assert!(output.contains("setup"));
        assert!(output.contains("--prefix=/opt/demo"));
        assert!(output.contains("--buildtype=release"));
        assert!(output.contains("-Dfeature=enabled"));
        assert!(output.contains(source.path().join("builddir").to_string_lossy().as_ref()));
        Ok(())
    }

    #[test]
    fn cmake_install_uses_internal_fakeroot_and_destdir() -> Result<()> {
        let source = tempdir()?;
        let build_dir = source.path().join("build");
        let tools = tempdir()?;
        let cmake_log = tools.path().join("cmake.log");
        let destdir = source.path().join("dest");
        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&destdir)?;

        write_executable(
            &tools.path().join("cmake"),
            &format!(
                "#!/bin/sh\n{{ printf 'UID=%s\\n' \"$(id -u)\"; printf 'DESTDIR=%s\\n' \"$DESTDIR\"; printf '%s\\n' \"$@\"; }} > '{}'\n",
                cmake_log.display()
            ),
        )?;

        let mut env = TestEnv::new();
        env.set_var("PATH", tools.path());
        env.set_var("DESTDIR", &destdir);
        env.set_var(
            crate::builder::DEPOT_BUILD_HELPER_CONTEXT_ENV,
            toml::to_string(&helper_context(BuildFlags::default(), source.path()))?,
        );
        env.set_var(
            crate::builder::DEPOT_BUILD_HELPER_SOURCE_DIR_ENV,
            source.path(),
        );
        env.set_var(crate::builder::DEPOT_BUILD_HELPER_BUILD_DIR_ENV, &build_dir);

        run_internal_command(InternalCommands::CmakeInstall {
            args: vec!["--component".into(), "runtime".into()],
        })?;

        let cmake_output = fs::read_to_string(&cmake_log)?;
        assert!(cmake_output.contains("UID=0"));
        assert!(cmake_output.contains(&format!("DESTDIR={}", destdir.display())));
        assert!(cmake_output.contains("--install"));
        assert!(cmake_output.contains(build_dir.to_string_lossy().as_ref()));
        assert!(cmake_output.contains("--component"));
        assert!(cmake_output.contains("runtime"));
        Ok(())
    }
}
