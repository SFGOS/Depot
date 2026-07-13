use super::*;

pub(super) fn is_archive_install_request(spec_path: &Path) -> bool {
    spec_path.exists()
        && spec_path
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".tar.zst")
}

pub(super) fn shared_local_sibling_root(spec_paths: &[PathBuf]) -> Option<PathBuf> {
    let mut roots = spec_paths.iter().filter_map(|path| {
        path.parent()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
    });
    let first = roots.next()?;
    if roots.all(|path| path == first) {
        Some(first)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
pub(super) struct DirectInstallOptions<'a> {
    pub(super) rootfs: &'a Path,
    pub(super) no_deps: bool,
    pub(super) no_flags: bool,
    pub(super) cross_prefix: Option<&'a str>,
    pub(super) clean: bool,
    pub(super) dry_run: bool,
    pub(super) lib32_only: bool,
    pub(super) install_test_deps: bool,
}

pub(super) fn run_direct_archive_install_requests(
    options: DirectInstallOptions<'_>,
    config: &config::Config,
    archive_paths: &[PathBuf],
    confirm_installation: bool,
) -> Result<bool> {
    if archive_paths.is_empty() {
        return Ok(false);
    }

    let mut install_lock = locking::open_lock(config)?;
    let install_lock_path = locking::lock_path(config);
    let _install_lock_guard = locking::try_write(&mut install_lock, &install_lock_path, "install")?;

    let mut staged_dirs = Vec::with_capacity(archive_paths.len());
    let mut pkg_specs = Vec::with_capacity(archive_paths.len());
    let mut install_targets = Vec::with_capacity(archive_paths.len());
    let suppress_output = suppress_nested_install_output();

    for archive_path in archive_paths {
        if !suppress_output {
            ui::info(format!(
                "Installing package from: {}",
                archive_path.display()
            ));
        }

        let (pkg_spec, staging_dir) = load_package_archive_into_staging(config, archive_path)?;
        if options.lib32_only {
            anyhow::bail!("--lib32-only is only supported when installing from a package spec");
        }

        install_targets.push(format!(
            "{} v{}-{}",
            pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
        ));
        pkg_specs.push(pkg_spec);
        staged_dirs.push(staging_dir);
    }

    let mut conflict_subjects = Vec::new();
    for pkg_spec in &pkg_specs {
        conflict_subjects.extend(install_conflict_subjects_for_spec(pkg_spec, true, false));
    }
    resolve_installed_conflicts_for_subjects(
        &conflict_subjects,
        options.rootfs,
        config,
        options.dry_run,
    )?;

    if options.dry_run {
        ui::info("Dry run enabled, stopping before install/build work.");
        return Ok(false);
    }

    if confirm_installation
        && !suppress_output
        && !ui::prompt_package_action("installation", &install_targets, true)?
    {
        anyhow::bail!("Aborted");
    }

    if !suppress_output {
        ui::info(format!(
            "Installing {} binary archive payload(s)",
            archive_paths.len()
        ));
    }

    let mut transaction_plans = Vec::new();
    for (pkg_spec, staging_dir) in pkg_specs.iter().zip(staged_dirs.iter()) {
        let output_plans =
            plan_package_outputs_for_install(pkg_spec, staging_dir.path(), options.rootfs, config)?;
        transaction_plans.extend(output_plans);
    }

    install_direct_transaction(&transaction_plans, options.rootfs, config)?;

    Ok(true)
}

pub(super) struct SourceBuildCleanupGuard<'a> {
    pub(super) config: &'a config::Config,
    pub(super) enabled: bool,
}

impl<'a> SourceBuildCleanupGuard<'a> {
    fn new(config: &'a config::Config, enabled: bool) -> Self {
        Self { config, enabled }
    }
}

impl Drop for SourceBuildCleanupGuard<'_> {
    fn drop(&mut self) {
        if self.enabled
            && let Err(err) = clean_build_source_dirs(self.config)
        {
            crate::log_warn!("Failed to clean build source dirs: {}", err);
        }
    }
}

