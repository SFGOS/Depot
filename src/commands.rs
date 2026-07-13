use crate::cli::{
    BuildArgs, Cli, Commands, ConfigArgs, ConvertArgs, InfoArgs, InstallArgs, InternalCommands,
    ListArgs, OwnsArgs, RemoveArgs, RepoCommands, RepoKindArg, SearchArgs, SetArgs, SignArgs,
    UpdateArgs,
};
use crate::{
    builder, cli_assets, config, cross, db, deps, index, install, locking, package, planner,
    signing, source, staging, ui,
};
use anyhow::{Context, Result};
use git2::Direction;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
    mpsc,
};
use std::time::Duration;
use url::Url;
use walkdir::WalkDir;

const MAX_PARALLEL_DOWNLOADS: usize = 8;

use build_cmd::support::{
    automatic_tests_disabled_for_outputs, build_lib32_companion_package, clean_build_source_dirs,
    clean_build_workspace, effective_lib32_only, ensure_requested_development_package_installed,
    make_lib32_package_spec, maybe_disable_tests_for_missing_deps,
    maybe_prompt_to_skip_tests_for_missing_requested_deps, merge_missing_dependencies,
    requested_outputs, should_install_test_deps,
};
use install_cmd::archive::{
    extract_package_archive_to_staging, load_package_archive_into_staging,
    load_package_spec_from_staging_or_repo_record,
};
use repo::groups::{binary_arch_from_filename, human_bytes};
use update::candidates::{collect_update_candidates, compare_completed_at};
use update::versions::compare_versions_for_updates;

#[cfg(test)]
use build_cmd::support::{build_type_runs_automatic_tests, make_lib32_build_spec};
#[cfg(test)]
use install_cmd::archive::{package_spec_from_repo_record, staging_temp_root};
#[cfg(test)]
use misc::internal::run_internal_command;
#[cfg(test)]
use repo::groups::{expand_install_requests_for_groups, expand_installed_group_targets};
#[cfg(test)]
use update::candidates::{
    SourceUpdateCandidate, UpdateCandidate, UpdateOrigin, collect_best_source_update_candidates,
    collect_missing_update_dependencies, select_update_candidate,
};
#[cfg(test)]
use update::versions::{
    ArchiveListingProbe, VersionPattern, archive_listing_probe, best_newer_version,
    candidate_versions_from_listing, candidate_versions_from_refs, extract_version_patterns,
    list_archive_versions, remote_git_repository_from_source_url,
};

fn rootfs_is_system_root(rootfs: &Path) -> bool {
    if rootfs == Path::new("/") {
        return true;
    }
    fs::canonicalize(rootfs)
        .map(|path| path == Path::new("/"))
        .unwrap_or(false)
}

fn command_requires_live_root(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Install(_) | Commands::Remove(_) | Commands::Update(_) | Commands::Set(_)
    )
}

fn repo_command_rootfs(command: &RepoCommands) -> &Path {
    match command {
        RepoCommands::Create { args, .. }
        | RepoCommands::Sync { args }
        | RepoCommands::Update { args, .. }
        | RepoCommands::Index { args, .. }
        | RepoCommands::List { args }
        | RepoCommands::Add { args, .. }
        | RepoCommands::Remove { args, .. }
        | RepoCommands::Enable { args, .. }
        | RepoCommands::Disable { args, .. }
        | RepoCommands::Owns { args, .. }
        | RepoCommands::Status { args } => &args.rootfs_args.rootfs,
    }
}

