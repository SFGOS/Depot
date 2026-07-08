use super::*;
use crate::commands::repo::groups::scan_package_specs;

#[derive(Clone, Copy)]
pub(crate) struct UpdateCommandOptions<'a> {
    pub(crate) rootfs: &'a Path,
    pub(crate) no_deps: bool,
    pub(crate) no_flags: bool,
    pub(crate) cross_prefix: Option<&'a str>,
    pub(crate) clean: bool,
    pub(crate) dry_run: bool,
    pub(crate) assume_yes: bool,
    pub(crate) install_test_deps: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum UpdateOrigin {
    Source {
        repo_name: String,
        path: PathBuf,
    },
    Binary {
        repo_name: String,
        record: Box<db::repo::BinaryRepoPackageRecord>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct UpdateCandidate {
    pub(crate) installed_package: String,
    pub(crate) candidate_package: String,
    pub(crate) replaces_installed: bool,
    pub(crate) installed_version: String,
    pub(crate) installed_revision: u32,
    pub(crate) installed_completed_at: Option<i64>,
    pub(crate) candidate_version: String,
    pub(crate) candidate_revision: u32,
    pub(crate) candidate_completed_at: Option<i64>,
    pub(crate) runtime_dependencies: Vec<String>,
    pub(crate) provides: Vec<String>,
    pub(crate) conflicts: Vec<String>,
    pub(crate) repo_priority: i32,
    pub(crate) origin: UpdateOrigin,
}

#[derive(Debug, Clone)]
pub(crate) struct SourceUpdateCandidate {
    pub(crate) repo_name: String,
    pub(crate) repo_priority: i32,
    pub(crate) path: PathBuf,
    pub(crate) completed_at: Option<i64>,
    pub(crate) spec: package::PackageSpec,
}

pub(crate) fn source_update_candidate_is_better(
    candidate: &SourceUpdateCandidate,
    current: &SourceUpdateCandidate,
) -> bool {
    match compare_package_release(
        &candidate.spec.package.version,
        candidate.spec.package.revision,
        &current.spec.package.version,
        current.spec.package.revision,
    ) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match compare_completed_at(candidate.completed_at, current.completed_at)
        {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => {
                if candidate.repo_priority != current.repo_priority {
                    candidate.repo_priority < current.repo_priority
                } else if candidate.repo_name != current.repo_name {
                    candidate.repo_name < current.repo_name
                } else {
                    candidate.path < current.path
                }
            }
        },
    }
}

fn binary_update_candidate_is_better(
    repo_name: &str,
    repo_priority: i32,
    record: &db::repo::BinaryRepoPackageRecord,
    current_priority: i32,
    current: &db::repo::BinaryRepoPackageRecord,
) -> bool {
    match compare_package_release(
        &record.version,
        record.revision,
        &current.version,
        current.revision,
    ) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match compare_completed_at(record.completed_at, current.completed_at) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => {
                if repo_priority != current_priority {
                    repo_priority < current_priority
                } else if repo_name != current.repo_name {
                    repo_name < current.repo_name.as_str()
                } else {
                    record.filename < current.filename
                }
            }
        },
    }
}

fn compare_package_release(
    left_version: &str,
    left_revision: u32,
    right_version: &str,
    right_revision: u32,
) -> Ordering {
    super::compare_versions_for_updates(left_version, right_version)
        .then_with(|| left_revision.cmp(&right_revision))
}

pub(crate) fn compare_completed_at(left: Option<i64>, right: Option<i64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn update_candidate_is_newer_than_installed(
    candidate_version: &str,
    candidate_revision: u32,
    candidate_completed_at: Option<i64>,
    installed_version: &str,
    installed_revision: u32,
    installed_completed_at: Option<i64>,
) -> bool {
    match compare_package_release(
        candidate_version,
        candidate_revision,
        installed_version,
        installed_revision,
    ) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => {
            compare_completed_at(candidate_completed_at, installed_completed_at)
                == Ordering::Greater
        }
    }
}

fn path_modified_unix_timestamp(path: &Path) -> Result<Option<i64>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("Failed to read modification time for {}", path.display()))?;
    Ok(Some(crate::metadata_time::system_time_to_unix(modified)?))
}

