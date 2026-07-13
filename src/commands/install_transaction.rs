use super::*;

#[derive(Debug, Clone)]
pub(super) struct PlannedStagedInstall {
    pub(super) is_update: bool,
    pub(super) remove_paths: Vec<String>,
    pub(super) replacement_removals: Vec<String>,
    pub(super) renamed_transition: Option<RenamedPackageTransition>,
    pub(super) hook_context: install::hooks::HookExecutionContextOwned,
}

#[derive(Debug, Clone)]
pub(super) struct RenamedPackageTransition {
    pub(super) replaced: db::InstalledPackageRecord,
    pub(super) retained_files: Vec<String>,
    pub(super) retained_directories: Vec<String>,
}

impl RenamedPackageTransition {
    fn replacement(&self) -> db::PackageReplacement {
        db::PackageReplacement {
            old_name: self.replaced.name.clone(),
            retained_files: self.retained_files.clone(),
            retained_directories: self.retained_directories.clone(),
        }
    }

    fn retains_old_package(&self) -> bool {
        !self.retained_files.is_empty() || !self.retained_directories.is_empty()
    }
}

#[derive(Debug, Clone)]
pub(super) struct PlannedPackageInstall {
    pub(super) spec: package::PackageSpec,
    pub(super) destdir: PathBuf,
    pub(super) staged: PlannedStagedInstall,
}

#[derive(Debug, Clone)]
pub(super) struct PlannedInstalledRemoval {
    pub(super) package: String,
    pub(super) affected_paths: Vec<String>,
}

pub(super) struct PreparedDirectInstallResources<'a> {
    pub(super) _staging_dir: Option<tempfile::TempDir>,
    pub(super) _source_cleanup_guard: SourceBuildCleanupGuard<'a>,
}

pub(super) struct PreparedDirectInstall<'a> {
    pub(super) plans: Vec<PlannedPackageInstall>,
    pub(super) resources: PreparedDirectInstallResources<'a>,
}

pub(super) struct DirectInstallPreparationOptions<'a> {
    pub(super) build_dir: &'a Path,
    pub(super) clean_sources_before_build: bool,
    pub(super) suppress_output: bool,
    pub(super) confirm_installation: bool,
    pub(super) resolve_installed_conflicts: bool,
}