fn command_rootfs(command: &Commands) -> Option<&Path> {
    match command {
        Commands::Install(args) => Some(&args.rootfs_args.rootfs),
        Commands::Remove(args) => Some(&args.rootfs_args.rootfs),
        Commands::Build(args) => Some(&args.rootfs_args.rootfs),
        Commands::Update(args) => Some(&args.rootfs_args.rootfs),
        Commands::Info(args) => Some(&args.rootfs_args.rootfs),
        Commands::Search(args) => Some(&args.rootfs_args.rootfs),
        Commands::Owns(args) => Some(&args.rootfs_args.rootfs),
        Commands::List(args) => Some(&args.rootfs_args.rootfs),
        Commands::Sign(args) => Some(&args.rootfs_args.rootfs),
        Commands::Repo(args) => Some(repo_command_rootfs(&args.command)),
        Commands::Config(args) => Some(&args.rootfs_args.rootfs),
        Commands::Set(args) => Some(&args.rootfs_args.rootfs),
        Commands::Check(_)
        | Commands::Convert(_)
        | Commands::GenerateArtifacts(_)
        | Commands::MakeSpec(_)
        | Commands::Internal(_) => None,
    }
}

fn command_assume_yes(command: &Commands) -> bool {
    match command {
        Commands::Install(args) => args.prompt_args.yes,
        Commands::Remove(args) => args.prompt_args.yes,
        Commands::Build(args) => args.prompt_args.yes,
        Commands::Update(args) => args.prompt_args.yes,
        Commands::Check(_)
        | Commands::Convert(_)
        | Commands::Info(_)
        | Commands::Search(_)
        | Commands::Owns(_)
        | Commands::List(_)
        | Commands::Sign(_)
        | Commands::Repo(_)
        | Commands::Config(_)
        | Commands::Set(_)
        | Commands::GenerateArtifacts(_)
        | Commands::MakeSpec(_)
        | Commands::Internal(_) => false,
    }
}

fn should_reexec_with_sudo(cli: &Cli) -> bool {
    !crate::fakeroot::is_root()
        && command_rootfs(&cli.command).is_some_and(rootfs_is_system_root)
        && command_requires_live_root(&cli.command)
}

fn should_delegate_live_rootfs_installs(rootfs: &Path) -> bool {
    !crate::fakeroot::is_root() && rootfs_is_system_root(rootfs)
}

fn compare_package_release(
    left_version: &str,
    left_revision: u32,
    right_version: &str,
    right_revision: u32,
) -> Ordering {
    compare_versions_for_updates(left_version, right_version)
        .then_with(|| left_revision.cmp(&right_revision))
}

const DEPOT_INSTALL_CONTEXT_ENV: &str = "DEPOT_INSTALL_CONTEXT";
const INSTALL_CONTEXT_UPDATE: &str = "update";
const INSTALL_CONTEXT_PLANNED: &str = "planned";
const DEPOT_PACKAGE_NAME: &str = "depot";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallInvocationContext {
    Default,
    Update,
    Planned,
}

fn current_install_invocation_context() -> InstallInvocationContext {
    match std::env::var(DEPOT_INSTALL_CONTEXT_ENV).as_deref() {
        Ok(INSTALL_CONTEXT_UPDATE) => InstallInvocationContext::Update,
        Ok(INSTALL_CONTEXT_PLANNED) => InstallInvocationContext::Planned,
        _ => InstallInvocationContext::Default,
    }
}

fn suppress_nested_install_output() -> bool {
    matches!(
        current_install_invocation_context(),
        InstallInvocationContext::Update | InstallInvocationContext::Planned
    )
}

fn install_test_deps_enabled(cli_test_deps: bool, config: &config::Config) -> bool {
    cli_test_deps || config.install_test_deps
}

fn current_argv0() -> String {
    std::env::args_os()
        .next()
        .filter(|arg| !arg.is_empty())
        .map(|arg| PathBuf::from(arg).display().to_string())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| DEPOT_PACKAGE_NAME.to_string())
}

fn is_explicit_depot_self_update_request(packages: &[String]) -> bool {
    packages.len() == 1
        && packages
            .first()
            .is_some_and(|package| package == DEPOT_PACKAGE_NAME)
}

