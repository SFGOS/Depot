use crate::cli::{
    BuildArgs, Cli, Commands, ConfigArgs, ConvertArgs, InfoArgs, InstallArgs, InternalCommands,
    ListArgs, OwnsArgs, RemoveArgs, RepoCommands, RepoKindArg, SearchArgs, SignArgs, UpdateArgs,
};
use crate::{
    builder, cli_assets, config, cross, db, deps, index, install, locking, package, planner,
    signing, source, staging, ui,
};
use anyhow::{Context, Result};
use git2::Direction;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;
use walkdir::WalkDir;

use build_cmd::support::{
    automatic_tests_disabled_for_outputs, build_lib32_companion_package, clean_build_workspace,
    effective_lib32_only, ensure_requested_development_package_installed, make_lib32_package_spec,
    maybe_disable_tests_for_missing_deps, maybe_prompt_to_skip_tests_for_missing_requested_deps,
    merge_missing_dependencies, requested_outputs, should_install_test_deps,
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
        Commands::Install(_) | Commands::Remove(_) | Commands::Update(_)
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

#[derive(Debug, Clone)]
struct PlannedStagedInstall {
    is_update: bool,
    remove_paths: Vec<String>,
    replacement_removals: Vec<String>,
    renamed_transition: Option<RenamedPackageTransition>,
    hook_context: install::hooks::HookExecutionContextOwned,
}

#[derive(Debug, Clone)]
struct RenamedPackageTransition {
    replaced: db::InstalledPackageRecord,
    retained_files: Vec<String>,
    retained_directories: Vec<String>,
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
struct PlannedPackageInstall {
    spec: package::PackageSpec,
    destdir: PathBuf,
    staged: PlannedStagedInstall,
}

#[derive(Clone, Copy)]
struct PendingLifecycleHook {
    hook: install::scripts::Hook,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct InstalledPackageOutcome {
    package: package::PackageInfo,
    is_update: bool,
}

#[derive(Debug, Clone)]
struct InstallConflictSubject {
    package: String,
    provides: Vec<String>,
    conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
struct InstalledConflictPackage {
    name: String,
    provides: Vec<String>,
}

fn install_conflict_subjects_for_output_spec(
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

fn install_conflict_subjects_for_spec(
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

fn install_conflict_subject_for_binary_record(
    record: &db::repo::BinaryRepoPackageRecord,
) -> InstallConflictSubject {
    InstallConflictSubject {
        package: record.name.clone(),
        provides: record.provides.clone(),
        conflicts: record.conflicts.clone(),
    }
}

fn matching_conflict_names(
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

fn validate_no_transaction_conflicts(subjects: &[InstallConflictSubject]) -> Result<()> {
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

fn collect_installed_conflict_packages(db_path: &Path) -> Result<Vec<InstalledConflictPackage>> {
    let mut installed = Vec::new();
    for record in db::list_installed_package_records(db_path)? {
        installed.push(InstalledConflictPackage {
            provides: db::get_package_provides(db_path, &record.name)?,
            name: record.name,
        });
    }
    Ok(installed)
}

fn collect_conflicting_installed_packages(
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

fn collect_installed_replacement_packages(
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

fn remove_installed_package_with_hooks(
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
    install::hooks::run_transaction_hooks(
        rootfs,
        &install::hooks::HookExecutionContext {
            phase: install::hooks::HookPhase::Post,
            operation: install::hooks::HookOperation::Remove,
            package,
            affected_paths: &affected_paths,
        },
    )?;
    ui::success(format!("Successfully removed {}", package));
    Ok(())
}

fn resolve_installed_conflicts_for_subjects(
    subjects: &[InstallConflictSubject],
    rootfs: &Path,
    config: &config::Config,
    dry_run: bool,
) -> Result<()> {
    if subjects.is_empty() {
        return Ok(());
    }

    let db_path = config.installed_db_path(rootfs);
    let installed = collect_installed_conflict_packages(&db_path)?;
    let removals = collect_conflicting_installed_packages(subjects, &installed)?;
    if removals.is_empty() {
        return Ok(());
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
        return Ok(());
    }

    if !ui::prompt_package_action("conflict removal", &prompt_entries, true)? {
        anyhow::bail!("Aborted");
    }

    for package in removals.keys() {
        ui::info(format!("Removing conflicting package: {}", package));
        remove_installed_package_with_hooks(package, rootfs, config)?;
    }

    Ok(())
}

fn is_versioned_shared_library_path(path: &str) -> bool {
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

fn retained_abi_files_for_replacement(
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

fn retained_directories_for_files(
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

fn compare_installed_records_for_stream(
    left: &db::InstalledPackageRecord,
    right: &db::InstalledPackageRecord,
) -> Ordering {
    compare_package_release(&left.version, left.revision, &right.version, right.revision)
        .then_with(|| compare_completed_at(left.completed_at, right.completed_at))
        .then_with(|| left.name.cmp(&right.name))
}

fn select_primary_installed_record<'a>(
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

fn build_renamed_package_transition(
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

fn plan_staged_install(
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

fn plan_package_outputs_for_install(
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

fn run_transaction_hooks_for_plans(
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

fn install_staged_to_rootfs(
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

fn install_planned_packages_to_rootfs(
    plans: &[PlannedPackageInstall],
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    let mut removed_replacements = HashSet::new();
    let mut pending_post_hooks = Vec::new();
    for (idx, plan) in plans.iter().enumerate() {
        ui::info(format!(
            "{}/{} Installing package {}-{}-{}",
            idx + 1,
            plans.len(),
            plan.spec.package.name,
            plan.spec.package.version,
            plan.spec.package.revision
        ));
        for package in &plan.staged.replacement_removals {
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

#[cfg(test)]
fn install_package_outputs_to_rootfs(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    rootfs: &Path,
    config: &config::Config,
) -> Result<Vec<InstalledPackageOutcome>> {
    let plans = plan_package_outputs_for_install(pkg_spec, destdir, rootfs, config)?;
    let installed = plans
        .iter()
        .map(|plan| InstalledPackageOutcome {
            package: plan.spec.package.clone(),
            is_update: plan.staged.is_update,
        })
        .collect();
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Pre, &plans)?;
    install_planned_packages_to_rootfs(&plans, rootfs, config)?;
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Post, &plans)?;
    Ok(installed)
}

fn print_plan_summary(plan: &planner::ExecutionPlan) {
    let summary = plan.summary();
    ui::info(format!(
        "Plan summary: packages={} actions={} (binary_install={}, source_build_install={}, skip_installed={}) known_download={}",
        summary.total_packages,
        summary.binary_installs + summary.source_build_installs,
        summary.binary_installs,
        summary.source_build_installs,
        summary.skipped_installed,
        human_bytes(summary.known_download_bytes)
    ));
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

fn actionable_plan_packages(plan: &planner::ExecutionPlan) -> Vec<String> {
    plan.actionable_steps()
        .map(|step| step.package.clone())
        .collect()
}

fn source_build_reason(reason: &str) -> String {
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

fn source_build_warning_messages(plan: &planner::ExecutionPlan) -> Vec<String> {
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

fn warn_source_build_plan(plan: &planner::ExecutionPlan) {
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

fn validate_source_build_prereqs_for_plan(
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
struct InstallPlanExecutionOptions<'a> {
    no_flags: bool,
    cross_prefix: Option<&'a str>,
    clean: bool,
    dry_run: bool,
    confirm_installation: bool,
    lib32_only_requested_specs: bool,
    install_test_deps: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildInstallBatch {
    requests: Vec<PathBuf>,
    lib32_only: bool,
}

fn step_requests_only_lib32(
    step: &planner::PlannedStep,
    options: &InstallPlanExecutionOptions<'_>,
) -> bool {
    options.lib32_only_requested_specs
        && step
            .requested_by
            .iter()
            .any(|reason| reason.starts_with("requested "))
}

fn build_live_rootfs_child_install_batches(
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

fn flush_binary_install_batch(
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

fn execute_install_plan_with_child_commands(
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
    for step in &actionable_steps {
        if let planner::PlanOrigin::Binary { repo_name, record } = &step.origin {
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
        for item in &binary_phase_items {
            let label = format!(
                "{}-{}-{}",
                item.record.name,
                item.record.version,
                binary_arch_from_filename(&item.record.filename)
            );
            let pb = ProgressBar::new(item.record.size.max(1));
            pb.set_draw_target(if use_tty_progress {
                ProgressDrawTarget::stderr()
            } else {
                ProgressDrawTarget::hidden()
            });
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{prefix:.bold} [{bar:40.cyan/blue}] {eta}")
                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                    .progress_chars("#>-"),
            );
            pb.set_prefix(label);

            let repo_cfg = config
                .binary_repos
                .get(&item.repo_name)
                .with_context(|| format!("Binary repo '{}' not found in config", item.repo_name))?;
            let mut progress_cb = |downloaded: u64, total: Option<u64>| {
                if let Some(t) = total
                    && t > 0
                {
                    pb.set_length(t);
                }
                pb.set_position(downloaded);
            };
            let cached = db::repo::cache_binary_package_archive_with_progress(
                &item.repo_name,
                repo_cfg,
                &item.record,
                &config.package_cache_dir,
                Some(&mut progress_cb),
            )
            .with_context(|| {
                format!(
                    "Failed to cache binary package '{}' from repo '{}'",
                    item.record.filename, item.repo_name
                )
            })?;
            pb.finish_and_clear();
            binary_archives.insert(
                (item.repo_name.clone(), item.record.filename.clone()),
                cached,
            );
        }

        ui::info(format!(
            "Verifying checksums for {} binary package(s)...",
            binary_phase_items.len()
        ));
        let checksum_pb = ProgressBar::new(binary_phase_items.len() as u64);
        checksum_pb.set_draw_target(if use_tty_progress {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        });
        checksum_pb.set_style(
            ProgressStyle::default_bar()
                .template("{prefix:.bold} [{bar:40.cyan/blue}] {pos}/{len} {eta}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        checksum_pb.set_prefix("checksums");
        for item in &binary_phase_items {
            let cached = binary_archives
                .get(&(item.repo_name.clone(), item.record.filename.clone()))
                .with_context(|| {
                    format!(
                        "Cached archive missing for {} from repo '{}'",
                        item.record.filename, item.repo_name
                    )
                })?;
            db::repo::verify_binary_package_archive_checksums(&cached.package_path, &item.record)
                .with_context(|| {
                format!(
                    "Checksum verification failed for {} from repo '{}'",
                    item.record.filename, item.repo_name
                )
            })?;
            checksum_pb.inc(1);
        }
        checksum_pb.finish_and_clear();

        ui::info(format!(
            "Verifying detached signatures for {} binary package(s)...",
            binary_phase_items.len()
        ));
        let signature_pb = ProgressBar::new(binary_phase_items.len() as u64);
        signature_pb.set_draw_target(if use_tty_progress {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        });
        signature_pb.set_style(
            ProgressStyle::default_bar()
                .template("{prefix:.bold} [{bar:40.cyan/blue}] {pos}/{len} {eta}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        signature_pb.set_prefix("signatures");
        for item in &binary_phase_items {
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
            db::repo::verify_binary_package_archive_signature(
                &item.repo_name,
                repo_cfg,
                rootfs,
                &cached.package_path,
                &cached.signature_path,
            )
            .with_context(|| {
                format!(
                    "Detached signature verification failed for {} from repo '{}'",
                    item.record.filename, item.repo_name
                )
            })?;
            signature_pb.inc(1);
        }
        signature_pb.finish_and_clear();
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

fn is_archive_install_request(spec_path: &Path) -> bool {
    spec_path.exists()
        && spec_path
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".tar.zst")
}

fn shared_local_sibling_root(spec_paths: &[PathBuf]) -> Option<PathBuf> {
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
struct DirectInstallOptions<'a> {
    rootfs: &'a Path,
    no_deps: bool,
    no_flags: bool,
    cross_prefix: Option<&'a str>,
    clean: bool,
    dry_run: bool,
    lib32_only: bool,
    install_test_deps: bool,
}

fn run_direct_archive_install_requests(
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

    run_transaction_hooks_for_plans(
        options.rootfs,
        install::hooks::HookPhase::Pre,
        &transaction_plans,
    )?;
    install_planned_packages_to_rootfs(&transaction_plans, options.rootfs, config)?;
    run_transaction_hooks_for_plans(
        options.rootfs,
        install::hooks::HookPhase::Post,
        &transaction_plans,
    )?;

    Ok(true)
}

fn run_direct_install_request(
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

    let (mut pkg_spec, staging_dir): (package::PackageSpec, Option<tempfile::TempDir>) =
        if spec_path.to_string_lossy().ends_with(".tar.zst") {
            // Install from archive
            let (spec, tmp_dir) = load_package_archive_into_staging(config, &spec_path)?;
            (spec, Some(tmp_dir))
        } else {
            // Install from spec (normal build)
            let mut pkg_spec = package::PackageSpec::from_file(&spec_path)?;
            pkg_spec.apply_config(config);
            (pkg_spec, None)
        };

    if options.lib32_only && staging_dir.is_some() {
        anyhow::bail!("--lib32-only is only supported when installing from a package spec");
    }
    let lib32_only = effective_lib32_only(&pkg_spec, options.lib32_only);

    if staging_dir.is_none() && !suppress_output {
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

    let install_targets = vec![format!(
        "{} v{}-{}",
        pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
    )];
    if !suppress_output && !ui::prompt_package_action("installation", &install_targets, true)? {
        anyhow::bail!("Aborted");
    }

    let _snapper_pre_install_snapshot_todo: fn() -> ! =
        || todo!("snapper: create pre-install snapshot before install work starts");

    // Ensure database directory exists
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

    // Check dependencies and prompt for auto-install if needed
    if !options.no_deps {
        deps::print_dep_status_for_outputs(&pkg_spec, &db_path, requested_outputs)?;

        let missing_required = merge_missing_dependencies(
            deps::check_build_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?,
            deps::check_runtime_deps_for_outputs(&pkg_spec, &db_path, requested_outputs)?,
        );
        if !missing_required.is_empty() {
            // Check for dependency cycles via DEPOT_DEPCHAIN env var
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
                // Build package index for fast lookups
                let pkg_index =
                    index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));

                // Build new dep chain
                let new_chain = if dep_chain.is_empty() {
                    pkg_spec.package.name.clone()
                } else {
                    format!("{},{}", dep_chain, pkg_spec.package.name)
                };

                let mut dep_spec_paths = Vec::new();
                for dep in missing_required {
                    // Use package index for O(1) lookup
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

        // Enforce required dependencies before building/installing.
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
        // 1-2. Fetch + extract sources (supports archives and git URL#rev)
        source::preflight_manual_sources(&pkg_spec, &config.cache_dir)?;
        let src_dir = source::prepare(&pkg_spec, &config.cache_dir, &config.build_dir)?;
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

        // 3. Build
        let destdir = config
            .build_dir
            .join("destdir")
            .join(&pkg_spec.package.name);

        if !lib32_only {
            builder::build(
                &pkg_spec,
                &src_dir,
                &destdir,
                cross_config.as_ref(),
                !options.no_flags,
                host_build_dir.as_deref(),
            )?;

            // 3.1 Copy license files into staged tree
            staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;
            install::scripts::stage_scripts_from_spec_dir(&pkg_spec, &destdir)?;
        }

        destdir
    };

    let mut transaction_plans = Vec::new();

    if !lib32_only {
        if staging_dir.is_none() {
            // Source-build path: apply staging transforms (strip/compress/static cleanup).
            staging::process(&destdir, &pkg_spec)?;
            if let Some(src_dir) = built_src_dir.as_deref() {
                staging::stage_split_package_licenses(src_dir, &destdir, &pkg_spec)?;
            }
        } else {
            // Binary archive path: install as-packaged without post-build transformations.
            if !suppress_output {
                ui::info("Installing binary archive payload");
            }
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

    run_transaction_hooks_for_plans(
        options.rootfs,
        install::hooks::HookPhase::Pre,
        &transaction_plans,
    )?;
    let _snapper_post_install_snapshot_todo: fn() -> ! =
        || todo!("snapper: create post-install snapshot after install commit succeeds");
    install_planned_packages_to_rootfs(&transaction_plans, options.rootfs, config)?;
    run_transaction_hooks_for_plans(
        options.rootfs,
        install::hooks::HookPhase::Post,
        &transaction_plans,
    )?;

    Ok(true)
}

mod build_cmd;
mod check;
mod install_cmd;
mod misc;
mod repo;
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
        Commands::MakeSpec(args) => misc::run_make_spec(args)?,
        Commands::Convert(args) => misc::run_convert(args)?,
        Commands::Internal(args) => misc::run_internal(args)?,
    }

    Ok(())
}

#[cfg(test)]
mod tests;