pub(super) fn prepare_direct_install_request<'a>(
    options: DirectInstallOptions<'_>,
    config: &'a config::Config,
    spec_path: &Path,
    preparation: DirectInstallPreparationOptions<'_>,
) -> Result<PreparedDirectInstall<'a>> {
    let (mut pkg_spec, staging_dir): (package::PackageSpec, Option<tempfile::TempDir>) =
        if spec_path.to_string_lossy().ends_with(".tar.zst") {
            let (spec, tmp_dir) = load_package_archive_into_staging(config, spec_path)?;
            (spec, Some(tmp_dir))
        } else {
            let mut pkg_spec = package::PackageSpec::from_file(spec_path)?;
            pkg_spec.apply_config(config);
            pkg_spec.build.flags.rootfs = build_cmd::build_env_rootfs(options.rootfs);
            (pkg_spec, None)
        };
    let built_from_source = staging_dir.is_none();
    let source_cleanup_guard = SourceBuildCleanupGuard::new(config, built_from_source);

    if options.lib32_only && staging_dir.is_some() {
        anyhow::bail!("--lib32-only is only supported when installing from a package spec");
    }
    let lib32_only = effective_lib32_only(&pkg_spec, options.lib32_only);

    if staging_dir.is_none() && !preparation.suppress_output {
        ui::info(format!(
            "Package: {} v{}-{}",
            pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
        ));
    }

    let requested_outputs = requested_outputs(&pkg_spec, lib32_only);
    let db_path = config.installed_db_path(options.rootfs);

    if staging_dir.is_none() {
        source::preflight_local_manual_sources(&pkg_spec)?;
        if !pkg_spec.is_metapackage() {
            ensure_requested_development_package_installed(&db_path)?;
        }
    }

    let mut conflict_subjects = install_conflict_subjects_for_spec(
        &pkg_spec,
        !lib32_only,
        staging_dir.is_none() && (lib32_only || pkg_spec.builds_lib32_output()),
    );
    if staging_dir.is_some() {
        conflict_subjects = install_conflict_subjects_for_spec(&pkg_spec, true, false);
    }
    if preparation.resolve_installed_conflicts {
        resolve_installed_conflicts_for_subjects(
            &conflict_subjects,
            options.rootfs,
            config,
            options.dry_run,
        )?;
    }

    if options.dry_run {
        ui::info("Dry run enabled, stopping before install/build work.");
        return Ok(PreparedDirectInstall {
            plans: Vec::new(),
            resources: PreparedDirectInstallResources {
                _staging_dir: staging_dir,
                _source_cleanup_guard: source_cleanup_guard,
            },
        });
    }

    let install_targets = vec![format!(
        "{} v{}-{}",
        pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
    )];
    if preparation.confirm_installation
        && !preparation.suppress_output
        && !ui::prompt_package_action("installation", &install_targets, true)?
    {
        anyhow::bail!("Aborted");
    }

    std::fs::create_dir_all(&config.db_dir).with_context(|| {
        format!(
            "Failed to create database directory: {}",
            config.db_dir.display()
        )
    })?;

    if staging_dir.is_none() {
        if options.no_deps
            && should_install_test_deps(&pkg_spec, options.install_test_deps, requested_outputs)
        {
            let missing_test =
                deps::check_test_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            if !missing_test.is_empty()
                && !maybe_prompt_to_skip_tests_for_missing_requested_deps(
                    &mut pkg_spec,
                    &missing_test,
                    "Requested test dependencies are missing",
                )?
            {
                anyhow::bail!("Missing test dependencies: {}", missing_test.join(", "));
            }
        } else if options.no_deps
            || !should_install_test_deps(&pkg_spec, options.install_test_deps, requested_outputs)
        {
            maybe_disable_tests_for_missing_deps(&mut pkg_spec, &db_path, requested_outputs)?;
        }
    }

    if !options.no_deps {
        let missing_required = merge_missing_dependencies(
            deps::check_build_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?,
            deps::check_runtime_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?,
        );
        if !missing_required.is_empty() {
            let dep_chain = std::env::var("DEPOT_DEPCHAIN").unwrap_or_default();
            let chain_set: std::collections::HashSet<&str> =
                dep_chain.split(',').filter(|s| !s.is_empty()).collect();

            if chain_set.contains(pkg_spec.package.name.as_str()) {
                anyhow::bail!(
                    "Dependency cycle detected! {} is already in chain: {}",
                    pkg_spec.package.name,
                    dep_chain
                );
            }

            ui::warn(format!(
                "Missing dependencies: {}",
                missing_required.join(", ")
            ));
            let local_sibling_root = spec_path.parent().and_then(|path| path.parent());
            let dep_plan = planner::build_dependency_install_plan(
                config,
                options.rootfs,
                &missing_required,
                planner::PlannerOptions {
                    assume_yes: ui::assume_yes_enabled(),
                    prefer_binary: config.repo_settings.prefer_binary,
                    local_sibling_root: local_sibling_root.map(Path::to_path_buf),
                    include_test_deps: options.install_test_deps,
                    lib32_only_requested_specs: false,
                },
            )?;
            let dep_plan_packages = actionable_plan_packages(&dep_plan);
            warn_source_build_plan(&dep_plan);
            let dep_prompt_packages = if dep_plan_packages.is_empty() {
                missing_required.clone()
            } else {
                dep_plan_packages
            };
            if ui::prompt_package_action("dependency installation", &dep_prompt_packages, true)? {
                let pkg_index =
                    index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));

                let new_chain = if dep_chain.is_empty() {
                    pkg_spec.package.name.clone()
                } else {
                    format!("{},{}", dep_chain, pkg_spec.package.name)
                };

                let mut dep_spec_paths = Vec::new();
                for dep in missing_required {
                    let candidate = pkg_index.find(&dep);

                    if let Some(dep_spec_path) = candidate {
                        dep_spec_paths.push(dep_spec_path);
                    } else {
                        anyhow::bail!("Could not find package spec for dependency: {}", dep);
                    }
                }
                ui::info(format!(
                    "Installing dependencies: {}",
                    install_request_display(&dep_spec_paths)
                ));
                let exe = std::env::current_exe().context("Failed to locate depot executable")?;
                run_install_command_with_program(
                    &exe,
                    &dep_spec_paths,
                    options.rootfs,
                    ChildInstallCommandOptions {
                        no_deps: options.no_deps,
                        assume_yes: true,
                        no_flags: options.no_flags,
                        cross_prefix: options.cross_prefix,
                        clean: options.clean,
                        lib32_only: false,
                        install_test_deps: options.install_test_deps,
                        install_context: Some(INSTALL_CONTEXT_PLANNED),
                        dep_chain: Some(&new_chain),
                    },
                )?;
            }
        }

        deps::require_build_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
        deps::require_runtime_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
        if should_install_test_deps(&pkg_spec, options.install_test_deps, requested_outputs) {
            let missing_test =
                deps::check_test_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            if !missing_test.is_empty() {
                let pkg_index =
                    index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));
                let mut dep_spec_paths = Vec::new();
                let mut unavailable_test = Vec::new();
                for dep in &missing_test {
                    if let Some(dep_spec_path) = pkg_index.find(dep) {
                        dep_spec_paths.push(dep_spec_path);
                    } else {
                        unavailable_test.push(dep.clone());
                    }
                }

                if !unavailable_test.is_empty()
                    && !maybe_prompt_to_skip_tests_for_missing_requested_deps(
                        &mut pkg_spec,
                        &unavailable_test,
                        "Requested test dependencies could not be resolved",
                    )?
                {
                    anyhow::bail!("Missing test dependencies: {}", unavailable_test.join(", "));
                }

                if !automatic_tests_disabled_for_outputs(&pkg_spec, requested_outputs)
                    && !dep_spec_paths.is_empty()
                {
                    ui::warn(format!(
                        "Missing test dependencies: {}",
                        missing_test.join(", ")
                    ));
                    let local_sibling_root = spec_path.parent().and_then(|path| path.parent());
                    let dep_plan = planner::build_dependency_install_plan(
                        config,
                        options.rootfs,
                        &missing_test,
                        planner::PlannerOptions {
                            assume_yes: ui::assume_yes_enabled(),
                            prefer_binary: config.repo_settings.prefer_binary,
                            local_sibling_root: local_sibling_root.map(Path::to_path_buf),
                            include_test_deps: options.install_test_deps,
                            lib32_only_requested_specs: false,
                        },
                    )?;
                    let dep_plan_packages = actionable_plan_packages(&dep_plan);
                    warn_source_build_plan(&dep_plan);
                    let dep_prompt_packages = if dep_plan_packages.is_empty() {
                        missing_test.clone()
                    } else {
                        dep_plan_packages
                    };
                    if ui::prompt_package_action(
                        "dependency installation",
                        &dep_prompt_packages,
                        true,
                    )? {
                        ui::info(format!(
                            "Installing test dependencies: {}",
                            install_request_display(&dep_spec_paths)
                        ));
                        let exe =
                            std::env::current_exe().context("Failed to locate depot executable")?;
                        run_install_command_with_program(
                            &exe,
                            &dep_spec_paths,
                            options.rootfs,
                            ChildInstallCommandOptions {
                                no_deps: options.no_deps,
                                assume_yes: true,
                                no_flags: options.no_flags,
                                cross_prefix: options.cross_prefix,
                                clean: options.clean,
                                lib32_only: false,
                                install_test_deps: options.install_test_deps,
                                install_context: Some(INSTALL_CONTEXT_PLANNED),
                                dep_chain: None,
                            },
                        )?;
                    } else if !maybe_prompt_to_skip_tests_for_missing_requested_deps(
                        &mut pkg_spec,
                        &missing_test,
                        "Requested test dependencies were not installed",
                    )? {
                        anyhow::bail!("Aborted");
                    }
                }
            }
        }

        if should_install_test_deps(&pkg_spec, options.install_test_deps, requested_outputs) {
            let missing_test =
                deps::check_test_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            if !missing_test.is_empty()
                && !maybe_prompt_to_skip_tests_for_missing_requested_deps(
                    &mut pkg_spec,
                    &missing_test,
                    "Requested test dependencies are still missing",
                )?
            {
                deps::require_test_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            }
        }
    }

    let cross_config = options
        .cross_prefix
        .map(cross::CrossConfig::from_prefix)
        .transpose()?;
    let mut built_src_dir: Option<PathBuf> = None;

    let destdir = if let Some(dir) = &staging_dir {
        dir.path().to_path_buf()
    } else {
        if preparation.clean_sources_before_build {
            clean_build_source_dirs(config)?;
        }
        source::preflight_manual_sources(&pkg_spec, &config.cache_dir)?;
        let src_dir = source::prepare(&pkg_spec, &config.cache_dir, preparation.build_dir)?;
        built_src_dir = Some(src_dir.clone());
        let host_build_dir = builder::ensure_host_build(
            &pkg_spec,
            &src_dir,
            cross_config.as_ref(),
            !options.no_flags,
            builder::TargetBuildKind::Primary,
        )?;
        if let Some(host_dir) = host_build_dir.as_ref() {
            pkg_spec.build.flags.host_build_dir = Some(host_dir.to_string_lossy().into_owned());
        }

        let destdir = preparation
            .build_dir
            .join("destdir")
            .join(&pkg_spec.package.name);
        if destdir.exists() {
            fs::remove_dir_all(&destdir)
                .with_context(|| format!("Failed to clean destdir: {}", destdir.display()))?;
        }

        if !lib32_only {
            builder::build(
                &pkg_spec,
                &src_dir,
                &destdir,
                cross_config.as_ref(),
                !options.no_flags,
                host_build_dir.as_deref(),
            )?;

            staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;
            install::scripts::stage_scripts_from_spec_dir(&pkg_spec, &destdir)?;
            builder::stage_generated_lifecycle_scripts(&pkg_spec, &destdir)?;
        }

        destdir
    };

    let mut transaction_plans = Vec::new();

    if !lib32_only {
        if staging_dir.is_none() {
            staging::process(&destdir, &pkg_spec)?;
            if let Some(src_dir) = built_src_dir.as_deref() {
                staging::stage_split_package_licenses(src_dir, &destdir, &pkg_spec)?;
            }
        } else if !preparation.suppress_output {
            ui::info("Installing binary archive payload");
        }

        let output_plans =
            plan_package_outputs_for_install(&pkg_spec, &destdir, options.rootfs, config)?;
        transaction_plans.extend(output_plans);
    }

    if let Some(src_dir) = built_src_dir.as_deref()
        && let Some((lib32_spec, lib32_destdir)) = build_lib32_companion_package(
            &pkg_spec,
            src_dir,
            config,
            cross_config.as_ref(),
            !options.no_flags,
            lib32_only,
        )?
    {
        let staged = plan_staged_install(&lib32_spec, &lib32_destdir, options.rootfs, config)?;
        transaction_plans.push(PlannedPackageInstall {
            spec: lib32_spec,
            destdir: lib32_destdir,
            staged,
        });
    }

    Ok(PreparedDirectInstall {
        plans: transaction_plans,
        resources: PreparedDirectInstallResources {
            _staging_dir: staging_dir,
            _source_cleanup_guard: source_cleanup_guard,
        },
    })
}