fn installed_package_completed_at(
    installed: &db::InstalledPackageRecord,
    db_path: &Path,
    rootfs: &Path,
) -> Result<Option<i64>> {
    if installed.completed_at.is_some() {
        return Ok(installed.completed_at);
    }

    let mut latest = None;
    for relative_path in db::get_package_files(db_path, &installed.name)? {
        let path = rootfs.join(&relative_path);
        if !path.exists() {
            continue;
        }
        if let Some(modified) = path_modified_unix_timestamp(&path)? {
            latest = Some(latest.map_or(modified, |current: i64| current.max(modified)));
        }
    }

    Ok(latest)
}

fn update_origin_is_binary(origin: &UpdateOrigin) -> bool {
    matches!(origin, UpdateOrigin::Binary { .. })
}

fn update_origin_label(origin: &UpdateOrigin) -> String {
    match origin {
        UpdateOrigin::Source { repo_name, path } => {
            format!("source:{repo_name}:{}", path.display())
        }
        UpdateOrigin::Binary { repo_name, record } => {
            format!("binary:{repo_name}:{}", record.filename)
        }
    }
}

pub(crate) fn update_candidate_is_preferred(
    candidate: &UpdateCandidate,
    current: &UpdateCandidate,
    prefer_binary: bool,
) -> bool {
    match compare_package_release(
        &candidate.candidate_version,
        candidate.candidate_revision,
        &current.candidate_version,
        current.candidate_revision,
    ) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match compare_completed_at(
            candidate.candidate_completed_at,
            current.candidate_completed_at,
        ) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => match (
                update_origin_is_binary(&candidate.origin),
                update_origin_is_binary(&current.origin),
            ) {
                (true, false) => prefer_binary,
                (false, true) => !prefer_binary,
                _ => {
                    if candidate.repo_priority != current.repo_priority {
                        candidate.repo_priority < current.repo_priority
                    } else {
                        update_origin_label(&candidate.origin)
                            < update_origin_label(&current.origin)
                    }
                }
            },
        },
    }
}

fn candidate_request_path(
    candidate: &UpdateCandidate,
    config: &config::Config,
    rootfs: &Path,
) -> Result<PathBuf> {
    match &candidate.origin {
        UpdateOrigin::Source { path, .. } => Ok(path.clone()),
        UpdateOrigin::Binary { repo_name, record } => {
            let repo_cfg = config
                .binary_repos
                .get(repo_name)
                .with_context(|| format!("Binary repo '{}' not found in config", repo_name))?;
            db::repo::fetch_binary_package_archive(
                repo_name,
                repo_cfg,
                rootfs,
                record,
                &config.package_cache_dir,
            )
        }
    }
}

pub(crate) fn sync_source_repositories_for_update(config: &config::Config) -> Result<()> {
    if config.repo_settings.prefer_binary {
        return Ok(());
    }

    let mirrors = config.enabled_source_mirror_map();
    if mirrors.is_empty() {
        return Ok(());
    }

    let mut repo_lock = locking::open_lock(config)?;
    let repo_lock_path = locking::lock_path(config);
    let _repo_lock_guard = locking::try_write(&mut repo_lock, &repo_lock_path, "update sync")?;
    db::repo::sync_mirrors(&config.repo_clone_dir, &mirrors)?;
    Ok(())
}

fn configured_source_scan_roots(config: &config::Config) -> Vec<(String, i32, PathBuf)> {
    let mut roots = Vec::new();
    let mut repos: Vec<_> = config
        .source_repos
        .iter()
        .filter(|(_, repo)| repo.enabled)
        .collect();
    repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

    for (repo_name, repo) in repos {
        let repo_root = config.repo_clone_dir.join(repo_name);
        if repo.subdirs.is_empty() {
            roots.push((repo_name.clone(), repo.priority, repo_root));
        } else {
            for subdir in &repo.subdirs {
                roots.push((repo_name.clone(), repo.priority, repo_root.join(subdir)));
            }
        }
    }

    roots
}

pub(crate) fn collect_best_source_update_candidates(
    config: &config::Config,
    target_real_names: &HashSet<String>,
) -> Result<HashMap<String, SourceUpdateCandidate>> {
    let mut best: HashMap<String, SourceUpdateCandidate> = HashMap::new();

    for (repo_name, repo_priority, root) in configured_source_scan_roots(config) {
        if !root.exists() {
            continue;
        }

        for spec_path in scan_package_specs(&root)? {
            let mut spec = package::PackageSpec::from_file(&spec_path)?;
            spec.apply_config(config);
            let stream_name = spec.package.effective_real_name().to_string();
            if !target_real_names.contains(&stream_name) {
                continue;
            }

            let candidate = SourceUpdateCandidate {
                repo_name: repo_name.clone(),
                repo_priority,
                path: spec_path.clone(),
                completed_at: path_modified_unix_timestamp(&spec_path)?,
                spec,
            };

            let replace = match best.get(&stream_name) {
                Some(current) => source_update_candidate_is_better(&candidate, current),
                None => true,
            };

            if replace {
                best.insert(stream_name, candidate);
            }
        }
    }

    Ok(best)
}

