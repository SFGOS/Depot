use super::*;
use crate::config::Config;

fn source_search_hit_allowed(config: &Config, hit: &index::SourceSearchHit) -> bool {
    let path = &hit.path;

    if path.starts_with(Path::new("packages")) {
        return true;
    }

    if config.source_repos.is_empty() {
        return true;
    }

    for (repo_name, repo) in &config.source_repos {
        if !repo.enabled {
            continue;
        }
        let repo_root = config.repo_clone_dir.join(repo_name);
        if repo.subdirs.is_empty() {
            if path.starts_with(&repo_root) {
                return true;
            }
        } else {
            for subdir in &repo.subdirs {
                if path.starts_with(repo_root.join(subdir)) {
                    return true;
                }
            }
        }
    }

    false
}

fn source_hit_origin(config: &Config, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(&config.repo_clone_dir)
        && let Some(first) = rel.components().next()
    {
        return format!("source:{}", first.as_os_str().to_string_lossy());
    }
    "source:local".to_string()
}

pub(super) fn run_search_command(
    query: &str,
    files: bool,
    config: &Config,
    rootfs: &Path,
) -> Result<()> {
    let mut any = false;
    let host_arch = std::env::consts::ARCH;

    let pkg_index = index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));
    let source_hits: Vec<_> = pkg_index
        .search(query)
        .into_iter()
        .filter(|hit| source_search_hit_allowed(config, hit))
        .collect();
    if !source_hits.is_empty() {
        any = true;
        ui::info("Source matches:");
        for hit in source_hits {
            let provides = if hit.provides.is_empty() {
                String::new()
            } else {
                format!(" provides={}", hit.provides.join(","))
            };
            let replaces = if hit.replaces.is_empty() {
                String::new()
            } else {
                format!(" replaces={}", hit.replaces.join(","))
            };
            ui::info(format!(
                "  {} [{}] {}{}{}",
                hit.name,
                source_hit_origin(config, &hit.path),
                hit.path.display(),
                provides,
                replaces
            ));
        }
    }

    let mut binary_repos: Vec<_> = config
        .binary_repos
        .iter()
        .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
        .collect();
    binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

    if !binary_repos.is_empty() {
        ui::info("Binary matches:");
    }
    let mut binary_hits_total = 0usize;
    for (name, repo) in binary_repos {
        match db::repo::search_binary_repo(name, repo, rootfs, &config.package_cache_dir, query) {
            Ok(hits) => {
                for hit in hits {
                    any = true;
                    binary_hits_total += 1;
                    let provides = if hit.provides.is_empty() {
                        String::new()
                    } else {
                        format!(" provides={}", hit.provides.join(","))
                    };
                    ui::info(format!(
                        "  {} [binary:{}] {}-{} size={} file={}{}{}",
                        hit.name,
                        hit.repo_name,
                        hit.version,
                        hit.revision,
                        hit.size,
                        hit.filename,
                        hit.description
                            .as_ref()
                            .map(|d| format!(" desc={}", d))
                            .unwrap_or_default(),
                        provides
                    ));
                }
            }
            Err(e) => crate::log_warn!("Binary repo '{}': {}", name, e),
        }
    }
    if !config.binary_repos.is_empty() && binary_hits_total == 0 {
        ui::info("  (no binary matches)");
    }

    if files && !config.binary_repos.is_empty() {
        ui::info("Binary file matches:");
        let mut file_hits_total = 0usize;
        let mut binary_repos: Vec<_> = config
            .binary_repos
            .iter()
            .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
            .collect();
        binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));
        for (name, repo) in binary_repos {
            match db::repo::search_binary_repo_files(
                name,
                repo,
                rootfs,
                &config.package_cache_dir,
                query,
            ) {
                Ok(hits) => {
                    for hit in hits {
                        any = true;
                        file_hits_total += 1;
                        ui::info(format!(
                            "  {} [binary:{}] {}-{} size={} owns={}",
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
        if file_hits_total == 0 {
            ui::info("  (no binary file matches)");
        }
    }

    if !any {
        ui::warn(format!("No matches found for '{}'", query));
    }
    Ok(())
}