pub(super) fn install_direct_transaction(
    plans: &[PlannedPackageInstall],
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    let ordered_plans = preflight_file_ownership_and_order(plans, &HashSet::new(), rootfs, config)?;
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Pre, &ordered_plans)?;
    install_preflighted_planned_packages_to_rootfs_with_pre_removed(
        &ordered_plans,
        rootfs,
        config,
        &HashSet::new(),
        true,
    )?;
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Post, &ordered_plans)?;
    Ok(())
}

pub(super) fn install_requests_for_plan(
    plan: &planner::ExecutionPlan,
    config: &config::Config,
    rootfs: &Path,
) -> Result<Vec<PathBuf>> {
    let mut requests = Vec::new();
    for step in plan.actionable_steps() {
        match &step.origin {
            planner::PlanOrigin::Source { path, .. } => {
                requests.push(path.clone());
            }
            planner::PlanOrigin::Binary { repo_name, record } => {
                let repo_cfg = config
                    .binary_repos
                    .get(repo_name)
                    .with_context(|| format!("Binary repo '{}' not found in config", repo_name))?;
                let archive = db::repo::fetch_binary_package_archive(
                    repo_name,
                    repo_cfg,
                    rootfs,
                    record,
                    &config.package_cache_dir,
                )
                .with_context(|| {
                    format!(
                        "Failed to fetch binary package '{}' from repo '{}'",
                        record.filename, repo_name
                    )
                })?;
                requests.push(archive);
            }
            planner::PlanOrigin::Installed => {}
        }
    }
    Ok(requests)
}