fn collect_best_source_replacement_candidates(
    config: &config::Config,
    target_packages: &HashSet<String>,
) -> Result<HashMap<String, SourceUpdateCandidate>> {
    let mut best: HashMap<String, SourceUpdateCandidate> = HashMap::new();

    for (repo_name, repo_priority, root) in configured_source_scan_roots(config) {
        if !root.exists() {
            continue;
        }

        for spec_path in scan_package_specs(&root)? {
            let mut spec = package::PackageSpec::from_file(&spec_path)?;
            spec.apply_config(config);
            let candidate = SourceUpdateCandidate {
                repo_name: repo_name.clone(),
                repo_priority,
                path: spec_path.clone(),
                completed_at: path_modified_unix_timestamp(&spec_path)?,
                spec,
            };

            for replaced in &candidate.spec.alternatives.replaces {
                if !target_packages.contains(replaced) {
                    continue;
                }
                let replace = match best.get(replaced) {
                    Some(current) => source_update_candidate_is_better(&candidate, current),
                    None => true,
                };
                if replace {
                    best.insert(replaced.clone(), candidate.clone());
                }
            }
        }
    }

    Ok(best)
}

fn collect_best_binary_update_candidates(
    config: &config::Config,
    rootfs: &Path,
    target_real_names: &HashSet<String>,
) -> Result<HashMap<String, (i32, db::repo::BinaryRepoPackageRecord)>> {
    let mut best: HashMap<String, (i32, db::repo::BinaryRepoPackageRecord)> = HashMap::new();
    let host_arch = std::env::consts::ARCH;
    let mut repos: Vec<_> = config
        .binary_repos
        .iter()
        .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
        .collect();
    repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

    for (repo_name, repo_cfg) in repos {
        let records = db::repo::list_binary_repo_packages(
            repo_name,
            repo_cfg,
            rootfs,
            &config.package_cache_dir,
        )
        .with_context(|| format!("Failed to list binary repo '{}'", repo_name))?;

        for record in records {
            let stream_name = record.effective_real_name().to_string();
            if !target_real_names.contains(&stream_name) {
                continue;
            }

            let replace = match best.get(&stream_name) {
                Some((current_priority, current)) => binary_update_candidate_is_better(
                    repo_name,
                    repo_cfg.priority,
                    &record,
                    *current_priority,
                    current,
                ),
                None => true,
            };

            if replace {
                best.insert(stream_name, (repo_cfg.priority, record));
            }
        }
    }

    Ok(best)
}

fn collect_best_binary_replacement_candidates(
    config: &config::Config,
    rootfs: &Path,
    target_packages: &HashSet<String>,
) -> Result<HashMap<String, (i32, db::repo::BinaryRepoPackageRecord)>> {
    let mut best: HashMap<String, (i32, db::repo::BinaryRepoPackageRecord)> = HashMap::new();
    let host_arch = std::env::consts::ARCH;
    let mut repos: Vec<_> = config
        .binary_repos
        .iter()
        .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
        .collect();
    repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

    for (repo_name, repo_cfg) in repos {
        let records = db::repo::list_binary_repo_packages(
            repo_name,
            repo_cfg,
            rootfs,
            &config.package_cache_dir,
        )
        .with_context(|| format!("Failed to list binary repo '{}'", repo_name))?;

        for record in records {
            for replaced in &record.replaces {
                if !target_packages.contains(replaced) {
                    continue;
                }
                let replace = match best.get(replaced) {
                    Some((current_priority, current)) => binary_update_candidate_is_better(
                        repo_name,
                        repo_cfg.priority,
                        &record,
                        *current_priority,
                        current,
                    ),
                    None => true,
                };
                if replace {
                    best.insert(replaced.clone(), (repo_cfg.priority, record.clone()));
                }
            }
        }
    }

    Ok(best)
}

