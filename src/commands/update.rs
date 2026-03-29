use super::*;
use crate::commands::repo::groups::expand_installed_group_targets;

pub(crate) mod candidates;
pub(crate) mod check;
pub(crate) mod versions;

use self::candidates::{
    UpdateCommandOptions, run_update_command, sync_source_repositories_for_update,
};

pub(super) fn run_update(args: UpdateArgs, cli_test_deps: bool) -> Result<()> {
    let UpdateArgs {
        rootfs_args,
        prompt_args,
        build_exec_args,
        packages,
    } = args;
    let rootfs = rootfs_args.rootfs;
    let yes = prompt_args.yes;
    let no_deps = build_exec_args.no_deps;
    let no_flags = build_exec_args.no_flags;
    let cross_prefix = build_exec_args.cross_prefix;
    let clean = build_exec_args.clean;
    let dry_run = build_exec_args.dry_run;
    let config = config::Config::for_rootfs(&rootfs);
    sync_source_repositories_for_update(&config)?;
    let expanded_packages = if packages.is_empty() {
        Vec::new()
    } else {
        expand_installed_group_targets(&config.installed_db_path(&rootfs), &packages)?.0
    };
    if !is_explicit_depot_self_update_request(&expanded_packages) {
        ensure_depot_self_update_not_required(&config, &rootfs)?;
    }
    run_update_command(
        &expanded_packages,
        &config,
        UpdateCommandOptions {
            rootfs: &rootfs,
            no_deps,
            no_flags,
            cross_prefix: cross_prefix.as_deref(),
            clean,
            dry_run,
            assume_yes: yes,
            install_test_deps: install_test_deps_enabled(cli_test_deps, &config),
        },
    )
}
