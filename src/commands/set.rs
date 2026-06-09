use super::*;
use crate::cli::ToolRoleArg;
use std::io::ErrorKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ToolAlias {
    alias: &'static str,
    target: &'static str,
}

pub(super) fn run_set(args: SetArgs) -> Result<()> {
    let SetArgs {
        rootfs_args,
        role,
        connector,
        implementation,
    } = args;

    if connector != "to" {
        anyhow::bail!("Expected `to` in set command, for example: depot set compiler to clang");
    }

    let implementation = implementation.trim().to_ascii_lowercase();
    let aliases = aliases_for_selection(role, &implementation)?;
    let rootfs = rootfs_args.rootfs;
    let config = config::Config::for_rootfs(&rootfs);
    let mut set_lock = locking::open_lock(&config)?;
    let set_lock_path = locking::lock_path(&config);
    let _set_lock_guard = locking::try_write(&mut set_lock, &set_lock_path, "set")?;
    let alias_dir = configured_alias_dir(&config);
    let host_alias_dir = dir_in_rootfs(&rootfs, &alias_dir);

    configure_tool_aliases(&host_alias_dir, &aliases).with_context(|| {
        format!(
            "Failed to set {} to {} in {}",
            role_name(role),
            implementation,
            host_alias_dir.display()
        )
    })?;

    ui::success(format!(
        "Set {} to {} in {}",
        role_name(role),
        implementation,
        host_alias_dir.display()
    ));
    Ok(())
}

fn role_name(role: ToolRoleArg) -> &'static str {
    match role {
        ToolRoleArg::Compiler => "compiler",
        ToolRoleArg::Linker => "linker",
        ToolRoleArg::Shell => "shell",
    }
}

fn configured_alias_dir(config: &config::Config) -> PathBuf {
    let configured = config
        .build_overrides
        .get("flags")
        .and_then(|flags| flags.get("bindir"))
        .or_else(|| config.build_overrides.get("bindir"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("/usr/bin");
    PathBuf::from(configured)
}

fn dir_in_rootfs(rootfs: &Path, dir: &Path) -> PathBuf {
    let rootfs = resolve_rootfs_base(rootfs);
    if dir.is_absolute() && dir.starts_with(&rootfs) {
        dir.to_path_buf()
    } else if dir.is_absolute() {
        rootfs.join(dir.strip_prefix("/").unwrap_or(dir))
    } else {
        rootfs.join(dir)
    }
}

fn resolve_rootfs_base(rootfs: &Path) -> PathBuf {
    if rootfs.exists() {
        rootfs.canonicalize().unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(rootfs))
                .unwrap_or_else(|_| rootfs.to_path_buf())
        })
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(rootfs))
            .unwrap_or_else(|_| rootfs.to_path_buf())
    }
}

fn aliases_for_selection(role: ToolRoleArg, implementation: &str) -> Result<Vec<ToolAlias>> {
    let aliases = match (role, implementation) {
        (ToolRoleArg::Compiler, "clang") => vec![
            ToolAlias {
                alias: "cc",
                target: "clang",
            },
            ToolAlias {
                alias: "c++",
                target: "clang++",
            },
        ],
        (ToolRoleArg::Compiler, "gcc") => vec![
            ToolAlias {
                alias: "cc",
                target: "gcc",
            },
            ToolAlias {
                alias: "c++",
                target: "g++",
            },
        ],
        (ToolRoleArg::Linker, "lld" | "ld.lld") => vec![ToolAlias {
            alias: "ld",
            target: "ld.lld",
        }],
        (ToolRoleArg::Linker, "mold") => vec![ToolAlias {
            alias: "ld",
            target: "mold",
        }],
        (ToolRoleArg::Shell, "bash") => vec![ToolAlias {
            alias: "sh",
            target: "bash",
        }],
        (ToolRoleArg::Shell, "dash") => vec![ToolAlias {
            alias: "sh",
            target: "dash",
        }],
        (ToolRoleArg::Shell, "zsh") => vec![ToolAlias {
            alias: "sh",
            target: "zsh",
        }],
        (ToolRoleArg::Compiler, _) => {
            anyhow::bail!(
                "Unsupported compiler selection `{implementation}`; supported: clang, gcc"
            )
        }
        (ToolRoleArg::Linker, _) => {
            anyhow::bail!("Unsupported linker selection `{implementation}`; supported: lld, mold")
        }
        (ToolRoleArg::Shell, _) => {
            anyhow::bail!(
                "Unsupported shell selection `{implementation}`; supported: bash, dash, zsh"
            )
        }
    };
    Ok(aliases)
}