#[derive(Clone, Copy)]
pub(super) struct PendingLifecycleHook {
    pub(super) hook: install::scripts::Hook,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct InstalledPackageOutcome {
    pub(super) package: package::PackageInfo,
    pub(super) is_update: bool,
}

#[derive(Debug, Clone)]
pub(super) struct InstallConflictSubject {
    pub(super) package: String,
    pub(super) provides: Vec<String>,
    pub(super) conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct InstalledConflictPackage {
    pub(super) name: String,
    pub(super) provides: Vec<String>,
}

pub(super) fn install_conflict_subjects_for_output_spec(
    spec: &package::PackageSpec,
) -> Vec<InstallConflictSubject> {
    spec.outputs()
        .into_iter()
        .map(|output| {
            let alternatives = spec.alternatives_for_output(&output.name);
            InstallConflictSubject {
                package: output.name,
                provides: alternatives.provides,
                conflicts: alternatives.conflicts,
            }
        })
        .collect()
}

pub(super) fn install_conflict_subjects_for_spec(
    spec: &package::PackageSpec,
    include_primary: bool,
    include_lib32: bool,
) -> Vec<InstallConflictSubject> {
    let mut subjects = Vec::new();
    if include_primary {
        subjects.extend(install_conflict_subjects_for_output_spec(spec));
    }
    if include_lib32 {
        subjects.extend(install_conflict_subjects_for_output_spec(
            &make_lib32_package_spec(spec),
        ));
    }
    subjects
}

pub(super) fn install_conflict_subject_for_binary_record(
    record: &db::repo::BinaryRepoPackageRecord,
) -> InstallConflictSubject {
    InstallConflictSubject {
        package: record.name.clone(),
        provides: record.provides.clone(),
        conflicts: record.conflicts.clone(),
    }
}

pub(super) fn matching_conflict_names(
    conflicts: &[String],
    package_name: &str,
    provides: &[String],
) -> Vec<String> {
    let mut matches = Vec::new();
    for conflict in conflicts {
        if conflict == package_name || provides.iter().any(|provided| provided == conflict) {
            matches.push(conflict.clone());
        }
    }
    matches.sort();
    matches.dedup();
    matches
}

pub(super) fn validate_no_transaction_conflicts(subjects: &[InstallConflictSubject]) -> Result<()> {
    let mut violations = BTreeSet::new();
    for (idx, left) in subjects.iter().enumerate() {
        for right in subjects.iter().skip(idx + 1) {
            let left_hits =
                matching_conflict_names(&left.conflicts, &right.package, &right.provides);
            if !left_hits.is_empty() {
                violations.insert(format!(
                    "{} conflicts with {} via {}",
                    left.package,
                    right.package,
                    left_hits.join(", ")
                ));
            }
            let right_hits =
                matching_conflict_names(&right.conflicts, &left.package, &left.provides);
            if !right_hits.is_empty() {
                violations.insert(format!(
                    "{} conflicts with {} via {}",
                    right.package,
                    left.package,
                    right_hits.join(", ")
                ));
            }
        }
    }

    if violations.is_empty() {
        return Ok(());
    }

    let mut message =
        String::from("Cannot install conflicting packages in the same transaction:\n");
    for violation in violations {
        message.push_str("  ");
        message.push_str(&violation);
        message.push('\n');
    }
    anyhow::bail!(message.trim_end().to_string());
}

pub(super) fn collect_installed_conflict_packages(
    db_path: &Path,
) -> Result<Vec<InstalledConflictPackage>> {
    let mut installed = Vec::new();
    for record in db::list_installed_package_records(db_path)? {
        installed.push(InstalledConflictPackage {
            provides: db::get_package_provides(db_path, &record.name)?,
            name: record.name,
        });
    }
    Ok(installed)
}

pub(super) fn collect_conflicting_installed_packages(
    subjects: &[InstallConflictSubject],
    installed: &[InstalledConflictPackage],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    validate_no_transaction_conflicts(subjects)?;
    let planned_packages: HashSet<_> = subjects
        .iter()
        .map(|subject| subject.package.clone())
        .collect();
    let mut removals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for subject in subjects {
        for installed_pkg in installed {
            if installed_pkg.name == subject.package {
                continue;
            }
            let matched = matching_conflict_names(
                &subject.conflicts,
                &installed_pkg.name,
                &installed_pkg.provides,
            );
            if matched.is_empty() {
                continue;
            }
            if planned_packages.contains(&installed_pkg.name) {
                anyhow::bail!(
                    "Cannot install conflicting packages in the same transaction: {} conflicts with {}",
                    subject.package,
                    installed_pkg.name
                );
            }
            removals
                .entry(installed_pkg.name.clone())
                .or_default()
                .insert(subject.package.clone());
        }
    }

    Ok(removals)
}

pub(super) fn collect_installed_replacement_packages(
    db_path: &Path,
    pkg_spec: &package::PackageSpec,
) -> Result<Vec<String>> {
    let installed = db::get_installed_packages(db_path)?;
    let mut replacements: Vec<String> = pkg_spec
        .alternatives
        .replaces
        .iter()
        .filter(|name| *name != &pkg_spec.package.name)
        .filter(|name| installed.contains(*name))
        .cloned()
        .collect();
    replacements.sort();
    replacements.dedup();
    Ok(replacements)
}

pub(crate) fn remove_installed_package_with_hooks(
    package: &str,
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    let db_path = config.installed_db_path(rootfs);
    let affected_paths = db::get_package_files(&db_path, package)?;
    install::hooks::run_transaction_hooks(
        rootfs,
        &install::hooks::HookExecutionContext {
            phase: install::hooks::HookPhase::Pre,
            operation: install::hooks::HookOperation::Remove,
            package,
            affected_paths: &affected_paths,
        },
    )?;
    remove_installed_package_without_transaction_hooks(package, rootfs, config, &affected_paths)?;
    install::hooks::run_transaction_hooks(
        rootfs,
        &install::hooks::HookExecutionContext {
            phase: install::hooks::HookPhase::Post,
            operation: install::hooks::HookOperation::Remove,
            package,
            affected_paths: &affected_paths,
        },
    )?;
    Ok(())
}

pub(super) fn remove_installed_package_without_transaction_hooks(
    package: &str,
    rootfs: &Path,
    config: &config::Config,
    _affected_paths: &[String],
) -> Result<()> {
    let db_path = config.installed_db_path(rootfs);
    let script_dir = install::scripts::installed_scripts_dir(rootfs, package);
    let _ = install::scripts::run_hook_if_present(
        &script_dir,
        install::scripts::Hook::PreRemove,
        rootfs,
        package,
    )?;
    db::remove_package(&db_path, package, rootfs)?;
    let post_remove = install::scripts::run_hook_if_present(
        &script_dir,
        install::scripts::Hook::PostRemove,
        rootfs,
        package,
    );
    let cleanup_scripts = install::scripts::remove_installed_scripts(rootfs, package);
    post_remove?;
    cleanup_scripts?;
    ui::success(format!("Successfully removed {}", package));
    Ok(())
}

pub(super) fn prompt_installed_conflict_removals_for_subjects(
    subjects: &[InstallConflictSubject],
    rootfs: &Path,
    config: &config::Config,
    dry_run: bool,
) -> Result<Vec<String>> {
    if subjects.is_empty() {
        return Ok(Vec::new());
    }

    let db_path = config.installed_db_path(rootfs);
    let installed = collect_installed_conflict_packages(&db_path)?;
    let removals = collect_conflicting_installed_packages(subjects, &installed)?;
    if removals.is_empty() {
        return Ok(Vec::new());
    }

    let prompt_entries: Vec<String> = removals
        .iter()
        .map(|(package, conflicted_by)| {
            format!(
                "{} (conflicts with {})",
                package,
                conflicted_by.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();

    if dry_run {
        ui::info(format!(
            "Dry run: would remove conflicting installed package(s): {}",
            prompt_entries.join(", ")
        ));
        return Ok(Vec::new());
    }

    if !ui::prompt_package_action("conflict removal", &prompt_entries, true)? {
        anyhow::bail!("Aborted");
    }

    Ok(removals.keys().cloned().collect())
}

pub(super) fn resolve_installed_conflicts_for_subjects(
    subjects: &[InstallConflictSubject],
    rootfs: &Path,
    config: &config::Config,
    dry_run: bool,
) -> Result<()> {
    for package in
        prompt_installed_conflict_removals_for_subjects(subjects, rootfs, config, dry_run)?
    {
        ui::info(format!("Removing conflicting package: {}", package));
        remove_installed_package_with_hooks(&package, rootfs, config)?;
    }

    Ok(())
}

pub(super) fn is_versioned_shared_library_path(path: &str) -> bool {
    let Some(file_name) = Path::new(path).file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(version_suffix) = file_name.split(".so.").nth(1) else {
        return false;
    };
    !version_suffix.is_empty()
        && version_suffix
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch == '.')
        && version_suffix.chars().any(|ch| ch.is_ascii_digit())
}

pub(super) fn retained_abi_files_for_replacement(
    old_files: &[String],
    new_manifest: &staging::Manifest,
) -> Vec<String> {
    let new_files: HashSet<&str> = new_manifest.files.iter().map(String::as_str).collect();
    let mut retained: Vec<String> = old_files
        .iter()
        .filter(|path| is_versioned_shared_library_path(path))
        .filter(|path| !new_files.contains(path.as_str()))
        .cloned()
        .collect();
    retained.sort();
    retained
}

pub(super) fn retained_directories_for_files(
    old_directories: &[String],
    retained_files: &[String],
) -> Vec<String> {
    let retained_files: HashSet<&str> = retained_files.iter().map(String::as_str).collect();
    let mut directories: Vec<String> = old_directories
        .iter()
        .filter(|directory| {
            let prefix = format!("{}/", directory);
            retained_files
                .iter()
                .any(|file| *file == directory.as_str() || file.starts_with(&prefix))
        })
        .cloned()
        .collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    directories
}

pub(super) fn compare_installed_records_for_stream(
    left: &db::InstalledPackageRecord,
    right: &db::InstalledPackageRecord,
) -> Ordering {
    compare_package_release(&left.version, left.revision, &right.version, right.revision)
        .then_with(|| compare_completed_at(left.completed_at, right.completed_at))
        .then_with(|| left.name.cmp(&right.name))
}

pub(super) fn select_primary_installed_record<'a>(
    records: impl IntoIterator<Item = &'a db::InstalledPackageRecord>,
) -> Option<&'a db::InstalledPackageRecord> {
    let mut best: Option<&db::InstalledPackageRecord> = None;
    for record in records {
        if best.as_ref().is_none_or(|current| {
            compare_installed_records_for_stream(record, current) == Ordering::Greater
        }) {
            best = Some(record);
        }
    }
    best
}

pub(super) fn build_renamed_package_transition(
    db_path: &Path,
    pkg_spec: &package::PackageSpec,
    new_manifest: &staging::Manifest,
) -> Result<Option<RenamedPackageTransition>> {
    let installed = db::list_installed_package_records(db_path)?;
    if installed
        .iter()
        .any(|record| record.name == pkg_spec.package.name)
    {
        return Ok(None);
    }

    let stream_name = pkg_spec.package.effective_real_name();
    let Some(replaced) = select_primary_installed_record(
        installed
            .iter()
            .filter(|record| record.effective_real_name() == stream_name)
            .filter(|record| record.name != pkg_spec.package.name),
    )
    .cloned() else {
        return Ok(None);
    };

    let old_files = db::get_package_files(db_path, &replaced.name)?;
    let old_directories = db::get_package_directories(db_path, &replaced.name)?;
    let retained_files = if replaced.abi_breaking {
        retained_abi_files_for_replacement(&old_files, new_manifest)
    } else {
        Vec::new()
    };
    let retained_directories = if retained_files.is_empty() {
        Vec::new()
    } else {
        retained_directories_for_files(&old_directories, &retained_files)
    };

    Ok(Some(RenamedPackageTransition {
        replaced,
        retained_files,
        retained_directories,
    }))
}

pub(super) fn plan_staged_install(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    rootfs: &Path,
    config: &config::Config,
) -> Result<PlannedStagedInstall> {
    std::fs::create_dir_all(&config.db_dir).with_context(|| {
        format!(
            "Failed to create database directory: {}",
            config.db_dir.display()
        )
    })?;
    let db_path = config.installed_db_path(rootfs);

    let new_manifest = staging::generate_manifest_with_dirs(destdir)?;
    let replacement_removals = collect_installed_replacement_packages(&db_path, pkg_spec)?;
    let renamed_transition = build_renamed_package_transition(&db_path, pkg_spec, &new_manifest)?;
    let is_update = db::get_package_version(&db_path, &pkg_spec.package.name)?.is_some()
        || renamed_transition.is_some()
        || !replacement_removals.is_empty();
    let mut remove_paths =
        db::calculate_upgrade_paths(&db_path, &pkg_spec.package.name, &new_manifest)?;
    if let Some(transition) = &renamed_transition {
        let old_files = db::get_package_files(&db_path, &transition.replaced.name)?;
        let old_directories = db::get_package_directories(&db_path, &transition.replaced.name)?;
        let retained_files: HashSet<&str> = transition
            .retained_files
            .iter()
            .map(String::as_str)
            .collect();
        let retained_directories: HashSet<&str> = transition
            .retained_directories
            .iter()
            .map(String::as_str)
            .collect();
        remove_paths.extend(
            old_files
                .into_iter()
                .filter(|path| !retained_files.contains(path.as_str())),
        );
        remove_paths.extend(
            old_directories
                .into_iter()
                .filter(|path| !retained_directories.contains(path.as_str())),
        );
        remove_paths.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
        remove_paths.dedup();
    }
    let operation = if is_update {
        install::hooks::HookOperation::Update
    } else {
        install::hooks::HookOperation::Install
    };
    let mut affected_paths = new_manifest.files.clone();
    affected_paths.extend(remove_paths.iter().cloned());
    affected_paths.sort();
    affected_paths.dedup();

    Ok(PlannedStagedInstall {
        is_update,
        remove_paths,
        replacement_removals,
        renamed_transition,
        hook_context: install::hooks::HookExecutionContextOwned {
            operation,
            package: pkg_spec.package.name.clone(),
            affected_paths,
        },
    })
}

pub(super) fn plan_package_outputs_for_install(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    rootfs: &Path,
    config: &config::Config,
) -> Result<Vec<PlannedPackageInstall>> {
    let mut plans = Vec::new();
    for (spec_for_out, out_destdir) in staged_output_specs(pkg_spec, destdir)? {
        let staged = plan_staged_install(&spec_for_out, &out_destdir, rootfs, config)?;
        plans.push(PlannedPackageInstall {
            spec: spec_for_out,
            destdir: out_destdir,
            staged,
        });
    }
    Ok(plans)
}

pub(super) fn run_transaction_hooks_for_plans(
    rootfs: &Path,
    phase: install::hooks::HookPhase,
    plans: &[PlannedPackageInstall],
) -> Result<usize> {
    let contexts: Vec<_> = plans
        .iter()
        .map(|plan| plan.staged.hook_context.clone())
        .collect();
    install::hooks::run_transaction_hooks_batch(rootfs, phase, &contexts)
}

pub(super) fn preflight_file_ownership_and_order(
    plans: &[PlannedPackageInstall],
    pre_removed_packages: &HashSet<String>,
    rootfs: &Path,
    config: &config::Config,
) -> Result<Vec<PlannedPackageInstall>> {
    let db_path = config.installed_db_path(rootfs);
    let installed_ownership = db::get_file_ownership(&db_path)?;
    let mut manifests = Vec::with_capacity(plans.len());
    let mut plan_by_package = BTreeMap::new();
    let mut replacement_plan_by_package = BTreeMap::new();
    let mut violations = BTreeSet::new();

    for (idx, plan) in plans.iter().enumerate() {
        let package = &plan.spec.package.name;
        if plan_by_package.insert(package.clone(), idx).is_some() {
            violations.insert(format!(
                "package '{}' appears more than once in the transaction",
                package
            ));
        }
        for replaced in &plan.staged.replacement_removals {
            if replacement_plan_by_package
                .insert(replaced.clone(), idx)
                .is_some()
            {
                violations.insert(format!(
                    "installed package '{}' is replaced by more than one transaction package",
                    replaced
                ));
            }
        }
        if let Some(transition) = &plan.staged.renamed_transition
            && replacement_plan_by_package
                .insert(transition.replaced.name.clone(), idx)
                .is_some()
        {
            violations.insert(format!(
                "installed package '{}' is replaced by more than one transaction package",
                transition.replaced.name
            ));
        }

        let manifest = staging::generate_manifest_with_dirs(&plan.destdir).with_context(|| {
            format!(
                "Failed to inspect staged files for package '{}'",
                plan.spec.package.name
            )
        })?;
        manifests.push(manifest.files.into_iter().collect::<BTreeSet<_>>());
    }

    let mut planned_owner_by_path: BTreeMap<&str, usize> = BTreeMap::new();
    for (idx, manifest) in manifests.iter().enumerate() {
        for path in manifest {
            if let Some(previous_idx) = planned_owner_by_path.insert(path, idx)
                && plans[previous_idx].spec.package.name != plans[idx].spec.package.name
                && !db::should_auto_clear_conflict(&plans[previous_idx].spec.package.name, path)
            {
                violations.insert(format!(
                    "{} -> provided by both {} and {}",
                    path, plans[previous_idx].spec.package.name, plans[idx].spec.package.name
                ));
            }
        }
    }

    let mut edges = vec![BTreeSet::new(); plans.len()];
    let mut indegree = vec![0_usize; plans.len()];
    for (taker_idx, manifest) in manifests.iter().enumerate() {
        let taker = &plans[taker_idx].spec.package.name;
        for path in manifest {
            let Some(owner) = installed_ownership.get(path) else {
                continue;
            };
            if owner == taker
                || pre_removed_packages.contains(owner)
                || db::should_auto_clear_conflict(owner, path)
            {
                continue;
            }

            let owner_plan_idx = if let Some(owner_idx) = plan_by_package.get(owner) {
                if manifests[*owner_idx].contains(path) {
                    violations.insert(format!(
                        "{} -> owned by {} and still provided by its transaction update (wanted by {})",
                        path, owner, taker
                    ));
                    continue;
                }
                Some(*owner_idx)
            } else if let Some(owner_idx) = replacement_plan_by_package.get(owner) {
                let retained_by_rename = plans[*owner_idx]
                    .staged
                    .renamed_transition
                    .as_ref()
                    .is_some_and(|transition| {
                        transition.replaced.name == *owner
                            && transition.retained_files.contains(path)
                    });
                if retained_by_rename {
                    violations.insert(format!(
                        "{} -> retained by renamed package {} (wanted by {})",
                        path, owner, taker
                    ));
                    continue;
                }
                Some(*owner_idx)
            } else {
                violations.insert(format!(
                    "{} -> owned by {} (wanted by {})",
                    path, owner, taker
                ));
                None
            };

            if let Some(owner_idx) = owner_plan_idx
                && owner_idx != taker_idx
                && edges[owner_idx].insert(taker_idx)
            {
                indegree[taker_idx] += 1;
            }
        }
    }

    if !violations.is_empty() {
        let mut message = String::from("File ownership conflict detected before transaction:\n");
        for violation in violations {
            message.push_str(&format!("  {violation}\n"));
        }
        anyhow::bail!(message);
    }

    let mut order = Vec::with_capacity(plans.len());
    let mut emitted = vec![false; plans.len()];
    while order.len() < plans.len() {
        let Some(next) = (0..plans.len()).find(|idx| !emitted[*idx] && indegree[*idx] == 0) else {
            let packages = (0..plans.len())
                .filter(|idx| !emitted[*idx])
                .map(|idx| plans[idx].spec.package.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "File ownership handoff cycle detected before transaction among: {}",
                packages
            );
        };
        emitted[next] = true;
        order.push(next);
        for dependent in &edges[next] {
            indegree[*dependent] -= 1;
        }
    }

    Ok(order.into_iter().map(|idx| plans[idx].clone()).collect())
}

pub(super) fn install_staged_to_rootfs(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    rootfs: &Path,
    config: &config::Config,
    plan: &PlannedStagedInstall,
) -> Result<Option<PendingLifecycleHook>> {
    let staged_scripts_dir = install::scripts::staged_scripts_dir(destdir);
    let installed_scripts_dir =
        install::scripts::installed_scripts_dir(rootfs, &pkg_spec.package.name);

    if plan.is_update {
        let has_staged_pre = install::scripts::run_hook_if_present(
            &staged_scripts_dir,
            install::scripts::Hook::PreUpdate,
            rootfs,
            &pkg_spec.package.name,
        )?;
        if !has_staged_pre {
            let _ = install::scripts::run_hook_if_present(
                &installed_scripts_dir,
                install::scripts::Hook::PreUpdate,
                rootfs,
                &pkg_spec.package.name,
            )?;
        }
    } else {
        let _ = install::scripts::run_hook_if_present(
            &staged_scripts_dir,
            install::scripts::Hook::PreInstall,
            rootfs,
            &pkg_spec.package.name,
        )?;
    }

    let tx_base = config.build_dir.join("tx");
    let tx = staging::install_atomic(
        destdir,
        rootfs,
        &tx_base,
        &plan.remove_paths,
        &pkg_spec.build.flags.keep,
    )?;

    let db_path = config.installed_db_path(rootfs);
    let replacement = plan
        .renamed_transition
        .as_ref()
        .map(RenamedPackageTransition::replacement);
    let register_result = if let Some(replacement) = replacement.as_ref() {
        db::register_package_with_replacement(&db_path, pkg_spec, destdir, Some(replacement))
    } else {
        db::register_package(&db_path, pkg_spec, destdir)
    };
    if let Err(e) = register_result {
        let _ = tx.rollback();
        return Err(e);
    }
    tx.commit()?;

    if let Some(transition) = &plan.renamed_transition
        && !transition.retains_old_package()
    {
        install::scripts::remove_installed_scripts(rootfs, &transition.replaced.name)?;
    }

    install::scripts::sync_staged_scripts_to_rootfs(
        &staged_scripts_dir,
        rootfs,
        &pkg_spec.package.name,
    )?;

    Ok(Some(PendingLifecycleHook {
        hook: if plan.is_update {
            install::scripts::Hook::PostUpdate
        } else {
            install::scripts::Hook::PostInstall
        },
    }))
}

pub(super) fn install_planned_packages_to_rootfs(
    plans: &[PlannedPackageInstall],
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    install_planned_packages_to_rootfs_with_pre_removed(
        plans,
        rootfs,
        config,
        &HashSet::new(),
        true,
    )
}

pub(super) fn install_planned_packages_to_rootfs_with_pre_removed(
    plans: &[PlannedPackageInstall],
    rootfs: &Path,
    config: &config::Config,
    pre_removed_packages: &HashSet<String>,
    show_progress: bool,
) -> Result<()> {
    let ordered_plans =
        preflight_file_ownership_and_order(plans, pre_removed_packages, rootfs, config)?;
    install_preflighted_planned_packages_to_rootfs_with_pre_removed(
        &ordered_plans,
        rootfs,
        config,
        pre_removed_packages,
        show_progress,
    )
}

pub(super) fn install_preflighted_planned_packages_to_rootfs_with_pre_removed(
    plans: &[PlannedPackageInstall],
    rootfs: &Path,
    config: &config::Config,
    pre_removed_packages: &HashSet<String>,
    show_progress: bool,
) -> Result<()> {
    let mut removed_replacements = HashSet::new();
    let mut pending_post_hooks = Vec::new();
    for (idx, plan) in plans.iter().enumerate() {
        if show_progress {
            ui::info(format!(
                "{}/{} Installing package {}-{}-{}",
                idx + 1,
                plans.len(),
                plan.spec.package.name,
                plan.spec.package.version,
                plan.spec.package.revision
            ));
        }
        for package in &plan.staged.replacement_removals {
            if pre_removed_packages.contains(package) {
                continue;
            }
            if removed_replacements.insert(package.clone()) {
                remove_installed_package_with_hooks(package, rootfs, config)?;
            }
        }
        if let Some(hook) =
            install_staged_to_rootfs(&plan.spec, &plan.destdir, rootfs, config, &plan.staged)?
        {
            pending_post_hooks.push((plan.spec.package.name.clone(), hook));
        }
    }
    // Lifecycle hooks may invoke sh, cc, or ld. Select a sole provider before
    // any post-install hook runs so the aliases are usable within this transaction.
    set::auto_select_sole_tool_providers(rootfs, config)?;
    for (pkg_name, pending_hook) in pending_post_hooks {
        let installed_scripts_dir = install::scripts::installed_scripts_dir(rootfs, &pkg_name);
        let _ = install::scripts::run_hook_if_present_or_defer(
            &installed_scripts_dir,
            pending_hook.hook,
            rootfs,
            &pkg_name,
        )?;
    }
    install::scripts::run_deferred_hooks_if_possible(rootfs)?;
    Ok(())
}

pub(super) fn run_parallel_tasks<T, U, F>(
    items: &[T],
    worker_count: usize,
    task: F,
) -> Result<Vec<U>>
where
    T: Sync,
    U: Send,
    F: Fn(usize, &T) -> Result<U> + Sync,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = worker_count.max(1).min(items.len());
    let next_index = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();