pub(super) fn planned_installed_removals(
    rootfs: &Path,
    config: &config::Config,
    packages: impl IntoIterator<Item = String>,
) -> Result<Vec<PlannedInstalledRemoval>> {
    let db_path = config.installed_db_path(rootfs);
    let installed = db::get_installed_packages(&db_path)?;
    let mut unique = BTreeSet::new();
    for package in packages {
        if installed.contains(&package) {
            unique.insert(package);
        }
    }

    unique
        .into_iter()
        .map(|package| {
            let affected_paths = db::get_package_files(&db_path, &package)?;
            Ok(PlannedInstalledRemoval {
                package,
                affected_paths,
            })
        })
        .collect()
}

pub(super) fn transaction_contexts_for_update(
    removals: &[PlannedInstalledRemoval],
    plans: &[PlannedPackageInstall],
) -> Vec<install::hooks::HookExecutionContextOwned> {
    let mut contexts = Vec::with_capacity(removals.len() + plans.len());
    contexts.extend(
        removals
            .iter()
            .map(|removal| install::hooks::HookExecutionContextOwned {
                operation: install::hooks::HookOperation::Remove,
                package: removal.package.clone(),
                affected_paths: removal.affected_paths.clone(),
            }),
    );
    contexts.extend(plans.iter().map(|plan| plan.staged.hook_context.clone()));
    contexts
}