pub(crate) fn select_update_candidate(
    installed: &db::InstalledPackageRecord,
    installed_completed_at: Option<i64>,
    source_replacement_candidates: &HashMap<String, SourceUpdateCandidate>,
    binary_replacement_candidates: &HashMap<String, (i32, db::repo::BinaryRepoPackageRecord)>,
    source_candidates: &HashMap<String, SourceUpdateCandidate>,
    binary_candidates: &HashMap<String, (i32, db::repo::BinaryRepoPackageRecord)>,
    prefer_binary: bool,
) -> Option<UpdateCandidate> {
    let mut best: Option<UpdateCandidate> = None;
    let stream_name = installed.effective_real_name();

    if let Some(candidate) = source_replacement_candidates.get(&installed.name) {
        best = Some(UpdateCandidate {
            installed_package: installed.name.clone(),
            candidate_package: candidate.spec.package.name.clone(),
            replaces_installed: true,
            installed_version: installed.version.clone(),
            installed_revision: installed.revision,
            installed_completed_at,
            candidate_version: candidate.spec.package.version.clone(),
            candidate_revision: candidate.spec.package.revision,
            candidate_completed_at: candidate.completed_at,
            runtime_dependencies: candidate.spec.dependencies.runtime.clone(),
            provides: candidate.spec.alternatives.provides.clone(),
            conflicts: candidate.spec.alternatives.conflicts.clone(),
            repo_priority: candidate.repo_priority,
            origin: UpdateOrigin::Source {
                repo_name: candidate.repo_name.clone(),
                path: candidate.path.clone(),
            },
        });
    }

    if let Some((repo_priority, record)) = binary_replacement_candidates.get(&installed.name) {
        let binary_candidate = UpdateCandidate {
            installed_package: installed.name.clone(),
            candidate_package: record.name.clone(),
            replaces_installed: true,
            installed_version: installed.version.clone(),
            installed_revision: installed.revision,
            installed_completed_at,
            candidate_version: record.version.clone(),
            candidate_revision: record.revision,
            candidate_completed_at: record.completed_at,
            runtime_dependencies: record.runtime_dependencies.clone(),
            provides: record.provides.clone(),
            conflicts: record.conflicts.clone(),
            repo_priority: *repo_priority,
            origin: UpdateOrigin::Binary {
                repo_name: record.repo_name.clone(),
                record: Box::new(record.clone()),
            },
        };

        if best.as_ref().is_none_or(|current| {
            update_candidate_is_preferred(&binary_candidate, current, prefer_binary)
        }) {
            best = Some(binary_candidate);
        }
    }

    if best.is_some() {
        return best;
    }

    if let Some(candidate) = source_candidates.get(stream_name)
        && update_candidate_is_newer_than_installed(
            &candidate.spec.package.version,
            candidate.spec.package.revision,
            candidate.completed_at,
            &installed.version,
            installed.revision,
            installed_completed_at,
        )
    {
        best = Some(UpdateCandidate {
            installed_package: installed.name.clone(),
            candidate_package: candidate.spec.package.name.clone(),
            replaces_installed: false,
            installed_version: installed.version.clone(),
            installed_revision: installed.revision,
            installed_completed_at,
            candidate_version: candidate.spec.package.version.clone(),
            candidate_revision: candidate.spec.package.revision,
            candidate_completed_at: candidate.completed_at,
            runtime_dependencies: candidate.spec.dependencies.runtime.clone(),
            provides: candidate.spec.alternatives.provides.clone(),
            conflicts: candidate.spec.alternatives.conflicts.clone(),
            repo_priority: candidate.repo_priority,
            origin: UpdateOrigin::Source {
                repo_name: candidate.repo_name.clone(),
                path: candidate.path.clone(),
            },
        });
    }

    if let Some((repo_priority, record)) = binary_candidates.get(stream_name)
        && update_candidate_is_newer_than_installed(
            &record.version,
            record.revision,
            record.completed_at,
            &installed.version,
            installed.revision,
            installed_completed_at,
        )
    {
        let binary_candidate = UpdateCandidate {
            installed_package: installed.name.clone(),
            candidate_package: record.name.clone(),
            replaces_installed: false,
            installed_version: installed.version.clone(),
            installed_revision: installed.revision,
            installed_completed_at,
            candidate_version: record.version.clone(),
            candidate_revision: record.revision,
            candidate_completed_at: record.completed_at,
            runtime_dependencies: record.runtime_dependencies.clone(),
            provides: record.provides.clone(),
            conflicts: record.conflicts.clone(),
            repo_priority: *repo_priority,
            origin: UpdateOrigin::Binary {
                repo_name: record.repo_name.clone(),
                record: Box::new(record.clone()),
            },
        };

        if best.as_ref().is_none_or(|current| {
            update_candidate_is_preferred(&binary_candidate, current, prefer_binary)
        }) {
            best = Some(binary_candidate);
        }
    }

    best
}

