use super::*;
use crate::config::{Config, RepoConfigFile};

pub(crate) fn repo_kind_label(kind: RepoKindArg) -> &'static str {
    match kind {
        RepoKindArg::Source => "source",
        RepoKindArg::Binary => "binary",
    }
}

pub(crate) fn resolve_repo_kind_for_name(
    repos: &RepoConfigFile,
    name: &str,
    kind: Option<RepoKindArg>,
) -> Result<RepoKindArg> {
    if let Some(kind) = kind {
        return Ok(kind);
    }

    let in_source = repos.source.contains_key(name);
    let in_binary = repos.binary.contains_key(name);
    match (in_source, in_binary) {
        (true, false) => Ok(RepoKindArg::Source),
        (false, true) => Ok(RepoKindArg::Binary),
        (true, true) => anyhow::bail!(
            "Repo '{}' exists as both source and binary; rerun with --kind source|binary",
            name
        ),
        (false, false) => anyhow::bail!("Repo '{}' not found in repos.toml", name),
    }
}

pub(crate) fn print_repo_list(config: &Config) {
    if config.source_repos.is_empty() && config.binary_repos.is_empty() {
        ui::info("No repos configured in /etc/depot.d/repos.toml");
        if !config.mirrors.is_empty() {
            ui::info("Legacy mirrors.toml entries are loaded as source repos at runtime.");
        }
        return;
    }

    ui::info(format!(
        "Repo settings: prefer_binary={}",
        config.repo_settings.prefer_binary
    ));

    if config.source_repos.is_empty() {
        ui::info("Source repos: none");
    } else {
        ui::info("Source repos:");
        for (name, repo) in &config.source_repos {
            let subdirs = if repo.subdirs.is_empty() {
                "(all)".to_string()
            } else {
                repo.subdirs.join(", ")
            };
            ui::info(format!(
                "  {} [{}] priority={} subdirs={} url={}",
                name,
                if repo.enabled { "enabled" } else { "disabled" },
                repo.priority,
                subdirs,
                repo.url
            ));
        }
    }

    if config.binary_repos.is_empty() {
        ui::info("Binary repos: none");
    } else {
        ui::info("Binary repos:");
        let host_arch = std::env::consts::ARCH;
        for (name, repo) in &config.binary_repos {
            let arch_keys = if repo.arch.is_empty() {
                "(any)".to_string()
            } else {
                repo.arch.keys().cloned().collect::<Vec<_>>().join(",")
            };
            ui::info(format!(
                "  {} [{}] priority={} arches={} host_match={} repo_db={}{} url={}",
                name,
                if repo.enabled { "enabled" } else { "disabled" },
                repo.priority,
                arch_keys,
                if repo.supports_arch(host_arch) {
                    "yes"
                } else {
                    "no"
                },
                repo.repo_db,
                if repo.allow_unsigned {
                    " allow_unsigned=true"
                } else {
                    ""
                },
                repo.url
            ));
        }
    }
}

pub(crate) fn selected_source_repos(
    config: &Config,
    name: Option<&str>,
) -> Result<std::collections::HashMap<String, String>> {
    let mut mirrors = config.enabled_source_mirror_map();
    if let Some(name) = name {
        if let Some(url) = mirrors.remove(name) {
            let mut only = std::collections::HashMap::new();
            only.insert(name.to_string(), url);
            return Ok(only);
        }
        anyhow::bail!("Enabled source repo '{}' not found", name);
    }
    Ok(mirrors)
}