pub(super) fn install_update_transaction(
    plans: &[PlannedPackageInstall],
    removals: &[PlannedInstalledRemoval],
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    let pre_removed_packages: HashSet<String> = removals
        .iter()
        .map(|removal| removal.package.clone())
        .collect();
    let ordered_plans =
        preflight_file_ownership_and_order(plans, &pre_removed_packages, rootfs, config)?;
    let contexts = transaction_contexts_for_update(removals, &ordered_plans);
    install::hooks::run_transaction_hooks_batch(rootfs, install::hooks::HookPhase::Pre, &contexts)?;

    for removal in removals {
        remove_installed_package_without_transaction_hooks(
            &removal.package,
            rootfs,
            config,
            &removal.affected_paths,
        )?;
    }

    install_preflighted_planned_packages_to_rootfs_with_pre_removed(
        &ordered_plans,
        rootfs,
        config,
        &pre_removed_packages,
        false,
    )?;
    install::hooks::run_transaction_hooks_batch(
        rootfs,
        install::hooks::HookPhase::Post,
        &contexts,
    )?;
    Ok(())
}

pub(super) fn run_direct_install_request(
    options: DirectInstallOptions<'_>,
    config: &config::Config,
    mut spec_path: PathBuf,
) -> Result<bool> {
    let mut install_lock = locking::open_lock(config)?;
    let install_lock_path = locking::lock_path(config);
    let _install_lock_guard = locking::try_write(&mut install_lock, &install_lock_path, "install")?;

    // Repo clone dir is available via `config.repo_clone_dir` and
    // is passed explicitly to index builders below.

    // If the provided path doesn't exist, treat it as a package name and
    // try to locate a spec under configured repo dir or local packages/.
    if !spec_path.exists() {
        let name = spec_path.to_string_lossy().to_string();
        ui::info(format!("Looking up package '{}' in local indexes...", name));
        let pkg_index =
            index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));
        if let Some(found) = pkg_index.find(&name) {
            spec_path = found;
        } else {
            let host_arch = std::env::consts::ARCH;
            let mut binary_repos: Vec<_> = config
                .binary_repos
                .iter()
                .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
                .collect();
            binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

            for (repo_name, repo_cfg) in binary_repos {
                match db::repo::find_binary_repo_package(
                    repo_name,
                    repo_cfg,
                    options.rootfs,
                    &config.package_cache_dir,
                    &name,
                ) {
                    Ok(Some(rec)) => {
                        let archive = db::repo::fetch_binary_package_archive(
                            repo_name,
                            repo_cfg,
                            options.rootfs,
                            &rec,
                            &config.package_cache_dir,
                        )?;
                        ui::info(format!(
                            "Resolved '{}' from binary repo '{}' as {}-{} (package {}) ({} bytes){} -> {}",
                            name,
                            repo_name,
                            rec.version,
                            rec.revision,
                            rec.name,
                            rec.size,
                            rec.description
                                .as_ref()
                                .map(|d| format!(" [{}]", d))
                                .unwrap_or_default(),
                            archive.display()
                        ));
                        spec_path = archive;
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        crate::log_warn!("Binary repo '{}': {}", repo_name, e);
                    }
                }
            }
        }
    }

    let suppress_output = suppress_nested_install_output();
    if !suppress_output {
        ui::info(format!("Installing package from: {}", spec_path.display()));
    }

    let _snapper_pre_install_snapshot_todo: fn() -> ! =
        || todo!("snapper: create pre-install snapshot before install work starts");
    let _snapper_post_install_snapshot_todo: fn() -> ! =
        || todo!("snapper: create post-install snapshot after install commit succeeds");

    let prepared = prepare_direct_install_request(
        options,
        config,
        &spec_path,
        DirectInstallPreparationOptions {
            build_dir: &config.build_dir,
            clean_sources_before_build: true,
            suppress_output,
            confirm_installation: true,
            resolve_installed_conflicts: true,
        },
    )?;
    if options.dry_run {
        return Ok(false);
    }
    let _resources = prepared.resources;
    install_direct_transaction(&prepared.plans, options.rootfs, config)?;

    Ok(true)
}