pub(crate) fn collect_update_candidates(
    config: &config::Config,
    rootfs: &Path,
    requested_packages: &[String],
) -> Result<Vec<UpdateCandidate>> {
    let db_path = config.installed_db_path(rootfs);
    let installed = db::list_installed_package_records(&db_path)?;
    if installed.is_empty() {
        return Ok(Vec::new());
    }

    let mut installed_by_real_name: HashMap<String, Vec<&db::InstalledPackageRecord>> =
        HashMap::new();
    for record in &installed {
        installed_by_real_name
            .entry(record.effective_real_name().to_string())
            .or_default()
            .push(record);
    }

    let active_by_real_name: HashMap<String, db::InstalledPackageRecord> = installed_by_real_name
        .iter()
        .filter_map(|(real_name, records)| {
            super::super::select_primary_installed_record(records.iter().copied())
                .cloned()
                .map(|record| (real_name.clone(), record))
        })
        .collect();

    let target_real_names: HashSet<String> = if requested_packages.is_empty() {
        active_by_real_name.keys().cloned().collect()
    } else {
        let mut targets = HashSet::new();
        for package in requested_packages {
            if let Some(record) = installed.iter().find(|record| record.name == *package) {
                targets.insert(record.effective_real_name().to_string());
            } else if active_by_real_name.contains_key(package) {
                targets.insert(package.clone());
            } else {
                ui::warn(format!("Package '{}' is not installed; skipping", package));
            }
        }
        targets
    };
    let target_package_names: HashSet<String> = if requested_packages.is_empty() {
        active_by_real_name
            .values()
            .map(|record| record.name.clone())
            .collect()
    } else {
        let mut targets = HashSet::new();
        for package in requested_packages {
            if let Some(record) = installed.iter().find(|record| record.name == *package) {
                targets.insert(record.name.clone());
            } else if let Some(record) = active_by_real_name.get(package) {
                targets.insert(record.name.clone());
            }
        }
        targets
    };

    let source_candidates = if config.repo_settings.prefer_binary {
        HashMap::new()
    } else {
        collect_best_source_update_candidates(config, &target_real_names)?
    };
    let source_replacement_candidates = if config.repo_settings.prefer_binary {
        HashMap::new()
    } else {
        collect_best_source_replacement_candidates(config, &target_package_names)?
    };
    let binary_candidates =
        collect_best_binary_update_candidates(config, rootfs, &target_real_names)?;
    let binary_replacement_candidates =
        collect_best_binary_replacement_candidates(config, rootfs, &target_package_names)?;

    let mut updates = Vec::new();
    let mut targets: Vec<_> = target_real_names.into_iter().collect();
    targets.sort();
    for target in targets {
        let Some(installed) = active_by_real_name.get(&target) else {
            continue;
        };
        let installed_completed_at = installed_package_completed_at(installed, &db_path, rootfs)?;
        if let Some(candidate) = select_update_candidate(
            installed,
            installed_completed_at,
            &source_replacement_candidates,
            &binary_replacement_candidates,
            &source_candidates,
            &binary_candidates,
            config.repo_settings.prefer_binary,
        ) {
            updates.push(candidate);
        }
    }

    Ok(updates)
}

pub(crate) fn collect_missing_update_dependencies(
    candidates: &[UpdateCandidate],
    db_path: &Path,
) -> Result<Vec<String>> {
    let mut planned_provides = HashSet::new();
    for candidate in candidates {
        planned_provides.insert(candidate.candidate_package.clone());
        for provide in &candidate.provides {
            planned_provides.insert(provide.clone());
        }
    }

    let mut missing = Vec::new();
    for candidate in candidates {
        for dep in &candidate.runtime_dependencies {
            if planned_provides.contains(deps::dep_name(dep)) {
                continue;
            }
            if deps::is_dep_satisfied_in_db(dep, db_path)? {
                continue;
            }
            if !missing.contains(dep) {
                missing.push(dep.clone());
            }
        }
    }

    Ok(missing)
}

