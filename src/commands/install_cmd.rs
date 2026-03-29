use super::*;
use crate::commands::repo::groups::{
    expand_install_requests_for_groups, expand_installed_group_targets,
};

pub(crate) mod archive;

pub(super) fn run_install(args: InstallArgs, cli_test_deps: bool) -> Result<()> {
    let InstallArgs {
        rootfs_args,
        prompt_args,
        build_exec_args,
        lib32_args,
        spec_or_archive,
        spec,
    } = args;
    let rootfs = rootfs_args.rootfs;
    let yes = prompt_args.yes;
    let no_deps = build_exec_args.no_deps;
    let no_flags = build_exec_args.no_flags;
    let cross_prefix = build_exec_args.cross_prefix;
    let clean = build_exec_args.clean;
    let dry_run = build_exec_args.dry_run;
    let lib32_only = lib32_args.lib32_only;
    let install_requests = match spec {
        Some(spec_path) => vec![spec_path],
        None => spec_or_archive,
    };

    let config = config::Config::for_rootfs(&rootfs);
    ensure_depot_self_update_not_required(&config, &rootfs)?;
    let (install_requests, explicit_groups) =
        expand_install_requests_for_groups(&config, &rootfs, &install_requests)?;
    let install_test_deps = install_test_deps_enabled(cli_test_deps, &config);
    let mut planned_targets = Vec::new();
    let mut planned_spec_paths = Vec::new();
    let mut direct_requests = Vec::new();

    if no_deps {
        direct_requests = install_requests;
    } else {
        for request in install_requests {
            if is_archive_install_request(&request) {
                direct_requests.push(request);
                continue;
            }
            if request.exists() {
                planned_spec_paths.push(request.clone());
                planned_targets.push(planner::InstallTarget::SpecPath(request));
            } else {
                planned_targets.push(planner::InstallTarget::PackageName(
                    request.to_string_lossy().to_string(),
                ));
            }
        }
    }

    let mut ran_plan_mode = false;
    if !planned_targets.is_empty() {
        ran_plan_mode = true;
        let planner_opts = planner::PlannerOptions {
            assume_yes: yes,
            prefer_binary: config.repo_settings.prefer_binary,
            local_sibling_root: shared_local_sibling_root(&planned_spec_paths),
            include_test_deps: install_test_deps,
            lib32_only_requested_specs: lib32_only,
        };
        let plan = if planned_targets.len() == 1 {
            planner::build_install_plan(&config, &rootfs, planned_targets[0].clone(), planner_opts)?
        } else {
            planner::build_install_plan_for_targets(
                &config,
                &rootfs,
                &planned_targets,
                planner_opts,
            )?
        };
        print_plan_summary(&plan);
        execute_install_plan_with_child_commands(
            &plan,
            &rootfs,
            &config,
            InstallPlanExecutionOptions {
                no_flags,
                cross_prefix: cross_prefix.as_deref(),
                clean,
                dry_run,
                confirm_installation: true,
                lib32_only_requested_specs: lib32_only,
                install_test_deps,
            },
        )?;
    }

    let mut ran_direct_install = false;
    let direct_install_options = DirectInstallOptions {
        rootfs: &rootfs,
        no_deps,
        no_flags,
        cross_prefix: cross_prefix.as_deref(),
        clean,
        dry_run,
        lib32_only,
        install_test_deps,
    };
    if direct_requests.len() > 1
        && direct_requests
            .iter()
            .all(|request| is_archive_install_request(request))
    {
        ran_direct_install |= run_direct_archive_install_requests(
            direct_install_options,
            &config,
            &direct_requests,
            true,
        )?;
    } else {
        for request in direct_requests {
            ran_direct_install |=
                run_direct_install_request(direct_install_options, &config, request)?;
        }
    }
    if ran_direct_install {
        install::scripts::run_deferred_hooks_if_possible(&rootfs)?;
    }

    if !dry_run && !explicit_groups.is_empty() {
        db::record_installed_groups(&config.installed_db_path(&rootfs), &explicit_groups)?;
    }

    if clean && (ran_plan_mode || ran_direct_install) {
        clean_build_workspace(&config)?;
    }

    Ok(())
}

pub(super) fn run_remove(args: RemoveArgs) -> Result<()> {
    let RemoveArgs {
        rootfs_args,
        package,
        ..
    } = args;
    let rootfs = rootfs_args.rootfs;
    let config = config::Config::for_rootfs(&rootfs);
    let mut remove_lock = locking::open_lock(&config)?;
    let remove_lock_path = locking::lock_path(&config);
    let _remove_lock_guard = locking::try_write(&mut remove_lock, &remove_lock_path, "remove")?;
    let db_path = config.installed_db_path(&rootfs);
    let (removal_targets, explicit_groups) =
        expand_installed_group_targets(&db_path, std::slice::from_ref(&package))?;
    if !ui::prompt_package_action("removal", &removal_targets, true)? {
        anyhow::bail!("Aborted");
    }
    for target in &removal_targets {
        remove_installed_package_with_hooks(target, &rootfs, &config)?;
    }
    for group in explicit_groups {
        db::remove_installed_group(&db_path, &group)?;
    }

    Ok(())
}
