use super::*;
use crate::config::Config;

pub(crate) fn scan_package_specs(dir: &Path) -> Result<Vec<PathBuf>> {
    let root = dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve scan root {}", dir.display()))?;
    if !root.is_dir() {
        anyhow::bail!("Scan root is not a directory: {}", root.display());
    }

    let mut specs = Vec::new();
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"))
    {
        crate::interrupts::check()?;
        let entry = entry.with_context(|| format!("Failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let candidate = path.to_path_buf();
        if package::PackageSpec::from_file(&candidate).is_ok() {
            specs.push(candidate);
        }
    }
    specs.sort();
    Ok(specs)
}

fn spec_group_package_names(spec: &package::PackageSpec, group: &str) -> Vec<String> {
    let mut names = Vec::new();
    for output in spec.outputs() {
        if spec
            .dependencies_for_output(&output.name)
            .groups
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(group))
        {
            names.push(output.name);
        }
    }
    if spec.builds_lib32_output() {
        let lib32_name = spec.lib32_package_name();
        if spec
            .dependencies_for_output(&lib32_name)
            .groups
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(group))
        {
            names.push(lib32_name);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn collect_source_group_package_names(config: &Config, group: &str) -> Result<Vec<String>> {
    if !config.repo_clone_dir.exists() {
        return Ok(Vec::new());
    }

    let mut packages = BTreeSet::new();
    for spec_path in scan_package_specs(&config.repo_clone_dir)? {
        let spec = package::PackageSpec::from_file(&spec_path)
            .with_context(|| format!("Failed to parse spec {}", spec_path.display()))?;
        for package_name in spec_group_package_names(&spec, group) {
            packages.insert(package_name);
        }
    }
    Ok(packages.into_iter().collect())
}

fn collect_binary_group_package_names(
    config: &Config,
    rootfs: &Path,
    group: &str,
) -> Result<Vec<String>> {
    let host_arch = std::env::consts::ARCH;
    let mut binary_repos: Vec<_> = config
        .binary_repos
        .iter()
        .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
        .collect();
    binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

    let mut packages = BTreeSet::new();
    for (repo_name, repo_cfg) in binary_repos {
        let matches = db::repo::find_binary_repo_packages_by_group(
            repo_name,
            repo_cfg,
            rootfs,
            &config.package_cache_dir,
            group,
        )?;
        for record in matches {
            packages.insert(record.name);
        }
    }
    Ok(packages.into_iter().collect())
}

fn collect_install_group_package_names(
    config: &Config,
    rootfs: &Path,
    group: &str,
) -> Result<Vec<String>> {
    let mut packages = BTreeSet::new();
    for package_name in collect_source_group_package_names(config, group)? {
        packages.insert(package_name);
    }
    for package_name in collect_binary_group_package_names(config, rootfs, group)? {
        packages.insert(package_name);
    }
    Ok(packages.into_iter().collect())
}

pub(crate) fn expand_install_requests_for_groups(
    config: &Config,
    rootfs: &Path,
    requests: &[PathBuf],
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let mut expanded = Vec::new();
    let mut explicit_groups = Vec::new();
    let mut seen = HashSet::new();

    for request in requests {
        if request.exists() {
            let key = request.to_string_lossy().to_string();
            if seen.insert(key) {
                expanded.push(request.clone());
            }
            continue;
        }

        let request_name = request.to_string_lossy().to_string();
        let group_packages = collect_install_group_package_names(config, rootfs, &request_name)?;
        if group_packages.is_empty() {
            if seen.insert(request_name.clone()) {
                expanded.push(request.clone());
            }
            continue;
        }

        ui::info(format!(
            "Expanding group '{}' ({} packages)...",
            request_name,
            group_packages.len()
        ));
        explicit_groups.push(request_name);
        for package_name in group_packages {
            if seen.insert(package_name.clone()) {
                expanded.push(PathBuf::from(package_name));
            }
        }
    }

    Ok((expanded, explicit_groups))
}

pub(crate) fn expand_installed_group_targets(
    db_path: &Path,
    requests: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut expanded = Vec::new();
    let mut explicit_groups = Vec::new();
    let mut seen = HashSet::new();

    for request in requests {
        if db::is_installed_group(db_path, request)? {
            let group_packages = db::get_packages_in_installed_group(db_path, request)?;
            if !group_packages.is_empty() {
                ui::info(format!(
                    "Expanding group '{}' ({} packages)...",
                    request,
                    group_packages.len()
                ));
                explicit_groups.push(request.clone());
                for package_name in group_packages {
                    if seen.insert(package_name.clone()) {
                        expanded.push(package_name);
                    }
                }
                continue;
            }
        }

        if seen.insert(request.clone()) {
            expanded.push(request.clone());
        }
    }

    Ok((expanded, explicit_groups))
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn binary_arch_from_filename(filename: &str) -> String {
    let stem = filename
        .strip_suffix(".depot.pkg.tar.zst")
        .unwrap_or(filename);
    let mut parts = stem.rsplitn(4, '-');
    parts
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(std::env::consts::ARCH)
        .to_string()
}