fn confirm_update_replacements(candidates: &[UpdateCandidate]) -> Result<()> {
    for candidate in candidates {
        if !candidate.replaces_installed {
            continue;
        }
        if !ui::prompt_yes_no(
            &format!(
                "replace {} with {}?",
                candidate.installed_package, candidate.candidate_package
            ),
            true,
        )? {
            anyhow::bail!(
                "Replacement declined: {} -> {}",
                candidate.installed_package,
                candidate.candidate_package
            );
        }
    }
    Ok(())
}

pub(crate) fn run_update_command(
    packages: &[String],
    config: &config::Config,
    options: UpdateCommandOptions<'_>,
) -> Result<()> {
    let updates = collect_update_candidates(config, options.rootfs, packages)?;
    if updates.is_empty() {
        ui::info("All installed packages are up to date.");
        return Ok(());
    }

    let targets: Vec<String> = updates
        .iter()
        .map(|candidate| {
            let summary = format!(
                "{} v{}-{} -> {} v{}-{}",
                candidate.installed_package,
                candidate.installed_version,
                candidate.installed_revision,
                candidate.candidate_package,
                candidate.candidate_version,
                candidate.candidate_revision
            );
            if candidate.installed_version == candidate.candidate_version
                && candidate.installed_revision == candidate.candidate_revision
                && compare_completed_at(
                    candidate.candidate_completed_at,
                    candidate.installed_completed_at,
                ) == Ordering::Greater
            {
                format!("{summary} (newer UTC build timestamp)")
            } else {
                summary
            }
        })
        .collect();

    let conflict_subjects: Vec<_> = updates
        .iter()
        .map(|candidate| super::super::InstallConflictSubject {
            package: candidate.candidate_package.clone(),
            provides: candidate.provides.clone(),
            conflicts: candidate.conflicts.clone(),
        })
        .collect();
    super::super::validate_no_transaction_conflicts(&conflict_subjects)?;

    ui::info(format!("{} package(s) can be updated:", updates.len()));
    for target in &targets {
        ui::info(format!("  {}", target));
    }

    let db_path = config.installed_db_path(options.rootfs);
    let missing_deps = if options.no_deps {
        Vec::new()
    } else {
        collect_missing_update_dependencies(&updates, &db_path)?
    };
    if !missing_deps.is_empty() {
        ui::info(format!(
            "New dependencies required by updates: {}",
            missing_deps.join(", ")
        ));
    }

    if !options.dry_run && !ui::prompt_package_action("update", &targets, true)? {
        anyhow::bail!("Aborted");
    }

    if !options.dry_run {
        confirm_update_replacements(&updates)?;
    }

    if options.dry_run {
        super::super::resolve_installed_conflicts_for_subjects(
            &conflict_subjects,
            options.rootfs,
            config,
            true,
        )?;
    }

    let mut transaction_requests = Vec::new();
    if !missing_deps.is_empty() {
        let dep_plan = planner::build_dependency_install_plan(
            config,
            options.rootfs,
            &missing_deps,
            planner::PlannerOptions {
                assume_yes: options.assume_yes,
                prefer_binary: config.repo_settings.prefer_binary,
                local_sibling_root: None,
                include_test_deps: options.install_test_deps,
                lib32_only_requested_specs: false,
            },
        )?;
        crate::commands::print_plan_summary(&dep_plan);

        if options.dry_run {
            ui::info("Dry run enabled, stopping before dependency installation/update.");
            return Ok(());
        }

        transaction_requests.extend(super::super::install_requests_for_plan(
            &dep_plan,
            config,
            options.rootfs,
        )?);
    } else if options.dry_run {
        ui::info("Dry run enabled, stopping before update.");
        return Ok(());
    }

    for (idx, candidate) in updates.iter().enumerate() {
        ui::info(format!(
            "[{}/{}] staging update {} v{}-{} -> {} v{}-{}",
            idx + 1,
            updates.len(),
            candidate.installed_package,
            candidate.installed_version,
            candidate.installed_revision,
            candidate.candidate_package,
            candidate.candidate_version,
            candidate.candidate_revision
        ));
        let request = candidate_request_path(candidate, config, options.rootfs)?;
        transaction_requests.push(request);
    }
    super::super::run_update_transaction_install_requests(
        super::super::DirectInstallOptions {
            rootfs: options.rootfs,
            no_deps: true,
            no_flags: options.no_flags,
            cross_prefix: options.cross_prefix,
            clean: options.clean,
            dry_run: false,
            lib32_only: false,
            install_test_deps: options.install_test_deps,
        },
        config,
        &transaction_requests,
    )?;

    if options.clean {
        crate::commands::clean_build_workspace(config)?;
    }

    Ok(())
}
