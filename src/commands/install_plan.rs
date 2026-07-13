use super::*;

pub(super) fn print_plan_summary(plan: &planner::ExecutionPlan) {
    if std::env::var_os("DEPOT_VERBOSE_PLAN").is_none() {
        return;
    }
    for step in &plan.steps {
        let (action, origin) = match &step.action {
            planner::PlanAction::SkipInstalled => ("skip", "installed".to_string()),
            planner::PlanAction::BuildAndInstall => match &step.origin {
                planner::PlanOrigin::Source {
                    path,
                    local_sibling,
                } => (
                    "build+install",
                    if *local_sibling {
                        format!("source:local-sibling ({})", path.display())
                    } else {
                        format!("source ({})", path.display())
                    },
                ),
                _ => ("build+install", "source".to_string()),
            },
            planner::PlanAction::InstallBinary => match &step.origin {
                planner::PlanOrigin::Binary { repo_name, record } => (
                    "install",
                    format!(
                        "binary:{} {}-{} size={}",
                        repo_name,
                        record.version,
                        record.revision,
                        human_bytes(record.size)
                    ),
                ),
                _ => ("install", "binary".to_string()),
            },
        };
        ui::info(format!("  {} [{}] {}", step.package, action, origin));
    }
}

pub(super) fn actionable_plan_packages(plan: &planner::ExecutionPlan) -> Vec<String> {
    plan.actionable_steps()
        .map(|step| step.package.clone())
        .collect()
}

pub(super) fn source_build_reason(reason: &str) -> String {
    if let Some(dep) = reason.strip_prefix("dependency ") {
        format!("requested dependency '{dep}'")
    } else if let Some((requester, _)) = reason.split_once(" needs ") {
        format!("needed by '{requester}'")
    } else if reason == "requested spec" {
        "requested spec".to_string()
    } else if reason == "requested package" {
        "requested package".to_string()
    } else {
        reason.to_string()
    }
}

pub(super) fn source_build_warning_messages(plan: &planner::ExecutionPlan) -> Vec<String> {
    let mut lines = Vec::new();
    for step in plan.actionable_steps() {
        if !matches!(step.action, planner::PlanAction::BuildAndInstall) {
            continue;
        }

        let mut reasons = Vec::new();
        for reason in &step.requested_by {
            let label = source_build_reason(reason);
            if !reasons.contains(&label) {
                reasons.push(label);
            }
        }

        if reasons.is_empty() {
            lines.push(step.package.clone());
        } else {
            lines.push(format!("{} ({})", step.package, reasons.join(", ")));
        }
    }
    lines
}

pub(super) fn warn_source_build_plan(plan: &planner::ExecutionPlan) {
    let lines = source_build_warning_messages(plan);
    if lines.is_empty() {
        return;
    }

    ui::warn(format!(
        "{} package(s) will be built from source before installation.",
        lines.len()
    ));
    for line in lines {
        ui::warn(format!("  {line}"));
    }
}