fn ensure_depot_self_update_not_required(config: &config::Config, rootfs: &Path) -> Result<()> {
    if current_install_invocation_context() == InstallInvocationContext::Update {
        return Ok(());
    }

    let db_path = config.installed_db_path(rootfs);
    if db::get_package_version(&db_path, DEPOT_PACKAGE_NAME)
        .with_context(|| {
            format!(
                "Failed to query installed package database at {}",
                db_path.display()
            )
        })?
        .is_none()
    {
        return Ok(());
    }

    let requested = [DEPOT_PACKAGE_NAME.to_string()];
    let updates = collect_update_candidates(config, rootfs, &requested)
        .context("Failed to check for pending depot self-update")?;
    if updates.is_empty() {
        return Ok(());
    }

    anyhow::bail!(
        "An update for '{}' is available. Run '{} update {}' before continuing.",
        DEPOT_PACKAGE_NAME,
        current_argv0(),
        DEPOT_PACKAGE_NAME
    );
}

fn maybe_reexec_with_sudo(cli: &Cli) -> Result<bool> {
    if !should_reexec_with_sudo(cli) {
        return Ok(false);
    }

    let exe = std::env::current_exe().context("Failed to locate depot executable")?;
    let mut cmd = std::process::Command::new("sudo");
    if let Some(preserve_arg) = sudo_preserve_env_arg() {
        cmd.arg(preserve_arg);
    }
    cmd.arg(exe);
    cmd.args(std::env::args_os().skip(1));

    let status = cmd
        .status()
        .context("Failed to re-execute depot via sudo for live-system install/remove")?;
    if status.success() {
        Ok(true)
    } else {
        anyhow::bail!("sudo depot command failed with status {}", status);
    }
}

fn sudo_preserve_env_arg() -> Option<String> {
    let mut keys = Vec::new();
    for key in [DEPOT_INSTALL_CONTEXT_ENV, "DEPOT_DEPCHAIN"] {
        if std::env::var_os(key).is_some() {
            keys.push(key);
        }
    }

    (!keys.is_empty()).then(|| format!("--preserve-env={}", keys.join(",")))
}

#[derive(Clone, Copy)]
struct ChildInstallCommandOptions<'a> {
    no_deps: bool,
    assume_yes: bool,
    no_flags: bool,
    cross_prefix: Option<&'a str>,
    clean: bool,
    lib32_only: bool,
    install_test_deps: bool,
    install_context: Option<&'a str>,
    dep_chain: Option<&'a str>,
}

fn install_request_display(install_requests: &[PathBuf]) -> String {
    install_requests
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn run_install_command_with_program(
    program: &Path,
    install_requests: &[PathBuf],
    rootfs: &Path,
    options: ChildInstallCommandOptions<'_>,
) -> Result<()> {
    if install_requests.is_empty() {
        return Ok(());
    }

    let mut cmd = std::process::Command::new(program);
    cmd.arg("install");
    cmd.arg("-r").arg(rootfs);
    if options.no_deps {
        cmd.arg("--no-deps");
    }
    if options.assume_yes {
        cmd.arg("--yes");
    }
    if options.no_flags {
        cmd.arg("--no-flags");
    }
    if let Some(p) = options.cross_prefix {
        cmd.arg("--cross-prefix").arg(p);
    }
    if options.clean {
        cmd.arg("--clean");
    }
    if options.lib32_only {
        cmd.arg("--lib32-only");
    }
    if options.install_test_deps {
        cmd.arg("--test-deps");
    }
    cmd.args(install_requests);
    if let Some(context) = options.install_context {
        cmd.env(DEPOT_INSTALL_CONTEXT_ENV, context);
    }
    if let Some(dep_chain) = options.dep_chain {
        cmd.env("DEPOT_DEPCHAIN", dep_chain);
    }

    let status = command_status_with_sh_fallback(&mut cmd).with_context(|| {
        format!(
            "Failed to spawn child install for {}",
            install_request_display(install_requests)
        )
    })?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "Child install failed for {} with status {}",
            install_request_display(install_requests),
            status
        );
    }
}

