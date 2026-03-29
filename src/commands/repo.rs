use super::*;

pub(crate) mod config;
pub(crate) mod groups;
mod search;

use self::config::{
    print_repo_list, repo_kind_label, resolve_repo_kind_for_name, selected_source_repos,
};
use self::search::run_search_command;

pub(super) fn run_search(args: SearchArgs) -> Result<()> {
    let SearchArgs {
        rootfs_args,
        query,
        files,
    } = args;
    let rootfs = rootfs_args.rootfs;
    let config = crate::config::Config::for_rootfs(&rootfs);
    let search_lock = locking::open_lock(&config)?;
    let search_lock_path = locking::lock_path(&config);
    let _search_lock_guard = locking::try_read(&search_lock, &search_lock_path, "search")?;
    run_search_command(&query, files, &config, &rootfs)
}

pub(super) fn run_repo(command: RepoCommands) -> Result<()> {
    match command {
        RepoCommands::Create { args, dir } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard =
                locking::try_write(&mut repo_lock, &repo_lock_path, "repo create")?;
            let repo = db::repo::RepoManager::new(dir);
            let db_path = repo.create_repo_db()?;
            if let Some(sig_path) = signing::auto_sign_zst_file_detached(&rootfs, &db_path)? {
                ui::success(format!(
                    "Created detached signature: {}",
                    sig_path.display()
                ));
            }
            ui::success(format!(
                "Created repository database: {}",
                db_path.display()
            ));
        }
        RepoCommands::Sync { args } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard =
                locking::try_write(&mut repo_lock, &repo_lock_path, "repo sync")?;
            if !crate::fakeroot::is_root() {
                anyhow::bail!("The 'repo sync' command must be run as root");
            }
            let config = cfg;
            let mirrors = config.enabled_source_mirror_map();
            if mirrors.is_empty() {
                ui::info("No enabled source repos configured");
            } else {
                db::repo::sync_mirrors(&config.repo_clone_dir, &mirrors)?;
                ui::success(format!(
                    "Source repos synchronized into {}",
                    config.repo_clone_dir.display()
                ));
            }
        }
        RepoCommands::Update { args, name } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard =
                locking::try_write(&mut repo_lock, &repo_lock_path, "repo update")?;
            if !crate::fakeroot::is_root() {
                anyhow::bail!("The 'repo update' command must be run as root");
            }
            let config = cfg;
            let mirrors = selected_source_repos(&config, name.as_deref())?;
            if mirrors.is_empty() {
                ui::info("No enabled source repos configured");
            } else {
                db::repo::sync_mirrors(&config.repo_clone_dir, &mirrors)?;
                if let Some(name) = name {
                    ui::success(format!(
                        "Source repo '{}' synchronized into {}",
                        name,
                        config.repo_clone_dir.display()
                    ));
                } else {
                    ui::success(format!(
                        "Source repos synchronized into {}",
                        config.repo_clone_dir.display()
                    ));
                }
            }
        }
        RepoCommands::Index { args, dir, subdirs } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard =
                locking::try_write(&mut repo_lock, &repo_lock_path, "repo index")?;
            let stats = index::create_source_repo_index(&dir, &subdirs)
                .with_context(|| format!("Failed to create source index for {}", dir.display()))?;
            ui::success(format!(
                "Wrote source index: {}",
                stats.index_path.display()
            ));
            ui::info(format!(
                "Indexed {} spec(s) from {} TOML file(s): package rows={} provides rows={} conflicts rows={} dependency rows={} ignored_toml={}",
                stats.specs_indexed,
                stats.toml_files_scanned,
                stats.package_rows,
                stats.provides_rows,
                stats.conflicts_rows,
                stats.dependency_rows,
                stats.ignored_toml_files
            ));
        }
        RepoCommands::List { args } => {
            let rootfs = args.rootfs_args.rootfs;
            let config = crate::config::Config::for_rootfs(&rootfs);
            let repo_lock = locking::open_lock(&config)?;
            let repo_lock_path = locking::lock_path(&config);
            let _repo_lock_guard = locking::try_read(&repo_lock, &repo_lock_path, "repo list")?;
            print_repo_list(&config);
        }
        RepoCommands::Add {
            args,
            name,
            url,
            kind,
            subdirs,
            priority,
            disabled,
            arch,
            repo_db,
            allow_unsigned,
        } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard = locking::try_write(&mut repo_lock, &repo_lock_path, "repo add")?;
            let mut repos = crate::config::load_repos_config_file(&rootfs)?;
            match kind {
                RepoKindArg::Source => {
                    if let Some(existing) = repos.source.get(&name)
                        && (existing.url != url
                            || existing.subdirs != subdirs
                            || existing.priority != priority
                            || existing.enabled == disabled)
                        && !ui::prompt_yes_no(
                            &format!("Source repo '{}' already exists. Overwrite it?", name),
                            false,
                        )?
                    {
                        anyhow::bail!("Aborted");
                    }
                    repos.source.insert(
                        name.clone(),
                        crate::config::SourceRepo {
                            url,
                            enabled: !disabled,
                            priority,
                            subdirs,
                        },
                    );
                }
                RepoKindArg::Binary => {
                    let arch_name = arch.unwrap_or_else(|| std::env::consts::ARCH.to_string());
                    let mut candidate = repos.binary.get(&name).cloned().unwrap_or_default();
                    candidate.url = url.clone();
                    candidate.enabled = !disabled;
                    candidate.priority = priority;
                    candidate.repo_db = repo_db.clone();
                    candidate.allow_unsigned = allow_unsigned;
                    candidate.arch.entry(arch_name.clone()).or_default().enabled = true;

                    if let Some(existing) = repos.binary.get(&name)
                        && (*existing != candidate)
                        && !ui::prompt_yes_no(
                            &format!("Binary repo '{}' already exists. Overwrite it?", name),
                            false,
                        )?
                    {
                        anyhow::bail!("Aborted");
                    }
                    repos.binary.insert(name.clone(), candidate);
                }
            }
            let path = crate::config::save_repos_config_file(&rootfs, &repos)?;
            ui::success(format!(
                "Saved {} repo '{}' to {}",
                repo_kind_label(kind),
                name,
                path.display()
            ));
        }
        RepoCommands::Remove { args, name, kind } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard =
                locking::try_write(&mut repo_lock, &repo_lock_path, "repo remove")?;
            let mut repos = crate::config::load_repos_config_file(&rootfs)?;
            let kind = resolve_repo_kind_for_name(&repos, &name, kind)?;
            if !ui::prompt_yes_no(
                &format!("Remove {} repo '{}'?", repo_kind_label(kind), name),
                false,
            )? {
                anyhow::bail!("Aborted");
            }

            match kind {
                RepoKindArg::Source => {
                    repos.source.remove(&name);
                }
                RepoKindArg::Binary => {
                    repos.binary.remove(&name);
                }
            }
            let path = crate::config::save_repos_config_file(&rootfs, &repos)?;
            ui::success(format!(
                "Removed {} repo '{}' from {}",
                repo_kind_label(kind),
                name,
                path.display()
            ));
        }
        RepoCommands::Enable { args, name, kind } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard =
                locking::try_write(&mut repo_lock, &repo_lock_path, "repo enable")?;
            let mut repos = crate::config::load_repos_config_file(&rootfs)?;
            let kind = resolve_repo_kind_for_name(&repos, &name, kind)?;
            match kind {
                RepoKindArg::Source => {
                    let repo = repos
                        .source
                        .get_mut(&name)
                        .with_context(|| format!("Source repo '{}' not found", name))?;
                    if repo.enabled {
                        ui::info(format!("Source repo '{}' is already enabled", name));
                    } else {
                        repo.enabled = true;
                        let path = crate::config::save_repos_config_file(&rootfs, &repos)?;
                        ui::success(format!(
                            "Enabled source repo '{}' in {}",
                            name,
                            path.display()
                        ));
                    }
                }
                RepoKindArg::Binary => {
                    let repo = repos
                        .binary
                        .get_mut(&name)
                        .with_context(|| format!("Binary repo '{}' not found", name))?;
                    if repo.enabled {
                        ui::info(format!("Binary repo '{}' is already enabled", name));
                    } else {
                        repo.enabled = true;
                        let path = crate::config::save_repos_config_file(&rootfs, &repos)?;
                        ui::success(format!(
                            "Enabled binary repo '{}' in {}",
                            name,
                            path.display()
                        ));
                    }
                }
            }
        }
        RepoCommands::Disable { args, name, kind } => {
            let rootfs = args.rootfs_args.rootfs;
            let cfg = crate::config::Config::for_rootfs(&rootfs);
            let mut repo_lock = locking::open_lock(&cfg)?;
            let repo_lock_path = locking::lock_path(&cfg);
            let _repo_lock_guard =
                locking::try_write(&mut repo_lock, &repo_lock_path, "repo disable")?;
            let mut repos = crate::config::load_repos_config_file(&rootfs)?;
            let kind = resolve_repo_kind_for_name(&repos, &name, kind)?;
            if !ui::prompt_yes_no(
                &format!("Disable {} repo '{}'?", repo_kind_label(kind), name),
                true,
            )? {
                anyhow::bail!("Aborted");
            }
            match kind {
                RepoKindArg::Source => {
                    let repo = repos
                        .source
                        .get_mut(&name)
                        .with_context(|| format!("Source repo '{}' not found", name))?;
                    repo.enabled = false;
                }
                RepoKindArg::Binary => {
                    let repo = repos
                        .binary
                        .get_mut(&name)
                        .with_context(|| format!("Binary repo '{}' not found", name))?;
                    repo.enabled = false;
                }
            }
            let path = crate::config::save_repos_config_file(&rootfs, &repos)?;
            ui::success(format!(
                "Disabled {} repo '{}' in {}",
                repo_kind_label(kind),
                name,
                path.display()
            ));
        }
        RepoCommands::Owns { args, path } => {
            let rootfs = args.rootfs_args.rootfs;
            let config = crate::config::Config::for_rootfs(&rootfs);
            let repo_lock = locking::open_lock(&config)?;
            let repo_lock_path = locking::lock_path(&config);
            let _repo_lock_guard = locking::try_read(&repo_lock, &repo_lock_path, "repo owns")?;
            let host_arch = std::env::consts::ARCH;
            let mut any = false;
            let mut binary_repos: Vec<_> = config
                .binary_repos
                .iter()
                .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
                .collect();
            binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

            for (name, repo) in binary_repos {
                match db::repo::binary_repo_owns_path(
                    name,
                    repo,
                    &rootfs,
                    &config.package_cache_dir,
                    &path.to_string_lossy(),
                ) {
                    Ok(hits) => {
                        for hit in hits {
                            any = true;
                            ui::info(format!(
                                "{} [binary:{}] {}-{} size={} owns={}",
                                hit.package_name,
                                hit.repo_name,
                                hit.version,
                                hit.revision,
                                hit.size,
                                hit.path
                            ));
                        }
                    }
                    Err(e) => crate::log_warn!("Binary repo '{}': {}", name, e),
                }
            }
            if !any {
                ui::warn(format!(
                    "No binary repo metadata entry owns {}",
                    path.display()
                ));
            }
        }
        RepoCommands::Status { args } => {
            let rootfs = args.rootfs_args.rootfs;
            let config = crate::config::Config::for_rootfs(&rootfs);
            let repo_lock = locking::open_lock(&config)?;
            let repo_lock_path = locking::lock_path(&config);
            let _repo_lock_guard = locking::try_read(&repo_lock, &repo_lock_path, "repo status")?;
            let mirrors = config.enabled_source_mirror_map();
            if mirrors.is_empty() {
                ui::info("No enabled source repos configured");
            } else {
                db::repo::mirrors_status(&config.repo_clone_dir, &mirrors)?;
            }
            if config.binary_repos.is_empty() {
                ui::info("No binary repos configured");
            } else {
                ui::info("Binary repo configuration:");
                let host_arch = std::env::consts::ARCH;
                for (name, repo) in &config.binary_repos {
                    let arch_keys = if repo.arch.is_empty() {
                        "(any)".to_string()
                    } else {
                        repo.arch.keys().cloned().collect::<Vec<_>>().join(",")
                    };
                    ui::info(format!(
                        "  {} [{}] url={} repo_db={} arches={} host_match={}",
                        name,
                        if repo.enabled { "enabled" } else { "disabled" },
                        repo.url,
                        repo.repo_db,
                        arch_keys,
                        if repo.supports_arch(host_arch) {
                            "yes"
                        } else {
                            "no"
                        }
                    ));
                }
            }
        }
    }

    Ok(())
}
