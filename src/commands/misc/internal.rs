use super::*;

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
    }
}