#[cfg(unix)]
fn configure_tool_aliases(tool_dir: &Path, aliases: &[ToolAlias]) -> Result<()> {
    use std::os::unix::fs as unix_fs;

    for alias in aliases {
        validate_tool_name("alias", alias.alias)?;
        validate_tool_name("target", alias.target)?;
        let target_path = tool_dir.join(alias.target);
        match fs::metadata(&target_path) {
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {
                anyhow::bail!(
                    "Cannot set {} to {}; target tool is missing: {}",
                    alias.alias,
                    alias.target,
                    target_path.display()
                );
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to inspect {}", target_path.display()));
            }
        }

        let alias_path = tool_dir.join(alias.alias);
        match fs::symlink_metadata(&alias_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let current = fs::read_link(&alias_path)
                    .with_context(|| format!("Failed to read symlink {}", alias_path.display()))?;
                if current == Path::new(alias.target) {
                    continue;
                }
                fs::remove_file(&alias_path)
                    .with_context(|| format!("Failed to replace {}", alias_path.display()))?;
            }
            Ok(_) => {
                anyhow::bail!(
                    "Refusing to replace non-symlink tool alias: {}",
                    alias_path.display()
                );
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to inspect {}", alias_path.display()));
            }
        }

        unix_fs::symlink(alias.target, &alias_path)
            .with_context(|| format!("Failed to create symlink {}", alias_path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn configure_tool_aliases(_tool_dir: &Path, _aliases: &[ToolAlias]) -> Result<()> {
    anyhow::bail!("depot set requires Unix symlink support")
}

fn validate_tool_name(kind: &str, name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        anyhow::bail!("Invalid tool {kind}: {name:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{RootfsArgs, ToolRoleArg};

    fn set_args(rootfs: &Path, role: ToolRoleArg, implementation: &str) -> SetArgs {
        SetArgs {
            rootfs_args: RootfsArgs {
                rootfs: rootfs.to_path_buf(),
            },
            role,
            connector: "to".into(),
            implementation: implementation.into(),
        }
    }

    fn make_tool_dir(rootfs: &Path) -> PathBuf {
        let etc = rootfs.join("etc/depot.d");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("build.toml"),
            r#"
[flags]
    tool_dir = "/usr/lib/depot/tools/bin"
    bindir = "/usr/bin"
    "#,
        )
        .unwrap();
        let tool_dir = rootfs.join("usr/bin");
        fs::create_dir_all(&tool_dir).unwrap();
        tool_dir
    }

    #[test]
    fn configured_alias_dir_prefers_bindir_over_tool_dir() {
        let tmp = tempfile::tempdir().unwrap();
        make_tool_dir(tmp.path());
        let config = config::Config::for_rootfs(tmp.path());

        assert_eq!(configured_alias_dir(&config), PathBuf::from("/usr/bin"));
    }

    #[test]
    fn dir_in_rootfs_does_not_duplicate_host_absolute_rootfs_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().canonicalize().unwrap();
        let host_dir = rootfs.join("usr/bin");

        assert_eq!(dir_in_rootfs(&rootfs, &host_dir), host_dir);
    }

    #[test]
    fn compiler_selection_maps_aliases() {
        assert_eq!(
            aliases_for_selection(ToolRoleArg::Compiler, "clang").unwrap(),
            vec![
                ToolAlias {
                    alias: "cc",
                    target: "clang"
                },
                ToolAlias {
                    alias: "c++",
                    target: "clang++"
                }
            ]
        );
        assert_eq!(
            aliases_for_selection(ToolRoleArg::Compiler, "gcc").unwrap(),
            vec![
                ToolAlias {
                    alias: "cc",
                    target: "gcc"
                },
                ToolAlias {
                    alias: "c++",
                    target: "g++"
                }
            ]
        );
    }

    #[test]
    fn linker_selection_maps_aliases() {
        assert_eq!(
            aliases_for_selection(ToolRoleArg::Linker, "lld").unwrap(),
            vec![ToolAlias {
                alias: "ld",
                target: "ld.lld"
            }]
        );
        assert_eq!(
            aliases_for_selection(ToolRoleArg::Linker, "mold").unwrap(),
            vec![ToolAlias {
                alias: "ld",
                target: "mold"
            }]
        );
    }

    #[test]
    fn shell_selection_maps_aliases() {
        for shell in ["bash", "dash", "zsh"] {
            assert_eq!(
                aliases_for_selection(ToolRoleArg::Shell, shell).unwrap(),
                vec![ToolAlias {
                    alias: "sh",
                    target: shell
                }]
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_set_switches_compiler_aliases() {
        let tmp = tempfile::tempdir().unwrap();
        let tool_dir = make_tool_dir(tmp.path());
        fs::write(tool_dir.join("clang"), "").unwrap();
        fs::write(tool_dir.join("clang++"), "").unwrap();
        fs::write(tool_dir.join("gcc"), "").unwrap();
        fs::write(tool_dir.join("g++"), "").unwrap();

        run_set(set_args(tmp.path(), ToolRoleArg::Compiler, "clang")).unwrap();
        assert_eq!(
            fs::read_link(tool_dir.join("cc")).unwrap(),
            PathBuf::from("clang")
        );
        assert_eq!(
            fs::read_link(tool_dir.join("c++")).unwrap(),
            PathBuf::from("clang++")
        );

        run_set(set_args(tmp.path(), ToolRoleArg::Compiler, "gcc")).unwrap();
        assert_eq!(
            fs::read_link(tool_dir.join("cc")).unwrap(),
            PathBuf::from("gcc")
        );
        assert_eq!(
            fs::read_link(tool_dir.join("c++")).unwrap(),
            PathBuf::from("g++")
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_set_switches_linker_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let tool_dir = make_tool_dir(tmp.path());
        fs::write(tool_dir.join("ld.lld"), "").unwrap();
        fs::write(tool_dir.join("mold"), "").unwrap();

        run_set(set_args(tmp.path(), ToolRoleArg::Linker, "lld")).unwrap();
        assert_eq!(
            fs::read_link(tool_dir.join("ld")).unwrap(),
            PathBuf::from("ld.lld")
        );

        run_set(set_args(tmp.path(), ToolRoleArg::Linker, "mold")).unwrap();
        assert_eq!(
            fs::read_link(tool_dir.join("ld")).unwrap(),
            PathBuf::from("mold")
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_set_switches_shell_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let tool_dir = make_tool_dir(tmp.path());
        fs::write(tool_dir.join("dash"), "").unwrap();
        fs::write(tool_dir.join("zsh"), "").unwrap();
        fs::write(tool_dir.join("bash"), "").unwrap();

        run_set(set_args(tmp.path(), ToolRoleArg::Shell, "dash")).unwrap();
        assert_eq!(
            fs::read_link(tool_dir.join("sh")).unwrap(),
            PathBuf::from("dash")
        );

        run_set(set_args(tmp.path(), ToolRoleArg::Shell, "zsh")).unwrap();
        assert_eq!(
            fs::read_link(tool_dir.join("sh")).unwrap(),
            PathBuf::from("zsh")
        );

        run_set(set_args(tmp.path(), ToolRoleArg::Shell, "bash")).unwrap();
        assert_eq!(
            fs::read_link(tool_dir.join("sh")).unwrap(),
            PathBuf::from("bash")
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_set_refuses_to_replace_real_tool_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let tool_dir = make_tool_dir(tmp.path());
        fs::write(tool_dir.join("clang"), "").unwrap();
        fs::write(tool_dir.join("clang++"), "").unwrap();
        fs::write(tool_dir.join("cc"), "real binary").unwrap();

        let err = run_set(set_args(tmp.path(), ToolRoleArg::Compiler, "clang")).unwrap_err();
        assert!(err.to_string().contains("Failed to set compiler to clang"));
        assert!(err.chain().any(|cause| {
            cause
                .to_string()
                .contains("Refusing to replace non-symlink")
        }));
    }
}