pub(super) fn isolated_update_build_dir(config: &config::Config, idx: usize) -> PathBuf {
    config
        .build_dir
        .join("update-tx")
        .join(format!("{:04}", idx + 1))
}

pub(super) fn run_update_transaction_install_requests(
    options: DirectInstallOptions<'_>,
    config: &config::Config,
    requests: &[PathBuf],
) -> Result<bool> {
    if requests.is_empty() {
        return Ok(false);
    }

    let mut install_lock = locking::open_lock(config)?;
    let install_lock_path = locking::lock_path(config);
    let _install_lock_guard = locking::try_write(&mut install_lock, &install_lock_path, "update")?;

    if requests
        .iter()
        .any(|request| !is_archive_install_request(request))
    {
        clean_build_source_dirs(config)?;
    }

    let mut transaction_plans = Vec::new();
    let mut resources = Vec::with_capacity(requests.len());
    for (idx, request) in requests.iter().enumerate() {
        let build_dir = isolated_update_build_dir(config, idx);
        let prepared = prepare_direct_install_request(
            options,
            config,
            request,
            DirectInstallPreparationOptions {
                build_dir: &build_dir,
                clean_sources_before_build: false,
                suppress_output: true,
                confirm_installation: false,
                resolve_installed_conflicts: false,
            },
        )
        .with_context(|| {
            format!(
                "Failed to prepare update payload from {}",
                request.display()
            )
        })?;
        transaction_plans.extend(prepared.plans);
        resources.push(prepared.resources);
    }

    if options.dry_run {
        return Ok(false);
    }

    let conflict_subjects: Vec<_> = transaction_plans
        .iter()
        .flat_map(|plan| install_conflict_subjects_for_output_spec(&plan.spec))
        .collect();
    validate_no_transaction_conflicts(&conflict_subjects)?;
    let mut removal_packages = prompt_installed_conflict_removals_for_subjects(
        &conflict_subjects,
        options.rootfs,
        config,
        false,
    )?;
    for plan in &transaction_plans {
        removal_packages.extend(plan.staged.replacement_removals.iter().cloned());
    }
    let removals = planned_installed_removals(options.rootfs, config, removal_packages)?;

    install_update_transaction(&transaction_plans, &removals, options.rootfs, config)?;
    drop(resources);
    Ok(true)
}