    std::thread::scope(|scope| -> Result<Vec<U>> {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let task = &task;
            let next_index = &next_index;
            scope.spawn(move || {
                loop {
                    let index = next_index.fetch_add(1, AtomicOrdering::Relaxed);
                    if index >= items.len() {
                        break;
                    }
                    let result = task(index, &items[index]);
                    if sender.send((index, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);

        let mut results: Vec<Option<Result<U>>> = (0..items.len()).map(|_| None).collect();
        for _ in 0..items.len() {
            let (index, result) = receiver
                .recv()
                .context("Parallel worker exited before reporting a result")?;
            results[index] = Some(result);
        }

        results
            .into_iter()
            .map(|result| result.expect("every parallel item must report a result"))
            .collect()
    })
}

pub(super) fn run_parallel_verification<T, F>(
    items: &[T],
    progress: &ProgressBar,
    verify: F,
) -> Result<()>
where
    T: Sync,
    F: Fn(&T) -> Result<()> + Sync,
{
    let worker_count = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    run_parallel_tasks(items, worker_count, |_, item| {
        let result = verify(item);
        progress.inc(1);
        result
    })?;
    Ok(())
}

#[cfg(test)]
pub(super) fn install_package_outputs_to_rootfs(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    rootfs: &Path,
    config: &config::Config,
) -> Result<Vec<InstalledPackageOutcome>> {
    let plans = plan_package_outputs_for_install(pkg_spec, destdir, rootfs, config)?;
    let ordered_plans =
        preflight_file_ownership_and_order(&plans, &HashSet::new(), rootfs, config)?;
    let installed = plans
        .iter()
        .map(|plan| InstalledPackageOutcome {
            package: plan.spec.package.clone(),
            is_update: plan.staged.is_update,
        })
        .collect();
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Pre, &ordered_plans)?;
    install_preflighted_planned_packages_to_rootfs_with_pre_removed(
        &ordered_plans,
        rootfs,
        config,
        &HashSet::new(),
        true,
    )?;
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Post, &ordered_plans)?;
    Ok(installed)
}