pub(super) fn validate_source_build_prereqs_for_plan(
    plan: &planner::ExecutionPlan,
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    let db_path = config.installed_db_path(rootfs);
    let mut checked_development_package = false;

    for step in plan.actionable_steps() {
        let planner::PlanOrigin::Source { path, .. } = &step.origin else {
            continue;
        };
        if !matches!(step.action, planner::PlanAction::BuildAndInstall) {
            continue;
        }

        let mut spec = package::PackageSpec::from_file(path)
            .with_context(|| format!("Failed to parse spec {}", path.display()))?;
        spec.apply_config(config);
        source::preflight_local_manual_sources(&spec)?;
        if !checked_development_package && !spec.is_metapackage() {
            ensure_requested_development_package_installed(&db_path)?;
            checked_development_package = true;
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct InstallPlanExecutionOptions<'a> {
    pub(super) no_flags: bool,
    pub(super) cross_prefix: Option<&'a str>,
    pub(super) clean: bool,
    pub(super) dry_run: bool,
    pub(super) confirm_installation: bool,
    pub(super) lib32_only_requested_specs: bool,
    pub(super) install_test_deps: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChildInstallBatch {
    pub(super) requests: Vec<PathBuf>,
    pub(super) lib32_only: bool,
}

pub(super) fn step_requests_only_lib32(
    step: &planner::PlannedStep,
    options: &InstallPlanExecutionOptions<'_>,
) -> bool {
    options.lib32_only_requested_specs
        && step
            .requested_by
            .iter()
            .any(|reason| reason.starts_with("requested "))
}

pub(super) fn build_live_rootfs_child_install_batches(
    steps: &[&planner::PlannedStep],
    options: &InstallPlanExecutionOptions<'_>,
    binary_archives: &HashMap<(String, String), db::repo::BinaryRepoCachedArchive>,
) -> Result<Vec<ChildInstallBatch>> {
    let mut batches = Vec::new();
    let mut pending_binary_requests = Vec::new();

    for step in steps {
        match &step.origin {
            planner::PlanOrigin::Source { path, .. } => {
                if !pending_binary_requests.is_empty() {
                    batches.push(ChildInstallBatch {
                        requests: std::mem::take(&mut pending_binary_requests),
                        lib32_only: false,
                    });
                }
                batches.push(ChildInstallBatch {
                    requests: vec![path.clone()],
                    lib32_only: step_requests_only_lib32(step, options),
                });
            }
            planner::PlanOrigin::Binary { repo_name, record } => {
                let cached = binary_archives
                    .get(&(repo_name.clone(), record.filename.clone()))
                    .with_context(|| {
                        format!(
                            "Cached archive missing for planned binary step '{}' from repo '{}'",
                            record.filename, repo_name
                        )
                    })?;
                pending_binary_requests.push(cached.package_path.clone());
            }
            planner::PlanOrigin::Installed => {}
        }
    }

    if !pending_binary_requests.is_empty() {
        batches.push(ChildInstallBatch {
            requests: pending_binary_requests,
            lib32_only: false,
        });
    }

    Ok(batches)
}

pub(super) fn flush_binary_install_batch(
    pending_plans: &mut Vec<PlannedPackageInstall>,
    pending_staging_dirs: &mut Vec<tempfile::TempDir>,
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    if pending_plans.is_empty() {
        return Ok(());
    }

    install_planned_packages_to_rootfs(pending_plans, rootfs, config)?;
    pending_plans.clear();
    pending_staging_dirs.clear();
    Ok(())
}

pub(super) fn execute_install_plan_with_child_commands(
    plan: &planner::ExecutionPlan,
    rootfs: &Path,
    config: &config::Config,
    options: InstallPlanExecutionOptions<'_>,
) -> Result<()> {
    #[derive(Clone)]
    struct BinaryPhaseItem {
        repo_name: String,
        record: db::repo::BinaryRepoPackageRecord,
    }

    let actionable_steps: Vec<_> = plan.actionable_steps().collect();
    if actionable_steps.is_empty() {
        ui::info("Nothing to do.");
        return Ok(());
    }

    validate_source_build_prereqs_for_plan(plan, rootfs, config)?;
    warn_source_build_plan(plan);
    let planned_packages = actionable_plan_packages(plan);
    if options.confirm_installation
        && !ui::prompt_package_action("installation", &planned_packages, true)?
    {
        anyhow::bail!("Aborted");
    }

    let mut conflict_subjects = Vec::new();
    for step in &actionable_steps {
        match &step.origin {
            planner::PlanOrigin::Source { path, .. } => {
                let mut spec = package::PackageSpec::from_file(path)
                    .with_context(|| format!("Failed to parse spec {}", path.display()))?;
                spec.apply_config(config);
                let lib32_only =
                    effective_lib32_only(&spec, step_requests_only_lib32(step, &options));
                conflict_subjects.extend(install_conflict_subjects_for_spec(
                    &spec,
                    !lib32_only,
                    spec.builds_lib32_output() || lib32_only,
                ));
            }
            planner::PlanOrigin::Binary { record, .. } => {
                conflict_subjects.push(install_conflict_subject_for_binary_record(record));
            }
            planner::PlanOrigin::Installed => {}
        }
    }
    resolve_installed_conflicts_for_subjects(&conflict_subjects, rootfs, config, options.dry_run)?;

    if options.dry_run {
        ui::info("Dry run enabled, no install/build actions executed.");
        return Ok(());
    }

    let mut binary_archives: HashMap<(String, String), db::repo::BinaryRepoCachedArchive> =
        HashMap::new();
    let mut binary_phase_items = Vec::new();
    let mut seen_binary_archives = HashSet::new();
    for step in &actionable_steps {
        if let planner::PlanOrigin::Binary { repo_name, record } = &step.origin
            && seen_binary_archives.insert((repo_name.clone(), record.filename.clone()))
        {
            binary_phase_items.push(BinaryPhaseItem {
                repo_name: repo_name.clone(),
                record: (**record).clone(),
            });
        }
    }

    if !binary_phase_items.is_empty() {
        ui::info(format!(
            "Downloading {} binary package(s) and detached signatures...",
            binary_phase_items.len()
        ));
        let use_tty_progress = std::io::stderr().is_terminal();
        let download_progress = MultiProgress::with_draw_target(if use_tty_progress {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        });
        let download_bars = binary_phase_items
            .iter()
            .map(|item| {
                let label = format!(
                    "{}-{}-{}",
                    item.record.name,
                    item.record.version,
                    binary_arch_from_filename(&item.record.filename)
                );
                let pb = download_progress.add(ProgressBar::new(item.record.size.max(1)));
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template("{prefix:.bold} [{bar:40.cyan/blue}] {eta}")
                        .unwrap_or_else(|_| ProgressStyle::default_bar())
                        .progress_chars("#>-"),
                );
                pb.set_prefix(label);
                pb
            })
            .collect::<Vec<_>>();
        let download_client = db::repo::binary_package_http_client()?;
        let download_results = run_parallel_tasks(
            &binary_phase_items,
            MAX_PARALLEL_DOWNLOADS,
            |index, item| {
                let pb = &download_bars[index];
                let mut progress_cb = |downloaded: u64, total: Option<u64>| {
                    if let Some(t) = total
                        && t > 0
                    {
                        pb.set_length(t);
                    }
                    pb.set_position(downloaded);
                };
                let result = (|| {
                    let repo_cfg = config.binary_repos.get(&item.repo_name).with_context(|| {
                        format!("Binary repo '{}' not found in config", item.repo_name)
                    })?;
                    db::repo::cache_binary_package_archive_with_client_and_progress(
                        &item.repo_name,
                        repo_cfg,
                        &item.record,
                        &config.package_cache_dir,
                        &download_client,
                        Some(&mut progress_cb),
                    )
                    .with_context(|| {
                        format!(
                            "Failed to cache binary package '{}' from repo '{}'",
                            item.record.filename, item.repo_name
                        )
                    })
                })();
                pb.finish_and_clear();
                result
            },
        );
        download_progress
            .clear()
            .context("Failed to clear binary download progress")?;
        for (item, cached) in binary_phase_items.iter().zip(download_results?) {
            binary_archives.insert(
                (item.repo_name.clone(), item.record.filename.clone()),
                cached,
            );
        }

        ui::info(format!(
            "Verifying checksums and detached signatures for {} binary package(s)...",
            binary_phase_items.len()
        ));
        let integrity_pb = ProgressBar::new(binary_phase_items.len() as u64);
        integrity_pb.set_draw_target(if use_tty_progress {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        });
        integrity_pb.set_style(
            ProgressStyle::default_bar()
                .template("{prefix:.bold} [{bar:40.cyan/blue}] {pos}/{len} {eta}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        integrity_pb.set_prefix("integrity");
        let has_detached_signatures = binary_phase_items.iter().any(|item| {
            binary_archives
                .get(&(item.repo_name.clone(), item.record.filename.clone()))
                .is_some_and(|cached| cached.signature_path.exists())
        });
        let trusted_keys = if has_detached_signatures {
            signing::load_trusted_public_keys(rootfs)
                .context("Failed to load trusted Minisign public keys")?
        } else {
            Vec::new()
        };
        run_parallel_verification(&binary_phase_items, &integrity_pb, |item| {
            let repo_cfg = config
                .binary_repos
                .get(&item.repo_name)
                .with_context(|| format!("Binary repo '{}' not found in config", item.repo_name))?;
            let cached = binary_archives
                .get(&(item.repo_name.clone(), item.record.filename.clone()))
                .with_context(|| {
                    format!(
                        "Cached archive missing for {} from repo '{}'",
                        item.record.filename, item.repo_name
                    )
                })?;
            db::repo::verify_binary_package_archive_integrity_with_trusted_keys(
                &item.repo_name,
                repo_cfg,
                &item.record,
                &cached.package_path,
                &cached.signature_path,
                &trusted_keys,
            )
            .with_context(|| {
                format!(
                    "Integrity verification failed for {} from repo '{}'",
                    item.record.filename, item.repo_name
                )
            })
        })?;
        integrity_pb.finish_and_clear();
    }

    if should_delegate_live_rootfs_installs(rootfs) {
        let exe = std::env::current_exe().context("Failed to locate depot executable")?;
        let batches =
            build_live_rootfs_child_install_batches(&actionable_steps, &options, &binary_archives)?;
        for batch in batches {
            run_install_command_with_program(
                &exe,
                &batch.requests,
                rootfs,
                ChildInstallCommandOptions {
                    no_deps: true,
                    assume_yes: true,
                    no_flags: options.no_flags,
                    cross_prefix: options.cross_prefix,
                    clean: options.clean,
                    lib32_only: batch.lib32_only,
                    install_test_deps: options.install_test_deps,
                    install_context: Some(INSTALL_CONTEXT_PLANNED),
                    dep_chain: None,
                },
            )?;
        }
        return Ok(());
    }

    let mut binary_pre_hook_plans = Vec::new();
    for step in &actionable_steps {
        if let planner::PlanOrigin::Binary { repo_name, record } = &step.origin {
            let cached = binary_archives
                .get(&(repo_name.clone(), record.filename.clone()))
                .with_context(|| {
                    format!(
                        "Cached archive missing for planned binary step '{}' from repo '{}'",
                        record.filename, repo_name
                    )
                })?;
            let staged = extract_package_archive_to_staging(config, &cached.package_path)?;
            let spec = load_package_spec_from_staging_or_repo_record(staged.path(), record)?;
            let plans = plan_package_outputs_for_install(&spec, staged.path(), rootfs, config)?;
            binary_pre_hook_plans.extend(plans);
        }
    }
    run_transaction_hooks_for_plans(
        rootfs,
        install::hooks::HookPhase::Pre,
        &binary_pre_hook_plans,
    )?;

    let exe = std::env::current_exe().context("Failed to locate depot executable")?;
    let total_steps = actionable_steps.len();
    let mut binary_post_hook_plans = Vec::new();
    let mut pending_binary_install_plans = Vec::new();
    let mut pending_binary_install_staging_dirs = Vec::new();
    for (idx, step) in actionable_steps.into_iter().enumerate() {
        match &step.origin {
            planner::PlanOrigin::Source { path, .. } => {
                flush_binary_install_batch(
                    &mut pending_binary_install_plans,
                    &mut pending_binary_install_staging_dirs,
                    rootfs,
                    config,
                )?;
                ui::info(format!(
                    "[{}/{}] building+installing {} from source",
                    idx + 1,
                    total_steps,
                    step.package
                ));

                run_install_command_with_program(
                    &exe,
                    std::slice::from_ref(path),
                    rootfs,
                    ChildInstallCommandOptions {
                        no_deps: true,
                        assume_yes: true,
                        no_flags: options.no_flags,
                        cross_prefix: options.cross_prefix,
                        clean: options.clean,
                        lib32_only: step_requests_only_lib32(step, &options),
                        install_test_deps: options.install_test_deps,
                        install_context: Some(INSTALL_CONTEXT_PLANNED),
                        dep_chain: None,
                    },
                )
                .with_context(|| {
                    format!("Failed to spawn planned install step '{}'", step.package)
                })?;
            }
            planner::PlanOrigin::Binary { repo_name, record } => {
                let cached = binary_archives
                    .get(&(repo_name.clone(), record.filename.clone()))
                    .with_context(|| {
                        format!(
                            "Cached archive missing for planned binary step '{}' from repo '{}'",
                            record.filename, repo_name
                        )
                    })?;
                let staged = extract_package_archive_to_staging(config, &cached.package_path)?;
                let spec = load_package_spec_from_staging_or_repo_record(staged.path(), record)?;
                let plans = plan_package_outputs_for_install(&spec, staged.path(), rootfs, config)?;
                binary_post_hook_plans.extend(plans.iter().cloned());
                pending_binary_install_plans.extend(plans);
                pending_binary_install_staging_dirs.push(staged);
            }
            planner::PlanOrigin::Installed => {}
        }
    }

    flush_binary_install_batch(
        &mut pending_binary_install_plans,
        &mut pending_binary_install_staging_dirs,
        rootfs,
        config,
    )?;
    run_transaction_hooks_for_plans(
        rootfs,
        install::hooks::HookPhase::Post,
        &binary_post_hook_plans,
    )?;
    install::scripts::run_deferred_hooks_if_possible(rootfs)?;
    Ok(())
}