fn command_status_with_sh_fallback(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::ExitStatus> {
    match crate::interrupts::command_status(cmd) {
        Ok(status) => Ok(status),
        Err(err)
            if err.kind() == std::io::ErrorKind::PermissionDenied
                || err.raw_os_error() == Some(26) =>
        {
            let program = cmd.get_program();
            let contents = fs::read(program);
            let is_script = contents.ok().is_some_and(|bytes| bytes.starts_with(b"#!"));
            if !is_script {
                return Err(err);
            }

            let mut fallback = std::process::Command::new("sh");
            fallback.arg(program);
            fallback.args(cmd.get_args());
            if let Some(dir) = cmd.get_current_dir() {
                fallback.current_dir(dir);
            }
            for (key, value) in cmd.get_envs() {
                match value {
                    Some(value) => {
                        fallback.env(key, value);
                    }
                    None => {
                        fallback.env_remove(key);
                    }
                }
            }
            crate::interrupts::command_status(&mut fallback)
        }
        Err(err) => Err(err),
    }
}

fn run_child_install_command(
    install_requests: &[PathBuf],
    rootfs: &Path,
    options: InstallPlanExecutionOptions<'_>,
) -> Result<()> {
    let exe = std::env::current_exe().context("Failed to locate depot executable")?;
    run_install_command_with_program(
        &exe,
        install_requests,
        rootfs,
        ChildInstallCommandOptions {
            no_deps: true,
            assume_yes: true,
            no_flags: options.no_flags,
            cross_prefix: options.cross_prefix,
            clean: options.clean,
            lib32_only: false,
            install_test_deps: options.install_test_deps,
            install_context: Some(INSTALL_CONTEXT_PLANNED),
            dep_chain: None,
        },
    )
}

#[derive(Clone)]
struct InterruptWatcher;

impl InterruptWatcher {
    fn install() -> Result<Self> {
        crate::interrupts::install()?;
        crate::interrupts::reset();
        Ok(Self)
    }

    fn was_interrupted(&self) -> bool {
        crate::interrupts::was_interrupted()
    }

    fn check(&self) -> Result<()> {
        crate::interrupts::check()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AutoInstalledDependencyKind {
    Build,
    Runtime,
    Test,
}

#[derive(Debug, Default, Clone)]
struct AutoInstalledDependencyTracker {
    install_order: Vec<String>,
    build: BTreeSet<String>,
    runtime: BTreeSet<String>,
    test: BTreeSet<String>,
}

impl AutoInstalledDependencyTracker {
    fn record_plan(
        &mut self,
        plan: &planner::ExecutionPlan,
        requested_deps: &[String],
        kind: AutoInstalledDependencyKind,
    ) {
        if requested_deps.is_empty() {
            return;
        }

        for step in plan.actionable_steps() {
            if !self.install_order.contains(&step.package) {
                self.install_order.push(step.package.clone());
            }
        }

        let closure = plan_dependency_closure_for_requested_deps(plan, requested_deps);
        let target = match kind {
            AutoInstalledDependencyKind::Build => &mut self.build,
            AutoInstalledDependencyKind::Runtime => &mut self.runtime,
            AutoInstalledDependencyKind::Test => &mut self.test,
        };
        target.extend(closure);
    }

    fn cleanup_targets(&self, include_runtime: bool) -> Vec<String> {
        let mut remove = self.build.clone();
        remove.extend(self.test.iter().cloned());
        if include_runtime {
            remove.extend(self.runtime.iter().cloned());
        } else {
            remove.retain(|package| !self.runtime.contains(package));
        }

        self.install_order
            .iter()
            .rev()
            .filter(|package| remove.contains(*package))
            .cloned()
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.build.is_empty() && self.runtime.is_empty() && self.test.is_empty()
    }
}

fn plan_dependency_closure_for_requested_deps(
    plan: &planner::ExecutionPlan,
    requested_deps: &[String],
) -> HashSet<String> {
    let requested: HashSet<_> = requested_deps.iter().map(String::as_str).collect();
    let mut roots = Vec::new();
    let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();

    for step in plan.actionable_steps() {
        if step.requested_by.iter().any(|reason| {
            reason
                .strip_prefix("dependency ")
                .is_some_and(|dep| requested.contains(dep))
        }) {
            roots.push(step.package.clone());
        }

        for reason in &step.requested_by {
            if let Some((parent, _dep)) = reason.split_once(" needs ") {
                let children = children_by_parent.entry(parent.to_string()).or_default();
                if !children.contains(&step.package) {
                    children.push(step.package.clone());
                }
            }
        }
    }

    let mut stack = roots;
    let mut closure = HashSet::new();
    while let Some(package) = stack.pop() {
        if !closure.insert(package.clone()) {
            continue;
        }
        if let Some(children) = children_by_parent.get(&package) {
            stack.extend(children.iter().cloned());
        }
    }

    closure
}

fn prompt_for_dependency_cleanup(packages: &[String]) -> Result<bool> {
    let assume_yes = ui::assume_yes_enabled();
    ui::set_assume_yes(false);
    let result = ui::prompt_package_action("dependency cleanup", packages, true);
    ui::set_assume_yes(assume_yes);
    result
}

fn cleanup_auto_installed_dependencies(
    tracker: &AutoInstalledDependencyTracker,
    rootfs: &Path,
    config: &config::Config,
    include_runtime: bool,
    prompt: bool,
) -> Result<()> {
    let db_path = config.installed_db_path(rootfs);
    let installed = db::get_installed_packages(&db_path)?;
    let targets: Vec<String> = tracker
        .cleanup_targets(include_runtime)
        .into_iter()
        .filter(|package| installed.contains(package))
        .collect();

    if targets.is_empty() {
        return Ok(());
    }

    if prompt && !prompt_for_dependency_cleanup(&targets)? {
        return Ok(());
    }

    ui::info(format!(
        "Removing auto-installed dependencies: {}",
        targets.join(", ")
    ));
    for package in targets {
        remove_installed_package_with_hooks(&package, rootfs, config)?;
    }

    Ok(())
}

fn parse_licenses_from_toml(metadata: &toml::Value) -> Vec<String> {
    if let Some(s) = metadata.get("license").and_then(|v| v.as_str()) {
        return vec![s.to_string()];
    }
    if let Some(arr) = metadata.get("license").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }
    Vec::new()
}

fn parse_dependency_list(metadata: &toml::Value, kind: &str) -> Vec<String> {
    metadata
        .get("dependencies")
        .and_then(|v| v.get(kind))
        .and_then(|v| v.as_array())
        .map(|deps| {
            deps.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_metadata_string_list(metadata: &toml::Value, key: &str) -> Vec<String> {
    metadata
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_keep_list(metadata: &toml::Value) -> Vec<String> {
    if let Some(s) = metadata.get("keep").and_then(|v| v.as_str()) {
        return vec![s.to_string()];
    }
    if let Some(arr) = metadata.get("keep").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }
    Vec::new()
}

fn output_destdir_for(base_destdir: &Path, primary_pkg: &str, output_pkg: &str) -> PathBuf {
    if output_pkg == primary_pkg {
        base_destdir.to_path_buf()
    } else {
        staging::output_staging_dir(base_destdir, output_pkg)
    }
}

fn spec_for_output(
    pkg_spec: &package::PackageSpec,
    output: package::PackageInfo,
) -> package::PackageSpec {
    let output_name = output.name.clone();
    let mut spec_for_out = pkg_spec.clone();
    spec_for_out.package = output;
    spec_for_out.alternatives = pkg_spec.alternatives_for_output(&output_name);
    spec_for_out.dependencies = pkg_spec.dependencies_for_output(&output_name);
    spec_for_out
}

fn destdir_has_packagable_content(destdir: &Path) -> Result<bool> {
    if !destdir.exists() {
        return Ok(false);
    }

    let manifest = staging::generate_manifest_with_dirs(destdir)?;
    Ok(!manifest.files.is_empty() || !manifest.directories.is_empty())
}

fn staged_output_specs(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
) -> Result<Vec<(package::PackageSpec, PathBuf)>> {
    let declared_outputs = pkg_spec.outputs();
    let declared_names: HashSet<String> = declared_outputs
        .iter()
        .map(|output| output.name.clone())
        .collect();
    let mut outputs = Vec::new();
    let mut seen = HashSet::new();

    for output in declared_outputs {
        let output_name = output.name.clone();
        let out_destdir = output_destdir_for(destdir, &pkg_spec.package.name, &output_name);
        outputs.push((spec_for_output(pkg_spec, output.clone()), out_destdir));
        seen.insert(output_name.clone());

        if !pkg_spec.build.flags.split_docs || output_name.ends_with("-docs") {
            continue;
        }

        let docs_pkg = pkg_spec.docs_package_for_output(&output);
        if declared_names.contains(&docs_pkg.name) || seen.contains(&docs_pkg.name) {
            continue;
        }

        let docs_destdir = output_destdir_for(destdir, &pkg_spec.package.name, &docs_pkg.name);
        if !destdir_has_packagable_content(&docs_destdir)? {
            continue;
        }

        seen.insert(docs_pkg.name.clone());
        outputs.push((spec_for_output(pkg_spec, docs_pkg), docs_destdir));
    }

    Ok(outputs)
}

mod direct_install;
mod install_plan;
mod install_transaction;

use direct_install::*;
use install_plan::*;
use install_transaction::*;

mod build_cmd;
mod check;
mod install_cmd;
mod misc;
mod repo;
mod set;
mod update;

pub fn run(cli: Cli) -> Result<()> {
    crate::interrupts::install()?;
    crate::interrupts::reset();
    ui::set_assume_yes(command_assume_yes(&cli.command));
    if maybe_reexec_with_sudo(&cli)? {
        return Ok(());
    }

    let cli_test_deps = match &cli.command {
        Commands::Install(args) => args.build_exec_args.test_deps,
        Commands::Build(args) => args.build_exec_args.test_deps,
        Commands::Update(args) => args.build_exec_args.test_deps,
        Commands::Check(_)
        | Commands::Remove(_)
        | Commands::Info(_)
        | Commands::Search(_)
        | Commands::Owns(_)
        | Commands::List(_)
        | Commands::Sign(_)
        | Commands::Repo(_)
        | Commands::Config(_)
        | Commands::Set(_)
        | Commands::GenerateArtifacts(_)
        | Commands::Convert(_)
        | Commands::MakeSpec(_)
        | Commands::Internal(_) => false,
    };

    match cli.command {
        Commands::Install(args) => install_cmd::run_install(args, cli_test_deps)?,
        Commands::Remove(args) => install_cmd::run_remove(args)?,
        Commands::Build(args) => build_cmd::run_build(args, cli_test_deps)?,
        Commands::Update(args) => update::run_update(args, cli_test_deps)?,
        Commands::Check(args) => check::run_check(args)?,
        Commands::Info(args) => misc::run_info(args)?,
        Commands::Search(args) => repo::run_search(args)?,
        Commands::Owns(args) => misc::run_owns(args)?,
        Commands::List(args) => misc::run_list(args)?,
        Commands::Sign(args) => misc::run_sign(args)?,
        Commands::Repo(args) => repo::run_repo(args.command)?,
        Commands::GenerateArtifacts(args) => misc::run_generate_artifacts(args)?,
        Commands::Config(args) => misc::run_config(args)?,
        Commands::Set(args) => set::run_set(args)?,
        Commands::MakeSpec(args) => misc::run_make_spec(args)?,
        Commands::Convert(args) => misc::run_convert(args)?,
        Commands::Internal(args) => misc::run_internal(args)?,
    }

    Ok(())
}

#[cfg(test)]
mod tests;
