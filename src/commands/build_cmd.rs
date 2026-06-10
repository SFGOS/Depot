use super::*;

pub(crate) mod support;

use self::support::{
    RequestedBuildToolPackageInstall, automatic_tests_disabled_for_outputs,
    build_lib32_companion_package, clean_build_source_dirs, clean_build_workspace,
    effective_lib32_only, ensure_requested_build_tool_package_installed,
    ensure_requested_development_package_installed, maybe_disable_tests_for_missing_deps,
    maybe_prompt_to_skip_tests_for_missing_requested_deps, merge_missing_dependencies,
    requested_outputs, should_install_test_deps, warn_if_running_as_root_for_build,
};

pub(crate) fn build_env_rootfs(rootfs: &Path) -> String {
    if rootfs == Path::new("/") {
        return "/".to_string();
    }
    rootfs
        .canonicalize()
        .unwrap_or_else(|_| rootfs.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

pub(super) fn run_build(args: BuildArgs, cli_test_deps: bool) -> Result<()> {
    let BuildArgs {
        rootfs_args,
        prompt_args,
        build_exec_args,
        lib32_args,
        spec_pos,
        spec,
        install,
        install_deps,
        cleanup_deps,
    } = args;
    let rootfs = rootfs_args.rootfs;
    let yes = prompt_args.yes;
    let no_deps = build_exec_args.no_deps;
    let no_flags = build_exec_args.no_flags;
    let cross_prefix = build_exec_args.cross_prefix;
    let clean = build_exec_args.clean;
    let dry_run = build_exec_args.dry_run;
    let cli_lib32_only = lib32_args.lib32_only;
    warn_if_running_as_root_for_build("build", &rootfs);
    let config = config::Config::for_rootfs(&rootfs);
    ensure_depot_self_update_not_required(&config, &rootfs)?;
    let spec_path = spec.or(spec_pos).context("No spec file provided")?;
    ui::info(format!("Building package from: {}", spec_path.display()));
    let mut pkg_spec = package::PackageSpec::from_file(&spec_path)?;
    let install_test_deps = install_test_deps_enabled(cli_test_deps, &config);
    let interrupt_watcher = if cleanup_deps {
        Some(InterruptWatcher::install()?)
    } else {
        None
    };
    let mut auto_installed_deps = AutoInstalledDependencyTracker::default();
    let build_result: Result<()> = (|| {
        pkg_spec.apply_config(&config);
        pkg_spec.build.flags.rootfs = build_env_rootfs(&rootfs);
        let lib32_only = effective_lib32_only(&pkg_spec, cli_lib32_only);
        let requested_outputs = requested_outputs(&pkg_spec, lib32_only);
        let db_path = config.installed_db_path(&rootfs);

        source::preflight_local_manual_sources(&pkg_spec)?;
        if !pkg_spec.is_metapackage() {
            ensure_requested_development_package_installed(&db_path)?;
        }

        let build_targets = vec![format!(
            "{} v{}-{}",
            pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
        )];
        if !ui::prompt_package_action("build", &build_targets, true)? {
            anyhow::bail!("Aborted");
        }

        std::fs::create_dir_all(&config.db_dir).with_context(|| {
            format!(
                "Failed to create database directory: {}",
                config.db_dir.display()
            )
        })?;

        if no_deps && should_install_test_deps(&pkg_spec, install_test_deps, requested_outputs) {
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
        } else if no_deps
            || !should_install_test_deps(&pkg_spec, install_test_deps, requested_outputs)
        {
            maybe_disable_tests_for_missing_deps(&mut pkg_spec, &db_path, requested_outputs)?;
        }

        if !no_deps {
            let missing_build =
                deps::check_build_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            let missing_runtime =
                deps::check_runtime_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            let missing_required =
                merge_missing_dependencies(missing_build.clone(), missing_runtime.clone());
            if !missing_required.is_empty() {
                ui::warn(format!(
                    "Missing dependencies: {}",
                    missing_required.join(", ")
                ));
                if !install_deps {
                    if dry_run {
                        ui::info("Dry run enabled, stopping before dependency installation/build.");
                        return Ok(());
                    }
                    anyhow::bail!(
                        "Missing dependencies: {}. Re-run with --install-deps to install them automatically, or install them manually.",
                        missing_required.join(", ")
                    );
                }

                let local_sibling_root = spec_path
                    .parent()
                    .and_then(|p| p.parent())
                    .map(Path::to_path_buf);
                let dep_plan = planner::build_dependency_install_plan(
                    &config,
                    &rootfs,
                    &missing_required,
                    planner::PlannerOptions {
                        assume_yes: yes,
                        prefer_binary: config.repo_settings.prefer_binary,
                        local_sibling_root,
                        include_test_deps: install_test_deps,
                        lib32_only_requested_specs: false,
                    },
                )?;
                if cleanup_deps {
                    auto_installed_deps.record_plan(
                        &dep_plan,
                        &missing_build,
                        AutoInstalledDependencyKind::Build,
                    );
                    auto_installed_deps.record_plan(
                        &dep_plan,
                        &missing_runtime,
                        AutoInstalledDependencyKind::Runtime,
                    );
                }
                print_plan_summary(&dep_plan);
                if dry_run {
                    ui::info("Dry run enabled, stopping before dependency installation/build.");
                    return Ok(());
                }
                execute_install_plan_with_child_commands(
                    &dep_plan,
                    &rootfs,
                    &config,
                    InstallPlanExecutionOptions {
                        no_flags,
                        cross_prefix: cross_prefix.as_deref(),
                        clean,
                        dry_run,
                        confirm_installation: false,
                        lib32_only_requested_specs: false,
                        install_test_deps,
                    },
                )?;
            }
            deps::require_build_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            deps::require_runtime_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
            if should_install_test_deps(&pkg_spec, install_test_deps, requested_outputs) {
                let missing_test =
                    deps::check_test_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?;
                if !missing_test.is_empty() {
                    let local_sibling_root = spec_path
                        .parent()
                        .and_then(|p| p.parent())
                        .map(Path::to_path_buf);
                    let dep_plan = match planner::build_dependency_install_plan(
                        &config,
                        &rootfs,
                        &missing_test,
                        planner::PlannerOptions {
                            assume_yes: yes,
                            prefer_binary: config.repo_settings.prefer_binary,
                            local_sibling_root,
                            include_test_deps: install_test_deps,
                            lib32_only_requested_specs: false,
                        },
                    ) {
                        Ok(plan) => plan,
                        Err(_err)
                            if maybe_prompt_to_skip_tests_for_missing_requested_deps(
                                &mut pkg_spec,
                                &missing_test,
                                "Requested test dependencies could not be resolved",
                            )? =>
                        {
                            planner::ExecutionPlan { steps: Vec::new() }
                        }
                        Err(err) => return Err(err),
                    };

                    if cleanup_deps
                        && !automatic_tests_disabled_for_outputs(&pkg_spec, requested_outputs)
                    {
                        auto_installed_deps.record_plan(
                            &dep_plan,
                            &missing_test,
                            AutoInstalledDependencyKind::Test,
                        );
                    }

                    if !automatic_tests_disabled_for_outputs(&pkg_spec, requested_outputs)
                        && !dep_plan.steps.is_empty()
                    {
                        ui::warn(format!(
                            "Missing test dependencies: {}",
                            missing_test.join(", ")
                        ));
                        print_plan_summary(&dep_plan);
                        if !install_deps {
                            if dry_run {
                                ui::info(
                                    "Dry run enabled, stopping before dependency installation/build.",
                                );
                                return Ok(());
                            }
                            if !maybe_prompt_to_skip_tests_for_missing_requested_deps(
                                &mut pkg_spec,
                                &missing_test,
                                "Requested test dependencies were not installed",
                            )? {
                                anyhow::bail!(
                                    "Missing test dependencies: {}. Re-run with --install-deps to install them automatically, or install them manually.",
                                    missing_test.join(", ")
                                );
                            }
                        } else if !dry_run {
                            execute_install_plan_with_child_commands(
                                &dep_plan,
                                &rootfs,
                                &config,
                                InstallPlanExecutionOptions {
                                    no_flags,
                                    cross_prefix: cross_prefix.as_deref(),
                                    clean,
                                    dry_run,
                                    confirm_installation: false,
                                    lib32_only_requested_specs: false,
                                    install_test_deps,
                                },
                            )?;
                        }
                    }

                    if should_install_test_deps(&pkg_spec, install_test_deps, requested_outputs) {
                        let missing_test = deps::check_test_deps_for_outputs(
                            &pkg_spec,
                            &db_path,
                            requested_outputs,
                        )?;
                        if !missing_test.is_empty()
                            && !maybe_prompt_to_skip_tests_for_missing_requested_deps(
                                &mut pkg_spec,
                                &missing_test,
                                "Requested test dependencies are still missing",
                            )?
                        {
                            deps::require_test_deps_for_outputs(
                                &pkg_spec,
                                &db_path,
                                requested_outputs,
                            )?;
                        }
                    }
                }
            }
        }

        ensure_requested_build_tool_package_installed(RequestedBuildToolPackageInstall {
            build_type: pkg_spec.build.build_type,
            rootfs: &rootfs,
            config: &config,
            db_path: &db_path,
            spec_path: &spec_path,
            execution: InstallPlanExecutionOptions {
                no_flags,
                cross_prefix: cross_prefix.as_deref(),
                clean,
                dry_run,
                confirm_installation: false,
                lib32_only_requested_specs: false,
                install_test_deps,
            },
            assume_yes: yes,
        })?;

        if dry_run {
            ui::info("Dry run enabled, stopping before fetch/build.");
            return Ok(());
        }

        let mut build_lock = locking::open_lock(&config)?;
        let build_lock_path = locking::lock_path(&config);
        let _build_lock_guard = locking::try_write(&mut build_lock, &build_lock_path, "build")?;

        if let Some(watcher) = interrupt_watcher.as_ref() {
            watcher.check()?;
        }

        clean_build_source_dirs(&config)?;
        source::preflight_manual_sources(&pkg_spec, &config.cache_dir)?;
        let src_dir = source::prepare(&pkg_spec, &config.cache_dir, &config.build_dir)?;
        if let Some(watcher) = interrupt_watcher.as_ref() {
            watcher.check()?;
        }

        let destdir = config
            .build_dir
            .join("destdir")
            .join(&pkg_spec.package.name);
        let cross_config = cross_prefix
            .as_ref()
            .map(|p| cross::CrossConfig::from_prefix(p))
            .transpose()?;
        let host_build_dir = builder::ensure_host_build(
            &pkg_spec,
            &src_dir,
            cross_config.as_ref(),
            !no_flags,
            builder::TargetBuildKind::Primary,
        )?;
        if let Some(host_dir) = host_build_dir.as_ref() {
            pkg_spec.build.flags.host_build_dir = Some(host_dir.to_string_lossy().into_owned());
        }
        if !lib32_only {
            builder::build(
                &pkg_spec,
                &src_dir,
                &destdir,
                cross_config.as_ref(),
                !no_flags,
                host_build_dir.as_deref(),
            )?;
            if let Some(watcher) = interrupt_watcher.as_ref() {
                watcher.check()?;
            }
        }

        if !lib32_only {
            staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;
            install::scripts::stage_scripts_from_spec_dir(&pkg_spec, &destdir)?;
            staging::process(&destdir, &pkg_spec)?;
            staging::stage_split_package_licenses(&src_dir, &destdir, &pkg_spec)?;
            if let Some(watcher) = interrupt_watcher.as_ref() {
                watcher.check()?;
            }
        }

        let arch = cross_prefix.as_deref().unwrap_or(std::env::consts::ARCH);

        let mut created_files = Vec::new();
        let staged_outputs = if !lib32_only {
            staged_output_specs(&pkg_spec, &destdir)?
        } else {
            Vec::new()
        };
        if !lib32_only {
            for (spec_for_out, out_destdir) in &staged_outputs {
                let packager = package::Packager::new(
                    spec_for_out.clone(),
                    out_destdir.clone(),
                    config.clone(),
                );
                let pkg_file = packager.create_package(Path::new("."), arch)?;
                created_files.push(pkg_file);
                if let Some(watcher) = interrupt_watcher.as_ref() {
                    watcher.check()?;
                }
            }
        }

        let mut lib32_install_bundle: Option<(package::PackageSpec, PathBuf)> = None;
        if let Some((lib32_spec, lib32_destdir)) = build_lib32_companion_package(
            &pkg_spec,
            &src_dir,
            &config,
            cross_config.as_ref(),
            !no_flags,
            lib32_only,
        )? {
            let packager =
                package::Packager::new(lib32_spec.clone(), lib32_destdir.clone(), config.clone());
            let pkg_file = packager.create_package(Path::new("."), arch)?;
            created_files.push(pkg_file);
            lib32_install_bundle = Some((lib32_spec, lib32_destdir));
            if let Some(watcher) = interrupt_watcher.as_ref() {
                watcher.check()?;
            }
        }

        for f in &created_files {
            ui::success(format!("Build complete. Package created: {}", f.display()));
        }

        for sig_path in signing::auto_sign_zst_files_detached(&rootfs, &created_files)? {
            ui::success(format!(
                "Created detached signature: {}",
                sig_path.display()
            ));
        }

        if install {
            let mut install_targets = Vec::new();
            if !lib32_only {
                for (spec_for_out, _) in &staged_outputs {
                    let out = &spec_for_out.package;
                    install_targets.push(format!("{} v{}-{}", out.name, out.version, out.revision));
                }
            }
            if let Some((lib32_spec, _)) = &lib32_install_bundle {
                install_targets.push(format!(
                    "{} v{}-{}",
                    lib32_spec.package.name,
                    lib32_spec.package.version,
                    lib32_spec.package.revision
                ));
            }
            if ui::prompt_package_action("installation", &install_targets, false)? {
                if let Some(watcher) = interrupt_watcher.as_ref() {
                    watcher.check()?;
                }
                if should_delegate_live_rootfs_installs(&rootfs) {
                    run_child_install_command(
                        &created_files,
                        &rootfs,
                        InstallPlanExecutionOptions {
                            no_flags,
                            cross_prefix: cross_prefix.as_deref(),
                            clean,
                            dry_run,
                            confirm_installation: false,
                            lib32_only_requested_specs: false,
                            install_test_deps,
                        },
                    )?;
                    return Ok(());
                }

                let mut transaction_plans = Vec::new();
                if !lib32_only {
                    let output_plans =
                        plan_package_outputs_for_install(&pkg_spec, &destdir, &rootfs, &config)?;
                    transaction_plans.extend(output_plans);
                }
                if let Some((lib32_spec, lib32_destdir)) = &lib32_install_bundle {
                    let staged = plan_staged_install(lib32_spec, lib32_destdir, &rootfs, &config)?;
                    transaction_plans.push(PlannedPackageInstall {
                        spec: lib32_spec.clone(),
                        destdir: lib32_destdir.clone(),
                        staged,
                    });
                }

                run_transaction_hooks_for_plans(
                    &rootfs,
                    install::hooks::HookPhase::Pre,
                    &transaction_plans,
                )?;
                install_planned_packages_to_rootfs(&transaction_plans, &rootfs, &config)?;
                run_transaction_hooks_for_plans(
                    &rootfs,
                    install::hooks::HookPhase::Post,
                    &transaction_plans,
                )?;

                install::scripts::run_deferred_hooks_if_possible(&rootfs)?;
            } else {
                if !lib32_only {
                    for (spec_for_out, _) in &staged_outputs {
                        let out = &spec_for_out.package;
                        ui::success(format!(
                            "Built successfully: {}-{}-{}",
                            out.name, out.version, out.revision
                        ));
                    }
                }
                if let Some((lib32_spec, _)) = &lib32_install_bundle {
                    ui::success(format!(
                        "Built successfully: {}-{}-{}",
                        lib32_spec.package.name,
                        lib32_spec.package.version,
                        lib32_spec.package.revision
                    ));
                }
            }
        }

        Ok(())
    })();

    let interrupted = interrupt_watcher
        .as_ref()
        .is_some_and(InterruptWatcher::was_interrupted);
    match build_result {
        Ok(()) => {
            if cleanup_deps && !auto_installed_deps.is_empty() {
                cleanup_auto_installed_dependencies(
                    &auto_installed_deps,
                    &rootfs,
                    &config,
                    !install,
                    false,
                )?;
            }
            clean_build_source_dirs(&config)?;
            if clean {
                clean_build_workspace(&config)?;
            }
        }
        Err(err) => {
            if cleanup_deps && !auto_installed_deps.is_empty() {
                if interrupted {
                    ui::warn("Build interrupted by Ctrl-C.");
                }
                if let Err(clean_err) = cleanup_auto_installed_dependencies(
                    &auto_installed_deps,
                    &rootfs,
                    &config,
                    !install,
                    interrupted,
                ) {
                    ui::warn(format!(
                        "Failed to remove auto-installed dependencies: {}",
                        clean_err
                    ));
                }
            }
            if let Err(clean_err) = clean_build_source_dirs(&config) {
                ui::warn(format!(
                    "Failed to clean build source dirs after failed build: {}",
                    clean_err
                ));
            }
            if interrupted {
                anyhow::bail!("Build interrupted by Ctrl-C");
            }
            return Err(err);
        }
    }

    Ok(())
}
