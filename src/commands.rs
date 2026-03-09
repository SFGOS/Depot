use crate::cli::{Cli, Commands, RepoCommands, RepoKindArg};
use crate::{
    builder, cli_assets, config, cross, db, deps, index, install, locking, package, planner,
    signing, source, staging, ui,
};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::collections::HashMap;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

fn rootfs_is_system_root(rootfs: &Path) -> bool {
    if rootfs == Path::new("/") {
        return true;
    }
    fs::canonicalize(rootfs)
        .map(|path| path == Path::new("/"))
        .unwrap_or(false)
}

fn command_requires_live_root(command: &Commands) -> bool {
    matches!(command, Commands::Install { .. } | Commands::Remove { .. })
}

fn should_reexec_with_sudo(cli: &Cli) -> bool {
    !crate::fakeroot::is_root()
        && rootfs_is_system_root(&cli.rootfs)
        && command_requires_live_root(&cli.command)
}

fn should_delegate_live_rootfs_installs(rootfs: &Path) -> bool {
    !crate::fakeroot::is_root() && rootfs_is_system_root(rootfs)
}

fn maybe_reexec_with_sudo(cli: &Cli) -> Result<bool> {
    if !should_reexec_with_sudo(cli) {
        return Ok(false);
    }

    let exe = std::env::current_exe().context("Failed to locate depot executable")?;
    let mut cmd = std::process::Command::new("sudo");
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

fn run_child_install_command(
    install_request: &Path,
    rootfs: &Path,
    options: InstallPlanExecutionOptions<'_>,
) -> Result<()> {
    let exe = std::env::current_exe().context("Failed to locate depot executable")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("-r").arg(rootfs);
    cmd.arg("--no-deps");
    cmd.arg("--yes");
    if options.no_flags {
        cmd.arg("--no-flags");
    }
    if let Some(p) = options.cross_prefix {
        cmd.arg("--cross-prefix").arg(p);
    }
    if options.clean {
        cmd.arg("--clean");
    }
    cmd.arg("install").arg(install_request);

    let status = cmd.status().with_context(|| {
        format!(
            "Failed to spawn child install for {}",
            install_request.display()
        )
    })?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "Child install failed for {} with status {}",
            install_request.display(),
            status
        );
    }
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

fn staging_temp_root(config: &config::Config) -> PathBuf {
    config.build_dir.join("staging")
}

fn create_archive_staging_dir(
    config: &config::Config,
    archive_path: &Path,
) -> Result<tempfile::TempDir> {
    let staging_root = staging_temp_root(config);
    fs::create_dir_all(&staging_root)
        .with_context(|| format!("Failed to create staging root {}", staging_root.display()))?;
    tempfile::Builder::new()
        .prefix("archive-")
        .tempdir_in(&staging_root)
        .with_context(|| {
            format!(
                "Failed to create staging dir for {} under {}",
                archive_path.display(),
                staging_root.display()
            )
        })
}

fn load_package_archive_into_staging(
    config: &config::Config,
    archive_path: &Path,
) -> Result<(package::PackageSpec, tempfile::TempDir)> {
    let tmp_dir = create_archive_staging_dir(config, archive_path).with_context(|| {
        format!(
            "Failed to create staging dir for {}",
            archive_path.display()
        )
    })?;
    let extract_dir = tmp_dir.path().to_path_buf();

    let file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive {}", archive_path.display()))?;
    let zstd_decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("Failed to read zstd stream {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(zstd_decoder);

    let mut metadata_content = String::new();
    for entry in archive.entries().with_context(|| {
        format!(
            "Failed to read archive entries from {}",
            archive_path.display()
        )
    })? {
        let mut entry = entry.with_context(|| {
            format!(
                "Failed to read archive entry from {}",
                archive_path.display()
            )
        })?;
        if entry.path()?.to_string_lossy() == ".metadata.toml" {
            use std::io::Read;
            entry
                .read_to_string(&mut metadata_content)
                .with_context(|| {
                    format!(
                        "Failed to read .metadata.toml in {}",
                        archive_path.display()
                    )
                })?;
            break;
        }
    }

    if metadata_content.is_empty() {
        anyhow::bail!(
            "Package archive does not contain .metadata.toml: {}",
            archive_path.display()
        );
    }

    let file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive {}", archive_path.display()))?;
    let zstd_decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("Failed to read zstd stream {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(zstd_decoder);
    archive.unpack(&extract_dir).with_context(|| {
        format!(
            "Failed to extract package archive {} into {}",
            archive_path.display(),
            extract_dir.display()
        )
    })?;

    let metadata: toml::Value = toml::from_str(&metadata_content).with_context(|| {
        format!(
            "Failed to parse .metadata.toml in {}",
            archive_path.display()
        )
    })?;

    let mut spec = package::PackageSpec {
        package: package::PackageInfo {
            name: metadata
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            version: metadata
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            revision: metadata
                .get("revision")
                .and_then(|v| v.as_integer())
                .unwrap_or(1) as u32,
            description: metadata
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            homepage: metadata
                .get("homepage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            license: parse_licenses_from_toml(&metadata),
        },
        packages: Vec::new(),
        alternatives: package::Alternatives::default(),
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: package::Build {
            build_type: package::BuildType::Bin,
            flags: package::BuildFlags::default(),
        },
        dependencies: package::Dependencies {
            build: Vec::new(),
            runtime: parse_dependency_list(&metadata, "runtime"),
            test: Vec::new(),
            optional: parse_dependency_list(&metadata, "optional"),
        },
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    if let Some(provides) = metadata.get("provides").and_then(|v| v.as_array()) {
        spec.alternatives.provides = provides
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }

    Ok((spec, tmp_dir))
}

fn extract_package_archive_to_staging(
    config: &config::Config,
    archive_path: &Path,
) -> Result<tempfile::TempDir> {
    let tmp_dir = create_archive_staging_dir(config, archive_path).with_context(|| {
        format!(
            "Failed to create staging dir for {}",
            archive_path.display()
        )
    })?;
    let extract_dir = tmp_dir.path().to_path_buf();

    let file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive {}", archive_path.display()))?;
    let zstd_decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("Failed to read zstd stream {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(zstd_decoder);
    archive.unpack(&extract_dir).with_context(|| {
        format!(
            "Failed to extract package archive {} into {}",
            archive_path.display(),
            extract_dir.display()
        )
    })?;
    Ok(tmp_dir)
}

fn parse_license_list_from_repo(license: &Option<String>) -> Vec<String> {
    let Some(raw) = license.as_ref() else {
        return Vec::new();
    };
    raw.split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(String::from)
        .collect()
}

fn package_spec_from_repo_record(
    record: &db::repo::BinaryRepoPackageRecord,
) -> package::PackageSpec {
    package::PackageSpec {
        package: package::PackageInfo {
            name: record.name.clone(),
            version: record.version.clone(),
            revision: record.revision,
            description: record.description.clone().unwrap_or_default(),
            homepage: record.homepage.clone().unwrap_or_default(),
            license: parse_license_list_from_repo(&record.license),
        },
        packages: Vec::new(),
        alternatives: package::Alternatives {
            provides: record.provides.clone(),
            replaces: Vec::new(),
        },
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: package::Build {
            build_type: package::BuildType::Bin,
            flags: package::BuildFlags::default(),
        },
        dependencies: package::Dependencies {
            build: Vec::new(),
            runtime: record.runtime_dependencies.clone(),
            test: Vec::new(),
            optional: record.optional_dependencies.clone(),
        },
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    }
}

fn build_type_runs_automatic_tests(build_type: package::BuildType) -> bool {
    matches!(
        build_type,
        package::BuildType::Autotools | package::BuildType::CMake | package::BuildType::Perl
    )
}

fn maybe_disable_tests_for_missing_deps(
    pkg_spec: &mut package::PackageSpec,
    db_path: &Path,
) -> Result<()> {
    if pkg_spec.build.flags.skip_tests
        || !build_type_runs_automatic_tests(pkg_spec.build.build_type)
        || pkg_spec.dependencies.test.is_empty()
    {
        return Ok(());
    }

    let missing_test = deps::check_test_deps(pkg_spec, db_path)?;
    if !missing_test.is_empty() {
        ui::warn(format!(
            "Missing test dependencies: {}. Tests will be skipped.",
            missing_test.join(", ")
        ));
        pkg_spec.build.flags.skip_tests = true;
    }

    Ok(())
}

fn clean_build_workspace(config: &config::Config) -> Result<()> {
    if config.build_dir.exists() {
        fs::remove_dir_all(&config.build_dir).with_context(|| {
            format!("Failed to clean build dir: {}", config.build_dir.display())
        })?;
        ui::success(format!(
            "Cleaned build workspace: {}",
            config.build_dir.display()
        ));
    }
    if config.cache_dir.exists() {
        fs::remove_dir_all(&config.cache_dir).with_context(|| {
            format!(
                "Failed to clean source cache dir: {}",
                config.cache_dir.display()
            )
        })?;
        ui::success(format!(
            "Cleaned source cache: {}",
            config.cache_dir.display()
        ));
    }
    Ok(())
}

fn warn_if_running_as_root_for_build(command: &str, rootfs: &Path) {
    if crate::fakeroot::is_root() {
        ui::warn(format!("Running '{}' as root is discouraged.", command));
        ui::warn(
            "A misconfigured build environment or malicious/buggy build file can overwrite or delete critical system files.",
        );
        ui::warn("Recommendation: use a non-root build user and only install as root.");
        ui::warn(format!("Current rootfs target: {}", rootfs.display()));
    }
}

fn output_destdir_for(base_destdir: &Path, primary_pkg: &str, output_pkg: &str) -> PathBuf {
    if output_pkg == primary_pkg {
        base_destdir.to_path_buf()
    } else {
        staging::output_staging_dir(base_destdir, output_pkg)
    }
}

fn lib32_package_name(name: &str) -> String {
    format!("lib32-{name}")
}

fn lib32_arch_name(arch: &str) -> String {
    arch.replace("x86_64", "i686")
}

fn make_lib32_build_spec(base: &package::PackageSpec) -> package::PackageSpec {
    let mut spec = base.clone();
    let flags = &mut spec.build.flags;
    flags.lib32_variant = true;

    if !flags.cflags_lib32.is_empty() {
        flags.cflags.extend(flags.cflags_lib32.clone());
    }
    if !flags.cxxflags_lib32.is_empty() {
        flags.cxxflags.extend(flags.cxxflags_lib32.clone());
    }
    if !flags.configure_lib32.is_empty() {
        flags.configure = flags.configure_lib32.clone();
    }
    if !flags.post_configure_lib32.is_empty() {
        flags.post_configure = flags.post_configure_lib32.clone();
    }
    if !flags.post_compile_lib32.is_empty() {
        flags.post_compile = flags.post_compile_lib32.clone();
    }
    if !flags.post_install_lib32.is_empty() {
        flags.post_install = flags.post_install_lib32.clone();
    }

    flags.cc = format!("{} -m32", flags.cc.trim());
    flags.cxx = format!("{} -m32", flags.cxx.trim());
    flags.build_dir = Some(match flags.build_dir.as_deref() {
        Some(dir) if !dir.trim().is_empty() => format!("{}-lib32", dir.trim()),
        _ => "build-lib32".to_string(),
    });

    spec
}

fn make_lib32_package_spec(base: &package::PackageSpec) -> package::PackageSpec {
    let mut spec = base.clone();
    spec.package.name = lib32_package_name(&base.package.name);
    // The lib32 pass currently emits a single companion package from /usr/lib32.
    spec.packages.clear();
    spec
}

fn copy_tree_preserving_links(src: &Path, dst: &Path) -> Result<()> {
    use walkdir::WalkDir;

    fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create destination dir: {}", dst.display()))?;

    for entry in WalkDir::new(src) {
        let entry = entry?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .with_context(|| format!("Failed to strip prefix: {}", src.display()))?;
        let target = dst.join(rel);

        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("Failed to create dir: {}", target.display()))?;
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create dir: {}", parent.display()))?;
        }

        if entry.file_type().is_symlink() {
            let link_target = fs::read_link(entry.path())
                .with_context(|| format!("Failed to read symlink: {}", entry.path().display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs as unix_fs;
                unix_fs::symlink(&link_target, &target).with_context(|| {
                    format!(
                        "Failed to create symlink {} -> {}",
                        target.display(),
                        link_target.display()
                    )
                })?;
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!(
                    "Symlink-preserving lib32 staging copy is only supported on unix hosts"
                );
            }
        } else {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
        }
    }

    Ok(())
}

fn build_lib32_companion_package(
    pkg_spec: &package::PackageSpec,
    src_dir: &Path,
    config: &config::Config,
    cross_config: Option<&cross::CrossConfig>,
    export_compiler_flags: bool,
    force: bool,
) -> Result<Option<(package::PackageSpec, PathBuf)>> {
    if !pkg_spec.build.flags.build_32 && !force {
        return Ok(None);
    }
    if pkg_spec.is_metapackage() {
        crate::log_warn!(
            "Ignoring build.flags.build-32 for metapackage {}",
            pkg_spec.package.name
        );
        return Ok(None);
    }

    crate::log_info!("Running separate lib32 build pass...");
    let mut lib32_input = pkg_spec.clone();
    lib32_input.build.flags.build_32 = true;
    let lib32_build_spec = make_lib32_build_spec(&lib32_input);
    let lib32_install_destdir = config
        .build_dir
        .join("destdir")
        .join(".lib32-build")
        .join(&pkg_spec.package.name)
        .join("lib32-dest");

    builder::build(
        &lib32_build_spec,
        src_dir,
        &lib32_install_destdir,
        cross_config,
        export_compiler_flags,
    )?;

    let lib32_src = lib32_install_destdir.join("usr/lib32");
    if !lib32_src.exists() {
        anyhow::bail!(
            "lib32 build completed but did not install usr/lib32 into {}",
            lib32_install_destdir.display()
        );
    }

    let lib32_pkg_spec = make_lib32_package_spec(pkg_spec);
    let lib32_destdir = config
        .build_dir
        .join("destdir")
        .join(&lib32_pkg_spec.package.name);
    if lib32_destdir.exists() {
        fs::remove_dir_all(&lib32_destdir).with_context(|| {
            format!("Failed to clean lib32 destdir: {}", lib32_destdir.display())
        })?;
    }
    fs::create_dir_all(&lib32_destdir).with_context(|| {
        format!(
            "Failed to create lib32 destdir: {}",
            lib32_destdir.display()
        )
    })?;

    copy_tree_preserving_links(&lib32_src, &lib32_destdir.join("usr/lib32"))?;
    staging::add_licenses(src_dir, &lib32_destdir, &lib32_pkg_spec.package.name)?;
    install::scripts::stage_scripts_from_spec_dir(&lib32_pkg_spec, &lib32_destdir)?;
    staging::process(&lib32_destdir, &lib32_pkg_spec)?;

    Ok(Some((lib32_pkg_spec, lib32_destdir)))
}

#[derive(Debug, Clone)]
struct PlannedStagedInstall {
    is_update: bool,
    remove_paths: Vec<String>,
    hook_context: install::hooks::HookExecutionContextOwned,
}

#[derive(Debug, Clone)]
struct PlannedPackageInstall {
    spec: package::PackageSpec,
    destdir: PathBuf,
    staged: PlannedStagedInstall,
}

fn plan_staged_install(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    config: &config::Config,
) -> Result<PlannedStagedInstall> {
    std::fs::create_dir_all(&config.db_dir).with_context(|| {
        format!(
            "Failed to create database directory: {}",
            config.db_dir.display()
        )
    })?;
    let db_path = config.db_dir.join("packages.db");

    let is_update = db::get_package_version(&db_path, &pkg_spec.package.name)?.is_some();
    let new_files = staging::generate_manifest_with_dirs(destdir)?;
    let remove_paths =
        db::calculate_upgrade_paths(&db_path, &pkg_spec.package.name, &new_files.files)?;
    let operation = if is_update {
        install::hooks::HookOperation::Update
    } else {
        install::hooks::HookOperation::Install
    };
    let mut affected_paths = new_files.files.clone();
    affected_paths.extend(remove_paths.iter().cloned());
    affected_paths.sort();
    affected_paths.dedup();

    Ok(PlannedStagedInstall {
        is_update,
        remove_paths,
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
    config: &config::Config,
) -> Result<Vec<PlannedPackageInstall>> {
    let mut plans = Vec::new();
    for out in pkg_spec.outputs() {
        let mut spec_for_out = pkg_spec.clone();
        let output_name = out.name.clone();
        spec_for_out.package = out;
        spec_for_out.alternatives = pkg_spec.alternatives_for_output(&output_name);
        spec_for_out.dependencies = pkg_spec.dependencies_for_output(&output_name);
        let out_destdir = output_destdir_for(destdir, &pkg_spec.package.name, &output_name);
        let staged = plan_staged_install(&spec_for_out, &out_destdir, config)?;
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
) -> Result<()> {
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

    let db_path = config.db_dir.join("packages.db");
    if let Err(e) = db::register_package(&db_path, pkg_spec, destdir) {
        let _ = tx.rollback();
        return Err(e);
    }
    tx.commit()?;

    install::scripts::sync_staged_scripts_to_rootfs(
        &staged_scripts_dir,
        rootfs,
        &pkg_spec.package.name,
    )?;

    if plan.is_update {
        let _ = install::scripts::run_hook_if_present(
            &installed_scripts_dir,
            install::scripts::Hook::PostUpdate,
            rootfs,
            &pkg_spec.package.name,
        )?;
    } else {
        let _ = install::scripts::run_hook_if_present(
            &installed_scripts_dir,
            install::scripts::Hook::PostInstall,
            rootfs,
            &pkg_spec.package.name,
        )?;
    }

    Ok(())
}

fn install_planned_packages_to_rootfs(
    plans: &[PlannedPackageInstall],
    rootfs: &Path,
    config: &config::Config,
) -> Result<Vec<package::PackageInfo>> {
    let mut installed = Vec::with_capacity(plans.len());
    for plan in plans {
        install_staged_to_rootfs(&plan.spec, &plan.destdir, rootfs, config, &plan.staged)?;
        installed.push(plan.spec.package.clone());
    }
    Ok(installed)
}

#[cfg(test)]
fn install_package_outputs_to_rootfs(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    rootfs: &Path,
    config: &config::Config,
) -> Result<Vec<package::PackageInfo>> {
    let plans = plan_package_outputs_for_install(pkg_spec, destdir, config)?;
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Pre, &plans)?;
    let installed = install_planned_packages_to_rootfs(&plans, rootfs, config)?;
    run_transaction_hooks_for_plans(rootfs, install::hooks::HookPhase::Post, &plans)?;
    Ok(installed)
}

fn repo_kind_label(kind: RepoKindArg) -> &'static str {
    match kind {
        RepoKindArg::Source => "source",
        RepoKindArg::Binary => "binary",
    }
}

fn resolve_repo_kind_for_name(
    repos: &config::RepoConfigFile,
    name: &str,
    kind: Option<RepoKindArg>,
) -> Result<RepoKindArg> {
    if let Some(kind) = kind {
        return Ok(kind);
    }

    let in_source = repos.source.contains_key(name);
    let in_binary = repos.binary.contains_key(name);
    match (in_source, in_binary) {
        (true, false) => Ok(RepoKindArg::Source),
        (false, true) => Ok(RepoKindArg::Binary),
        (true, true) => anyhow::bail!(
            "Repo '{}' exists as both source and binary; rerun with --kind source|binary",
            name
        ),
        (false, false) => anyhow::bail!("Repo '{}' not found in repos.toml", name),
    }
}

fn print_repo_list(config: &config::Config) {
    if config.source_repos.is_empty() && config.binary_repos.is_empty() {
        ui::info("No repos configured in /etc/depot.d/repos.toml");
        if !config.mirrors.is_empty() {
            ui::info("Legacy mirrors.toml entries are loaded as source repos at runtime.");
        }
        return;
    }

    ui::info(format!(
        "Repo settings: prefer_binary={}",
        config.repo_settings.prefer_binary
    ));

    if config.source_repos.is_empty() {
        ui::info("Source repos: none");
    } else {
        ui::info("Source repos:");
        for (name, repo) in &config.source_repos {
            let subdirs = if repo.subdirs.is_empty() {
                "(all)".to_string()
            } else {
                repo.subdirs.join(", ")
            };
            ui::info(format!(
                "  {} [{}] priority={} subdirs={} url={}",
                name,
                if repo.enabled { "enabled" } else { "disabled" },
                repo.priority,
                subdirs,
                repo.url
            ));
        }
    }

    if config.binary_repos.is_empty() {
        ui::info("Binary repos: none");
    } else {
        ui::info("Binary repos:");
        let host_arch = std::env::consts::ARCH;
        for (name, repo) in &config.binary_repos {
            let arch_keys = if repo.arch.is_empty() {
                "(any)".to_string()
            } else {
                repo.arch.keys().cloned().collect::<Vec<_>>().join(",")
            };
            ui::info(format!(
                "  {} [{}] priority={} arches={} host_match={} repo_db={}{} url={}",
                name,
                if repo.enabled { "enabled" } else { "disabled" },
                repo.priority,
                arch_keys,
                if repo.supports_arch(host_arch) {
                    "yes"
                } else {
                    "no"
                },
                repo.repo_db,
                if repo.allow_unsigned {
                    " allow_unsigned=true"
                } else {
                    ""
                },
                repo.url
            ));
        }
    }
}

fn selected_source_repos(
    config: &config::Config,
    name: Option<&str>,
) -> Result<std::collections::HashMap<String, String>> {
    let mut mirrors = config.enabled_source_mirror_map();
    if let Some(name) = name {
        if let Some(url) = mirrors.remove(name) {
            let mut only = std::collections::HashMap::new();
            only.insert(name.to_string(), url);
            return Ok(only);
        }
        anyhow::bail!("Enabled source repo '{}' not found", name);
    }
    Ok(mirrors)
}

fn source_search_hit_allowed(config: &config::Config, hit: &index::SourceSearchHit) -> bool {
    let path = &hit.path;

    if path.starts_with(Path::new("packages")) {
        return true;
    }

    if config.source_repos.is_empty() {
        return true;
    }

    for (repo_name, repo) in &config.source_repos {
        if !repo.enabled {
            continue;
        }
        let repo_root = config.repo_clone_dir.join(repo_name);
        if repo.subdirs.is_empty() {
            if path.starts_with(&repo_root) {
                return true;
            }
        } else {
            for subdir in &repo.subdirs {
                if path.starts_with(repo_root.join(subdir)) {
                    return true;
                }
            }
        }
    }

    false
}

fn source_hit_origin(config: &config::Config, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(&config.repo_clone_dir)
        && let Some(first) = rel.components().next()
    {
        return format!("source:{}", first.as_os_str().to_string_lossy());
    }
    "source:local".to_string()
}

fn run_search_command(
    query: &str,
    files: bool,
    config: &config::Config,
    rootfs: &Path,
) -> Result<()> {
    let mut any = false;
    let host_arch = std::env::consts::ARCH;

    let pkg_index = index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));
    let source_hits: Vec<_> = pkg_index
        .search(query)
        .into_iter()
        .filter(|hit| source_search_hit_allowed(config, hit))
        .collect();
    if !source_hits.is_empty() {
        any = true;
        ui::info("Source matches:");
        for hit in source_hits {
            let provides = if hit.provides.is_empty() {
                String::new()
            } else {
                format!(" provides={}", hit.provides.join(","))
            };
            ui::info(format!(
                "  {} [{}] {}{}",
                hit.name,
                source_hit_origin(config, &hit.path),
                hit.path.display(),
                provides
            ));
        }
    }

    let mut binary_repos: Vec<_> = config
        .binary_repos
        .iter()
        .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
        .collect();
    binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

    if !binary_repos.is_empty() {
        ui::info("Binary matches:");
    }
    let mut binary_hits_total = 0usize;
    for (name, repo) in binary_repos {
        match db::repo::search_binary_repo(name, repo, rootfs, &config.package_cache_dir, query) {
            Ok(hits) => {
                for hit in hits {
                    any = true;
                    binary_hits_total += 1;
                    let provides = if hit.provides.is_empty() {
                        String::new()
                    } else {
                        format!(" provides={}", hit.provides.join(","))
                    };
                    ui::info(format!(
                        "  {} [binary:{}] {}-{} size={} file={}{}{}",
                        hit.name,
                        hit.repo_name,
                        hit.version,
                        hit.revision,
                        hit.size,
                        hit.filename,
                        hit.description
                            .as_ref()
                            .map(|d| format!(" desc={}", d))
                            .unwrap_or_default(),
                        provides
                    ));
                }
            }
            Err(e) => crate::log_warn!("Binary repo '{}': {}", name, e),
        }
    }
    if !config.binary_repos.is_empty() && binary_hits_total == 0 {
        ui::info("  (no binary matches)");
    }

    if files && !config.binary_repos.is_empty() {
        ui::info("Binary file matches:");
        let mut file_hits_total = 0usize;
        let mut binary_repos: Vec<_> = config
            .binary_repos
            .iter()
            .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
            .collect();
        binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));
        for (name, repo) in binary_repos {
            match db::repo::search_binary_repo_files(
                name,
                repo,
                rootfs,
                &config.package_cache_dir,
                query,
            ) {
                Ok(hits) => {
                    for hit in hits {
                        any = true;
                        file_hits_total += 1;
                        ui::info(format!(
                            "  {} [binary:{}] {}-{} size={} owns={}",
                            hit.package_name,
                            hit.repo_name,
                            hit.version,
                            hit.revision,
                            hit.size,
                            hit.path
                        ));
                    }
                }
                Err(e) => crate::log_warn!("Binary repo '{}': {}", name, e),
            }
        }
        if file_hits_total == 0 {
            ui::info("  (no binary file matches)");
        }
    }

    if !any {
        ui::warn(format!("No matches found for '{}'", query));
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
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

fn binary_arch_from_filename(filename: &str) -> String {
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

fn merge_missing_dependencies(mut base: Vec<String>, extra: Vec<String>) -> Vec<String> {
    for dep in extra {
        if !base.contains(&dep) {
            base.push(dep);
        }
    }
    base
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

#[derive(Clone, Copy)]
struct InstallPlanExecutionOptions<'a> {
    no_flags: bool,
    cross_prefix: Option<&'a str>,
    clean: bool,
    dry_run: bool,
    confirm_installation: bool,
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

    let summary = plan.summary();
    let actionable_steps: Vec<_> = plan.actionable_steps().collect();
    if actionable_steps.is_empty() {
        ui::info("Nothing to do.");
        return Ok(());
    }

    if summary.source_build_installs > 0 {
        ui::warn(format!(
            "{} package(s) will be built from source before installation.",
            summary.source_build_installs
        ));
    }
    let planned_packages: Vec<String> = actionable_steps
        .iter()
        .map(|step| step.package.clone())
        .collect();
    if options.confirm_installation
        && !ui::prompt_package_action("installation", &planned_packages, true)?
    {
        anyhow::bail!("Aborted");
    }

    if options.dry_run {
        ui::info("Dry run enabled, no install/build actions executed.");
        return Ok(());
    }

    if should_delegate_live_rootfs_installs(rootfs) {
        for step in actionable_steps {
            match &step.origin {
                planner::PlanOrigin::Source { path, .. } => {
                    run_child_install_command(path, rootfs, options)?;
                }
                planner::PlanOrigin::Binary { repo_name, record } => {
                    let repo_cfg = config.binary_repos.get(repo_name).with_context(|| {
                        format!("Binary repo '{}' not found in config", repo_name)
                    })?;
                    let archive_path = db::repo::fetch_binary_package_archive(
                        repo_name,
                        repo_cfg,
                        rootfs,
                        record,
                        &config.package_cache_dir,
                    )?;
                    run_child_install_command(&archive_path, rootfs, options)?;
                }
                planner::PlanOrigin::Installed => {}
            }
        }
        return Ok(());
    }

    let mut binary_phase_items = Vec::new();
    for step in &actionable_steps {
        if let planner::PlanOrigin::Binary { repo_name, record } = &step.origin {
            binary_phase_items.push(BinaryPhaseItem {
                repo_name: repo_name.clone(),
                record: (**record).clone(),
            });
        }
    }

    let mut binary_archives: HashMap<(String, String), db::repo::BinaryRepoCachedArchive> =
        HashMap::new();
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
            let spec = package_spec_from_repo_record(record);
            let plans = plan_package_outputs_for_install(&spec, staged.path(), config)?;
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
    for (idx, step) in actionable_steps.into_iter().enumerate() {
        match &step.origin {
            planner::PlanOrigin::Source { path, .. } => {
                ui::info(format!(
                    "[{}/{}] building+installing {} from source",
                    idx + 1,
                    total_steps,
                    step.package
                ));

                let mut cmd = std::process::Command::new(&exe);
                cmd.arg("-r").arg(rootfs);
                cmd.arg("--no-deps");
                cmd.arg("--yes");
                if options.no_flags {
                    cmd.arg("--no-flags");
                }
                if let Some(p) = options.cross_prefix {
                    cmd.arg("--cross-prefix").arg(p);
                }
                if options.clean {
                    cmd.arg("--clean");
                }
                cmd.arg("install").arg(path);

                let status = cmd
                    .status()
                    .context("Failed to spawn planned install step")?;
                if !status.success() {
                    anyhow::bail!("Planned install step for '{}' failed", step.package);
                }
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
                let spec = package_spec_from_repo_record(record);
                let plans = plan_package_outputs_for_install(&spec, staged.path(), config)?;
                let installed = install_planned_packages_to_rootfs(&plans, rootfs, config)?;
                binary_post_hook_plans.extend(plans);
                for pkg in installed {
                    ui::success(format!("Installed {} v{}", pkg.name, pkg.version));
                }
            }
            planner::PlanOrigin::Installed => {}
        }
    }

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

    ui::info(format!("Installing package from: {}", spec_path.display()));

    let (mut pkg_spec, staging_dir): (package::PackageSpec, Option<tempfile::TempDir>) =
        if spec_path.to_string_lossy().ends_with(".tar.zst") {
            // Install from archive
            ui::info(format!("Detected package archive: {}", spec_path.display()));
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

    ui::info(format!(
        "Package: {} v{}-{}",
        pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
    ));

    if options.dry_run {
        ui::info("Dry run enabled, stopping before install/build work.");
        return Ok(false);
    }

    let install_targets = vec![format!(
        "{} v{}-{}",
        pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
    )];
    if !ui::prompt_package_action("installation", &install_targets, true)? {
        anyhow::bail!("Aborted");
    }

    // TODO(snapper): create pre-install snapshot before install work starts.

    // Ensure database directory exists
    std::fs::create_dir_all(&config.db_dir).with_context(|| {
        format!(
            "Failed to create database directory: {}",
            config.db_dir.display()
        )
    })?;
    let db_path = config.db_dir.join("packages.db");

    if staging_dir.is_none() {
        maybe_disable_tests_for_missing_deps(&mut pkg_spec, &db_path)?;
    }

    // Check dependencies and prompt for auto-install if needed
    if !options.no_deps {
        deps::print_dep_status(&pkg_spec, &db_path)?;

        // Collect all missing dependencies (build + runtime)
        let missing = merge_missing_dependencies(
            deps::check_build_deps(&pkg_spec, &db_path)?,
            deps::check_runtime_deps(&pkg_spec, &db_path)?,
        );
        if !missing.is_empty() {
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

            ui::warn(format!("Missing dependencies: {}", missing.join(", ")));
            if ui::prompt_package_action("dependency installation", &missing, true)? {
                // Build package index for fast lookups
                let pkg_index =
                    index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));

                // Build new dep chain
                let new_chain = if dep_chain.is_empty() {
                    pkg_spec.package.name.clone()
                } else {
                    format!("{},{}", dep_chain, pkg_spec.package.name)
                };

                // Attempt to install missing deps
                for dep in missing {
                    // Use package index for O(1) lookup
                    let candidate = pkg_index.find(&dep);

                    if let Some(dep_spec_path) = candidate {
                        ui::info(format!("Installing dependency: {}...", dep));

                        let mut cmd = std::process::Command::new(std::env::current_exe()?);
                        cmd.arg("-r").arg(options.rootfs);

                        if options.no_deps {
                            cmd.arg("--no-deps");
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

                        cmd.arg("install").arg(&dep_spec_path);
                        cmd.env("DEPOT_DEPCHAIN", &new_chain);

                        let status = cmd.status()?;

                        if !status.success() {
                            anyhow::bail!("Failed to install dependency: {}", dep);
                        }
                    } else {
                        anyhow::bail!("Could not find package spec for dependency: {}", dep);
                    }
                }
            }
        }

        // Enforce required dependencies before building/installing.
        deps::require_build_deps(&pkg_spec, &db_path)?;
        deps::require_runtime_deps(&pkg_spec, &db_path)?;
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
        let src_dir = source::prepare(&pkg_spec, &config.cache_dir, &config.build_dir)?;
        built_src_dir = Some(src_dir.clone());

        // 3. Build
        let destdir = config
            .build_dir
            .join("destdir")
            .join(&pkg_spec.package.name);

        if !options.lib32_only {
            builder::build(
                &pkg_spec,
                &src_dir,
                &destdir,
                cross_config.as_ref(),
                !options.no_flags,
            )?;

            // 3.1 Copy license files into staged tree
            staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;
            install::scripts::stage_scripts_from_spec_dir(&pkg_spec, &destdir)?;
        }

        destdir
    };

    let mut transaction_plans = Vec::new();

    if !options.lib32_only {
        if staging_dir.is_none() {
            // Source-build path: apply staging transforms (strip/compress/static cleanup).
            staging::process(&destdir, &pkg_spec)?;
        } else {
            // Binary archive path: install as-packaged without post-build transformations.
            ui::info("Installing binary archive payload without staging transforms");
        }

        let output_plans = plan_package_outputs_for_install(&pkg_spec, &destdir, config)?;
        transaction_plans.extend(output_plans);
    }

    if let Some(src_dir) = built_src_dir.as_deref()
        && let Some((lib32_spec, lib32_destdir)) = build_lib32_companion_package(
            &pkg_spec,
            src_dir,
            config,
            cross_config.as_ref(),
            !options.no_flags,
            options.lib32_only,
        )?
    {
        let staged = plan_staged_install(&lib32_spec, &lib32_destdir, config)?;
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
    let installed = install_planned_packages_to_rootfs(&transaction_plans, options.rootfs, config)?;
    for pkg in installed {
        ui::success(format!(
            "Successfully installed {} v{}",
            pkg.name, pkg.version
        ));
        // TODO(snapper): create post-install snapshot after install commit succeeds.
    }
    run_transaction_hooks_for_plans(
        options.rootfs,
        install::hooks::HookPhase::Post,
        &transaction_plans,
    )?;

    Ok(true)
}

pub fn run(cli: Cli) -> Result<()> {
    ui::set_assume_yes(cli.yes);
    if maybe_reexec_with_sudo(&cli)? {
        return Ok(());
    }

    match cli.command {
        Commands::Install {
            spec_or_archive,
            spec,
        } => {
            let install_requests = match spec {
                Some(spec_path) => vec![spec_path],
                None => spec_or_archive,
            };

            // Load configuration early so we can use configured repos/paths.
            let config = config::Config::for_rootfs(&cli.rootfs);
            let mut planned_targets = Vec::new();
            let mut planned_spec_paths = Vec::new();
            let mut direct_requests = Vec::new();

            if cli.no_deps {
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
                    assume_yes: cli.yes,
                    prefer_binary: config.repo_settings.prefer_binary,
                    local_sibling_root: shared_local_sibling_root(&planned_spec_paths),
                };
                let plan = if planned_targets.len() == 1 {
                    planner::build_install_plan(
                        &config,
                        &cli.rootfs,
                        planned_targets[0].clone(),
                        planner_opts,
                    )?
                } else {
                    planner::build_install_plan_for_targets(
                        &config,
                        &cli.rootfs,
                        &planned_targets,
                        planner_opts,
                    )?
                };
                print_plan_summary(&plan);
                execute_install_plan_with_child_commands(
                    &plan,
                    &cli.rootfs,
                    &config,
                    InstallPlanExecutionOptions {
                        no_flags: cli.no_flags,
                        cross_prefix: cli.cross_prefix.as_deref(),
                        clean: cli.clean,
                        dry_run: cli.dry_run,
                        confirm_installation: true,
                    },
                )?;
            }

            let mut ran_direct_install = false;
            for request in direct_requests {
                ran_direct_install |= run_direct_install_request(
                    DirectInstallOptions {
                        rootfs: &cli.rootfs,
                        no_deps: cli.no_deps,
                        no_flags: cli.no_flags,
                        cross_prefix: cli.cross_prefix.as_deref(),
                        clean: cli.clean,
                        dry_run: cli.dry_run,
                        lib32_only: cli.lib32_only,
                    },
                    &config,
                    request,
                )?;
            }
            if ran_direct_install {
                install::scripts::run_deferred_hooks_if_possible(&cli.rootfs)?;
            }

            if cli.clean && (ran_plan_mode || ran_direct_install) {
                clean_build_workspace(&config)?;
            }
        }
        Commands::Remove { package } => {
            ui::info(format!("Removing package: {}", package));
            let config = config::Config::for_rootfs(&cli.rootfs);
            let mut remove_lock = locking::open_lock(&config)?;
            let remove_lock_path = locking::lock_path(&config);
            let _remove_lock_guard =
                locking::try_write(&mut remove_lock, &remove_lock_path, "remove")?;
            let db_path = config.db_dir.join("packages.db");
            let removal_targets = vec![package.clone()];
            if !ui::prompt_package_action("removal", &removal_targets, true)? {
                anyhow::bail!("Aborted");
            }
            let affected_paths = db::get_package_files(&db_path, &package)?;
            install::hooks::run_transaction_hooks(
                &cli.rootfs,
                &install::hooks::HookExecutionContext {
                    phase: install::hooks::HookPhase::Pre,
                    operation: install::hooks::HookOperation::Remove,
                    package: &package,
                    affected_paths: &affected_paths,
                },
            )?;
            let script_dir = install::scripts::installed_scripts_dir(&cli.rootfs, &package);
            let _ = install::scripts::run_hook_if_present(
                &script_dir,
                install::scripts::Hook::PreRemove,
                &cli.rootfs,
                &package,
            )?;
            db::remove_package(&db_path, &package, &cli.rootfs)?;
            let post_remove = install::scripts::run_hook_if_present(
                &script_dir,
                install::scripts::Hook::PostRemove,
                &cli.rootfs,
                &package,
            );
            let cleanup_scripts = install::scripts::remove_installed_scripts(&cli.rootfs, &package);
            post_remove?;
            cleanup_scripts?;
            install::hooks::run_transaction_hooks(
                &cli.rootfs,
                &install::hooks::HookExecutionContext {
                    phase: install::hooks::HookPhase::Post,
                    operation: install::hooks::HookOperation::Remove,
                    package: &package,
                    affected_paths: &affected_paths,
                },
            )?;
            ui::success(format!("Successfully removed {}", package));
        }
        Commands::Build {
            spec_pos,
            spec,
            install,
        } => {
            warn_if_running_as_root_for_build("build", &cli.rootfs);
            let spec_path = spec.or(spec_pos).context("No spec file provided")?;
            ui::info(format!("Building package from: {}", spec_path.display()));
            let mut pkg_spec = package::PackageSpec::from_file(&spec_path)?;

            let config = config::Config::for_rootfs(&cli.rootfs);

            // Apply system overrides
            pkg_spec.apply_config(&config);

            // Ensure database directory exists
            std::fs::create_dir_all(&config.db_dir).with_context(|| {
                format!(
                    "Failed to create database directory: {}",
                    config.db_dir.display()
                )
            })?;
            let db_path = config.db_dir.join("packages.db");

            maybe_disable_tests_for_missing_deps(&mut pkg_spec, &db_path)?;

            // Check build dependencies
            if !cli.no_deps {
                deps::print_dep_status(&pkg_spec, &db_path)?;
                let missing = merge_missing_dependencies(
                    deps::check_build_deps(&pkg_spec, &db_path)?,
                    deps::check_runtime_deps(&pkg_spec, &db_path)?,
                );
                if !missing.is_empty() {
                    ui::warn(format!("Missing dependencies: {}", missing.join(", ")));
                    let local_sibling_root = spec_path
                        .parent()
                        .and_then(|p| p.parent())
                        .map(Path::to_path_buf);
                    let dep_plan = planner::build_dependency_install_plan(
                        &config,
                        &cli.rootfs,
                        &missing,
                        planner::PlannerOptions {
                            assume_yes: cli.yes,
                            prefer_binary: config.repo_settings.prefer_binary,
                            local_sibling_root,
                        },
                    )?;
                    print_plan_summary(&dep_plan);
                    if !ui::prompt_package_action("dependency installation", &missing, true)? {
                        anyhow::bail!("Aborted");
                    }
                    if cli.dry_run {
                        ui::info("Dry run enabled, stopping before dependency installation/build.");
                        return Ok(());
                    }
                    execute_install_plan_with_child_commands(
                        &dep_plan,
                        &cli.rootfs,
                        &config,
                        InstallPlanExecutionOptions {
                            no_flags: cli.no_flags,
                            cross_prefix: cli.cross_prefix.as_deref(),
                            clean: cli.clean,
                            dry_run: cli.dry_run,
                            confirm_installation: false,
                        },
                    )?;
                }
                deps::require_build_deps(&pkg_spec, &db_path)?;
                deps::require_runtime_deps(&pkg_spec, &db_path)?;
            } else if cli.dry_run {
                ui::info("Dry run enabled, stopping before build.");
                return Ok(());
            }

            let mut build_lock = locking::open_lock(&config)?;
            let build_lock_path = locking::lock_path(&config);
            let _build_lock_guard = locking::try_write(&mut build_lock, &build_lock_path, "build")?;

            let build_targets = vec![format!(
                "{} v{}-{}",
                pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
            )];
            if !ui::prompt_package_action("build", &build_targets, true)? {
                anyhow::bail!("Aborted");
            }

            if cli.dry_run {
                ui::info("Dry run enabled, stopping before fetch/build.");
                return Ok(());
            }

            // TODO(snapper): create pre-build snapshot before fetch/build starts.
            let src_dir = source::prepare(&pkg_spec, &config.cache_dir, &config.build_dir)?;

            let destdir = config
                .build_dir
                .join("destdir")
                .join(&pkg_spec.package.name);
            // Build with optional cross-compilation
            let cross_config = cli
                .cross_prefix
                .as_ref()
                .map(|p| cross::CrossConfig::from_prefix(p))
                .transpose()?;
            if !cli.lib32_only {
                builder::build(
                    &pkg_spec,
                    &src_dir,
                    &destdir,
                    cross_config.as_ref(),
                    !cli.no_flags,
                )?;
            }

            if !cli.lib32_only {
                staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;
                install::scripts::stage_scripts_from_spec_dir(&pkg_spec, &destdir)?;
                staging::process(&destdir, &pkg_spec)?;
            }

            // Create package archive(s) — support multiple outputs from a single spec.
            let arch = cli
                .cross_prefix
                .as_deref()
                .unwrap_or(std::env::consts::ARCH);

            let mut created_files = Vec::new();
            if !cli.lib32_only {
                for out in pkg_spec.outputs() {
                    let mut spec_for_out = pkg_spec.clone();
                    let output_name = out.name.clone();
                    spec_for_out.package = out;
                    spec_for_out.alternatives = pkg_spec.alternatives_for_output(&output_name);
                    spec_for_out.dependencies = pkg_spec.dependencies_for_output(&output_name);
                    let out_destdir =
                        output_destdir_for(&destdir, &pkg_spec.package.name, &output_name);
                    let packager =
                        package::Packager::new(spec_for_out.clone(), out_destdir, config.clone());
                    let pkg_file = packager.create_package(Path::new("."), arch)?;
                    if let Some(sig_path) =
                        signing::auto_sign_zst_file_detached(&cli.rootfs, &pkg_file)?
                    {
                        ui::success(format!(
                            "Created detached signature: {}",
                            sig_path.display()
                        ));
                    }
                    created_files.push(pkg_file);
                }
            }

            let mut lib32_install_bundle: Option<(package::PackageSpec, PathBuf)> = None;
            if let Some((lib32_spec, lib32_destdir)) = build_lib32_companion_package(
                &pkg_spec,
                &src_dir,
                &config,
                cross_config.as_ref(),
                !cli.no_flags,
                cli.lib32_only,
            )? {
                let lib32_arch = lib32_arch_name(arch);
                let packager = package::Packager::new(
                    lib32_spec.clone(),
                    lib32_destdir.clone(),
                    config.clone(),
                );
                let pkg_file = packager.create_package(Path::new("."), &lib32_arch)?;
                if let Some(sig_path) =
                    signing::auto_sign_zst_file_detached(&cli.rootfs, &pkg_file)?
                {
                    ui::success(format!(
                        "Created detached signature: {}",
                        sig_path.display()
                    ));
                }
                created_files.push(pkg_file);
                lib32_install_bundle = Some((lib32_spec, lib32_destdir));
            }

            for f in &created_files {
                ui::success(format!("Build complete. Package created: {}", f.display()));
            }
            // TODO(snapper): create post-build snapshot after package build completes.

            if install {
                let mut install_targets = Vec::new();
                if !cli.lib32_only {
                    for out in pkg_spec.outputs() {
                        install_targets
                            .push(format!("{} v{}-{}", out.name, out.version, out.revision));
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
                    if should_delegate_live_rootfs_installs(&cli.rootfs) {
                        for archive in &created_files {
                            run_child_install_command(
                                archive,
                                &cli.rootfs,
                                InstallPlanExecutionOptions {
                                    no_flags: cli.no_flags,
                                    cross_prefix: cli.cross_prefix.as_deref(),
                                    clean: cli.clean,
                                    dry_run: cli.dry_run,
                                    confirm_installation: false,
                                },
                            )?;
                        }
                        if cli.clean {
                            clean_build_workspace(&config)?;
                        }
                        return Ok(());
                    }

                    let mut transaction_plans = Vec::new();
                    if !cli.lib32_only {
                        let output_plans =
                            plan_package_outputs_for_install(&pkg_spec, &destdir, &config)?;
                        transaction_plans.extend(output_plans);
                    }
                    if let Some((lib32_spec, lib32_destdir)) = &lib32_install_bundle {
                        let staged = plan_staged_install(lib32_spec, lib32_destdir, &config)?;
                        transaction_plans.push(PlannedPackageInstall {
                            spec: lib32_spec.clone(),
                            destdir: lib32_destdir.clone(),
                            staged,
                        });
                    }

                    run_transaction_hooks_for_plans(
                        &cli.rootfs,
                        install::hooks::HookPhase::Pre,
                        &transaction_plans,
                    )?;
                    let installed = install_planned_packages_to_rootfs(
                        &transaction_plans,
                        &cli.rootfs,
                        &config,
                    )?;
                    for pkg in installed {
                        ui::success(format!(
                            "Successfully installed {} v{}",
                            pkg.name, pkg.version
                        ));
                        // TODO(snapper): create post-install snapshot after --install commit succeeds.
                    }
                    run_transaction_hooks_for_plans(
                        &cli.rootfs,
                        install::hooks::HookPhase::Post,
                        &transaction_plans,
                    )?;

                    install::scripts::run_deferred_hooks_if_possible(&cli.rootfs)?;
                } else {
                    if !cli.lib32_only {
                        for out in pkg_spec.outputs() {
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

            if cli.clean {
                clean_build_workspace(&config)?;
            }
        }
        Commands::Info { package } => {
            // Try as file first, then as installed package name
            let path = PathBuf::from(&package);
            if path.exists() {
                let config = config::Config::for_rootfs(&cli.rootfs);
                let info_lock = locking::open_lock(&config)?;
                let info_lock_path = locking::lock_path(&config);
                let _info_lock_guard = locking::try_read(&info_lock, &info_lock_path, "info")?;
                let pkg_spec = package::PackageSpec::from_file(&path)?;
                println!("{}", pkg_spec);

                // Also show dependency status
                let db_path = config.db_dir.join("packages.db");
                deps::print_dep_status(&pkg_spec, &db_path)?;
            } else {
                let config = config::Config::for_rootfs(&cli.rootfs);
                let info_lock = locking::open_lock(&config)?;
                let info_lock_path = locking::lock_path(&config);
                let _info_lock_guard = locking::try_read(&info_lock, &info_lock_path, "info")?;
                let db_path = config.db_dir.join("packages.db");
                db::show_package_info(&db_path, &package)?;
            }
        }
        Commands::Search { query, files } => {
            let config = config::Config::for_rootfs(&cli.rootfs);
            let search_lock = locking::open_lock(&config)?;
            let search_lock_path = locking::lock_path(&config);
            let _search_lock_guard = locking::try_read(&search_lock, &search_lock_path, "search")?;
            run_search_command(&query, files, &config, &cli.rootfs)?;
        }
        Commands::Owns { path } => {
            let config = config::Config::for_rootfs(&cli.rootfs);
            let owns_lock = locking::open_lock(&config)?;
            let owns_lock_path = locking::lock_path(&config);
            let _owns_lock_guard = locking::try_read(&owns_lock, &owns_lock_path, "owns")?;
            let db_path = config.db_dir.join("packages.db");
            match db::owns_path(&db_path, &path)? {
                Some(owner) => ui::info(format!("{} is owned by {}", path.display(), owner)),
                None => ui::warn(format!("No installed package owns {}", path.display())),
            }
        }
        Commands::List => {
            let config = config::Config::for_rootfs(&cli.rootfs);
            let list_lock = locking::open_lock(&config)?;
            let list_lock_path = locking::lock_path(&config);
            let _list_lock_guard = locking::try_read(&list_lock, &list_lock_path, "list")?;
            let db_path = config.db_dir.join("packages.db");
            db::list_packages(&db_path)?;
        }
        Commands::Sign { files } => {
            let sig_paths = signing::sign_zst_files_detached(&cli.rootfs, &files)?;
            for sig_path in sig_paths {
                ui::success(format!(
                    "Created detached signature: {}",
                    sig_path.display()
                ));
            }
        }
        Commands::Repo { command } => match command {
            RepoCommands::Create { dir } => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo create")?;
                let repo = db::repo::RepoManager::new(dir);
                let db_path = repo.create_repo_db()?;
                if let Some(sig_path) = signing::auto_sign_zst_file_detached(&cli.rootfs, &db_path)?
                {
                    ui::success(format!(
                        "Created detached signature: {}",
                        sig_path.display()
                    ));
                }
                ui::success(format!(
                    "Created repository database: {}",
                    db_path.display()
                ));
            }
            RepoCommands::Sync => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo sync")?;
                // Only root may run sync
                if !crate::fakeroot::is_root() {
                    anyhow::bail!("The 'repo sync' command must be run as root");
                }
                let config = cfg;
                let mirrors = config.enabled_source_mirror_map();
                if mirrors.is_empty() {
                    ui::info("No enabled source repos configured");
                } else {
                    db::repo::sync_mirrors(&config.repo_clone_dir, &mirrors)?;
                    ui::success(format!(
                        "Source repos synchronized into {}",
                        config.repo_clone_dir.display()
                    ));
                }
            }
            RepoCommands::Update { name } => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo update")?;
                if !crate::fakeroot::is_root() {
                    anyhow::bail!("The 'repo update' command must be run as root");
                }
                let config = cfg;
                let mirrors = selected_source_repos(&config, name.as_deref())?;
                if mirrors.is_empty() {
                    ui::info("No enabled source repos configured");
                } else {
                    db::repo::sync_mirrors(&config.repo_clone_dir, &mirrors)?;
                    if let Some(name) = name {
                        ui::success(format!(
                            "Source repo '{}' synchronized into {}",
                            name,
                            config.repo_clone_dir.display()
                        ));
                    } else {
                        ui::success(format!(
                            "Source repos synchronized into {}",
                            config.repo_clone_dir.display()
                        ));
                    }
                }
            }
            RepoCommands::Index { dir, subdirs } => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo index")?;
                let stats = index::create_source_repo_index(&dir, &subdirs).with_context(|| {
                    format!("Failed to create source index for {}", dir.display())
                })?;
                ui::success(format!(
                    "Wrote source index: {}",
                    stats.index_path.display()
                ));
                ui::info(format!(
                    "Indexed {} spec(s) from {} TOML file(s): package rows={} provides rows={} ignored_toml={}",
                    stats.specs_indexed,
                    stats.toml_files_scanned,
                    stats.package_rows,
                    stats.provides_rows,
                    stats.ignored_toml_files
                ));
            }
            RepoCommands::List => {
                let config = config::Config::for_rootfs(&cli.rootfs);
                let repo_lock = locking::open_lock(&config)?;
                let repo_lock_path = locking::lock_path(&config);
                let _repo_lock_guard = locking::try_read(&repo_lock, &repo_lock_path, "repo list")?;
                print_repo_list(&config);
            }
            RepoCommands::Add {
                name,
                url,
                kind,
                subdirs,
                priority,
                disabled,
                arch,
                repo_db,
                allow_unsigned,
            } => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo add")?;
                let mut repos = config::load_repos_config_file(&cli.rootfs)?;
                match kind {
                    RepoKindArg::Source => {
                        if let Some(existing) = repos.source.get(&name)
                            && (existing.url != url
                                || existing.subdirs != subdirs
                                || existing.priority != priority
                                || existing.enabled == disabled)
                            && !ui::prompt_yes_no(
                                &format!("Source repo '{}' already exists. Overwrite it?", name),
                                false,
                            )?
                        {
                            anyhow::bail!("Aborted");
                        }
                        repos.source.insert(
                            name.clone(),
                            config::SourceRepo {
                                url,
                                enabled: !disabled,
                                priority,
                                subdirs,
                            },
                        );
                    }
                    RepoKindArg::Binary => {
                        let arch_name = arch.unwrap_or_else(|| std::env::consts::ARCH.to_string());
                        let mut candidate = repos.binary.get(&name).cloned().unwrap_or_default();
                        candidate.url = url.clone();
                        candidate.enabled = !disabled;
                        candidate.priority = priority;
                        candidate.repo_db = repo_db.clone();
                        candidate.allow_unsigned = allow_unsigned;
                        candidate.arch.entry(arch_name.clone()).or_default().enabled = true;

                        if let Some(existing) = repos.binary.get(&name)
                            && (*existing != candidate)
                            && !ui::prompt_yes_no(
                                &format!("Binary repo '{}' already exists. Overwrite it?", name),
                                false,
                            )?
                        {
                            anyhow::bail!("Aborted");
                        }
                        repos.binary.insert(name.clone(), candidate);
                    }
                }
                let path = config::save_repos_config_file(&cli.rootfs, &repos)?;
                ui::success(format!(
                    "Saved {} repo '{}' to {}",
                    repo_kind_label(kind),
                    name,
                    path.display()
                ));
            }
            RepoCommands::Remove { name, kind } => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo remove")?;
                let mut repos = config::load_repos_config_file(&cli.rootfs)?;
                let kind = resolve_repo_kind_for_name(&repos, &name, kind)?;
                if !ui::prompt_yes_no(
                    &format!("Remove {} repo '{}'?", repo_kind_label(kind), name),
                    false,
                )? {
                    anyhow::bail!("Aborted");
                }

                match kind {
                    RepoKindArg::Source => {
                        repos.source.remove(&name);
                    }
                    RepoKindArg::Binary => {
                        repos.binary.remove(&name);
                    }
                }
                let path = config::save_repos_config_file(&cli.rootfs, &repos)?;
                ui::success(format!(
                    "Removed {} repo '{}' from {}",
                    repo_kind_label(kind),
                    name,
                    path.display()
                ));
            }
            RepoCommands::Enable { name, kind } => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo enable")?;
                let mut repos = config::load_repos_config_file(&cli.rootfs)?;
                let kind = resolve_repo_kind_for_name(&repos, &name, kind)?;
                match kind {
                    RepoKindArg::Source => {
                        let repo = repos
                            .source
                            .get_mut(&name)
                            .with_context(|| format!("Source repo '{}' not found", name))?;
                        if repo.enabled {
                            ui::info(format!("Source repo '{}' is already enabled", name));
                        } else {
                            repo.enabled = true;
                            let path = config::save_repos_config_file(&cli.rootfs, &repos)?;
                            ui::success(format!(
                                "Enabled source repo '{}' in {}",
                                name,
                                path.display()
                            ));
                        }
                    }
                    RepoKindArg::Binary => {
                        let repo = repos
                            .binary
                            .get_mut(&name)
                            .with_context(|| format!("Binary repo '{}' not found", name))?;
                        if repo.enabled {
                            ui::info(format!("Binary repo '{}' is already enabled", name));
                        } else {
                            repo.enabled = true;
                            let path = config::save_repos_config_file(&cli.rootfs, &repos)?;
                            ui::success(format!(
                                "Enabled binary repo '{}' in {}",
                                name,
                                path.display()
                            ));
                        }
                    }
                }
            }
            RepoCommands::Disable { name, kind } => {
                let cfg = config::Config::for_rootfs(&cli.rootfs);
                let mut repo_lock = locking::open_lock(&cfg)?;
                let repo_lock_path = locking::lock_path(&cfg);
                let _repo_lock_guard =
                    locking::try_write(&mut repo_lock, &repo_lock_path, "repo disable")?;
                let mut repos = config::load_repos_config_file(&cli.rootfs)?;
                let kind = resolve_repo_kind_for_name(&repos, &name, kind)?;
                if !ui::prompt_yes_no(
                    &format!("Disable {} repo '{}'?", repo_kind_label(kind), name),
                    true,
                )? {
                    anyhow::bail!("Aborted");
                }
                match kind {
                    RepoKindArg::Source => {
                        let repo = repos
                            .source
                            .get_mut(&name)
                            .with_context(|| format!("Source repo '{}' not found", name))?;
                        repo.enabled = false;
                    }
                    RepoKindArg::Binary => {
                        let repo = repos
                            .binary
                            .get_mut(&name)
                            .with_context(|| format!("Binary repo '{}' not found", name))?;
                        repo.enabled = false;
                    }
                }
                let path = config::save_repos_config_file(&cli.rootfs, &repos)?;
                ui::success(format!(
                    "Disabled {} repo '{}' in {}",
                    repo_kind_label(kind),
                    name,
                    path.display()
                ));
            }
            RepoCommands::Owns { path } => {
                let config = config::Config::for_rootfs(&cli.rootfs);
                let repo_lock = locking::open_lock(&config)?;
                let repo_lock_path = locking::lock_path(&config);
                let _repo_lock_guard = locking::try_read(&repo_lock, &repo_lock_path, "repo owns")?;
                let host_arch = std::env::consts::ARCH;
                let mut any = false;
                let mut binary_repos: Vec<_> = config
                    .binary_repos
                    .iter()
                    .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
                    .collect();
                binary_repos
                    .sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

                for (name, repo) in binary_repos {
                    match db::repo::binary_repo_owns_path(
                        name,
                        repo,
                        &cli.rootfs,
                        &config.package_cache_dir,
                        &path.to_string_lossy(),
                    ) {
                        Ok(hits) => {
                            for hit in hits {
                                any = true;
                                ui::info(format!(
                                    "{} [binary:{}] {}-{} size={} owns={}",
                                    hit.package_name,
                                    hit.repo_name,
                                    hit.version,
                                    hit.revision,
                                    hit.size,
                                    hit.path
                                ));
                            }
                        }
                        Err(e) => crate::log_warn!("Binary repo '{}': {}", name, e),
                    }
                }
                if !any {
                    ui::warn(format!(
                        "No binary repo metadata entry owns {}",
                        path.display()
                    ));
                }
            }
            RepoCommands::Status => {
                let config = config::Config::for_rootfs(&cli.rootfs);
                let repo_lock = locking::open_lock(&config)?;
                let repo_lock_path = locking::lock_path(&config);
                let _repo_lock_guard =
                    locking::try_read(&repo_lock, &repo_lock_path, "repo status")?;
                let mirrors = config.enabled_source_mirror_map();
                if mirrors.is_empty() {
                    ui::info("No enabled source repos configured");
                } else {
                    db::repo::mirrors_status(&config.repo_clone_dir, &mirrors)?;
                }
                if config.binary_repos.is_empty() {
                    ui::info("No binary repos configured");
                } else {
                    ui::info("Binary repo configuration:");
                    let host_arch = std::env::consts::ARCH;
                    for (name, repo) in &config.binary_repos {
                        let arch_keys = if repo.arch.is_empty() {
                            "(any)".to_string()
                        } else {
                            repo.arch.keys().cloned().collect::<Vec<_>>().join(",")
                        };
                        ui::info(format!(
                            "  {} [{}] url={} repo_db={} arches={} host_match={}",
                            name,
                            if repo.enabled { "enabled" } else { "disabled" },
                            repo.url,
                            repo.repo_db,
                            arch_keys,
                            if repo.supports_arch(host_arch) {
                                "yes"
                            } else {
                                "no"
                            }
                        ));
                    }
                }
            }
        },
        Commands::GenerateArtifacts { out_dir } => {
            cli_assets::generate_cli_assets(&out_dir)?;
            ui::success(format!("Generated CLI assets in {}", out_dir.display()));
        }
        Commands::Config => {
            let config = config::Config::for_rootfs(&cli.rootfs);
            let config_lock = locking::open_lock(&config)?;
            let config_lock_path = locking::lock_path(&config);
            let _config_lock_guard = locking::try_read(&config_lock, &config_lock_path, "config")?;
            println!("Cache Directory: {}", config.cache_dir.display());
            println!(
                "Package Cache Directory: {}",
                config.package_cache_dir.display()
            );
            println!("Build Directory: {}", config.build_dir.display());
            println!("Database Directory: {}", config.db_dir.display());
            println!("Repo Clone Directory: {}", config.repo_clone_dir.display());
            println!(
                "Configured Repos: {} source, {} binary",
                config.source_repos.len(),
                config.binary_repos.len()
            );
            println!("\nBuild Overrides: {}", config.build_overrides);
            println!("Package Overrides: {}", config.package_overrides);
            if !config.appends.is_empty() {
                println!("\nAppends:");
                for (k, v) in &config.appends {
                    println!("  {} = {:?}", k, v);
                }
            }
        }
        Commands::MakeSpec { output } => {
            let spec = package::create_interactive()?;
            // Produce a minimal TOML for interactive-created specs (omit defaults)
            let toml_string = package::spec_to_minimal_toml(&spec)?;

            let output_path =
                output.unwrap_or_else(|| PathBuf::from(format!("{}.toml", spec.package.name)));

            if output_path.exists() {
                ui::warn(format!("File {} already exists.", output_path.display()));
                if !ui::prompt_yes_no("Overwrite it?", false)? {
                    anyhow::bail!("Aborted");
                }
            }

            fs::write(&output_path, toml_string)?;
            ui::success(format!(
                "Package specification saved to {}",
                output_path.display()
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::io::Write;

    #[test]
    fn clean_build_workspace_removes_build_and_source_cache_dirs() -> Result<()> {
        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("tmp/build");
        cfg.cache_dir = rootfs.path().join("tmp/sources");

        fs::create_dir_all(&cfg.build_dir)
            .with_context(|| format!("Failed to create {}", cfg.build_dir.display()))?;
        fs::create_dir_all(&cfg.cache_dir)
            .with_context(|| format!("Failed to create {}", cfg.cache_dir.display()))?;

        let mut build_file = fs::File::create(cfg.build_dir.join("artifact.txt"))?;
        build_file.write_all(b"build data")?;
        build_file.flush()?;

        let mut source_file = fs::File::create(cfg.cache_dir.join("source.tar.zst"))?;
        source_file.write_all(b"source data")?;
        source_file.flush()?;

        clean_build_workspace(&cfg)?;

        assert!(!cfg.build_dir.exists());
        assert!(!cfg.cache_dir.exists());
        Ok(())
    }

    #[test]
    fn clean_build_workspace_noops_when_dirs_are_missing() -> Result<()> {
        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("tmp/build");
        cfg.cache_dir = rootfs.path().join("tmp/sources");

        clean_build_workspace(&cfg)?;

        assert!(!cfg.build_dir.exists());
        assert!(!cfg.cache_dir.exists());
        Ok(())
    }

    #[test]
    fn binary_install_path_uses_repo_record_metadata_without_archive_metadata() -> Result<()> {
        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
        let archive_path = pkg_dir.path().join("pkg-1.0-1-x86_64.depot.pkg.tar.zst");

        // Build an archive that intentionally does not contain .metadata.toml.
        let file = fs::File::create(&archive_path)
            .with_context(|| format!("Failed to create {}", archive_path.display()))?;
        let encoder =
            zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
        let mut tar = tar::Builder::new(encoder);
        let payload = b"hello";
        let mut header = tar::Header::new_gnu();
        header.set_path("usr/bin/hello").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append(&header, &payload[..]).unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("var/cache/depot/build");
        cfg.db_dir = rootfs.path().join("var/lib/depot");

        let staged = extract_package_archive_to_staging(&cfg, &archive_path)?;
        let record = db::repo::BinaryRepoPackageRecord {
            repo_name: "core".into(),
            name: "pkg".into(),
            version: "1.0".into(),
            revision: 1,
            filename: archive_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default()
                .to_string(),
            size: payload.len() as u64,
            sha256: String::new(),
            sha512: String::new(),
            description: Some("test package".into()),
            homepage: Some("https://example.test".into()),
            license: Some("MIT".into()),
            provides: vec!["pkg-virtual".into()],
            runtime_dependencies: vec!["glibc".into()],
            optional_dependencies: vec!["manpages".into()],
        };
        let spec = package_spec_from_repo_record(&record);
        let installed =
            install_package_outputs_to_rootfs(&spec, staged.path(), rootfs.path(), &cfg)?;

        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "pkg");
        assert!(rootfs.path().join("usr/bin/hello").exists());

        let db_path = cfg.db_dir.join("packages.db");
        assert_eq!(
            db::get_package_version(&db_path, "pkg")?,
            Some("1.0".into())
        );
        Ok(())
    }

    #[test]
    fn binary_archive_staging_uses_config_build_dir_instead_of_process_tmpdir() -> Result<()> {
        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
        let archive_path = pkg_dir.path().join("pkg-1.0-1-x86_64.depot.pkg.tar.zst");

        let file = fs::File::create(&archive_path)
            .with_context(|| format!("Failed to create {}", archive_path.display()))?;
        let encoder =
            zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
        let mut tar = tar::Builder::new(encoder);
        let payload = b"hello";
        let mut header = tar::Header::new_gnu();
        header.set_path("usr/bin/hello").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append(&header, &payload[..]).unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("var/cache/depot/build");

        let blocked_tmp = rootfs.path().join("blocked-tmp");
        fs::create_dir_all(&blocked_tmp)
            .with_context(|| format!("Failed to create {}", blocked_tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&blocked_tmp, fs::Permissions::from_mode(0o555))
                .with_context(|| format!("Failed to chmod {}", blocked_tmp.display()))?;
        }

        let previous_tmpdir = std::env::var_os("TMPDIR");
        unsafe {
            std::env::set_var("TMPDIR", &blocked_tmp);
        }
        let staged = extract_package_archive_to_staging(&cfg, &archive_path)?;
        unsafe {
            if let Some(value) = previous_tmpdir {
                std::env::set_var("TMPDIR", value);
            } else {
                std::env::remove_var("TMPDIR");
            }
        }

        assert!(staged.path().starts_with(staging_temp_root(&cfg)));
        assert!(staged.path().join("usr/bin/hello").exists());
        Ok(())
    }

    #[test]
    fn merge_missing_dependencies_preserves_order_and_uniqueness() {
        let merged = merge_missing_dependencies(
            vec!["make".into(), "pkgconf".into(), "glibc".into()],
            vec![
                "glibc".into(),
                "openssl".into(),
                "pkgconf".into(),
                "zlib".into(),
            ],
        );
        assert_eq!(merged, vec!["make", "pkgconf", "glibc", "openssl", "zlib"]);
    }

    #[test]
    fn rootfs_is_system_root_detects_live_rootfs() {
        assert!(rootfs_is_system_root(Path::new("/")));
        assert!(!rootfs_is_system_root(Path::new("/tmp/depot-test-rootfs")));
    }

    #[test]
    fn command_requires_live_root_only_for_install_and_remove() {
        assert!(command_requires_live_root(&Commands::Install {
            spec_or_archive: vec![PathBuf::from("foo")],
            spec: None,
        }));
        assert!(command_requires_live_root(&Commands::Remove {
            package: "foo".to_string(),
        }));
        assert!(!command_requires_live_root(&Commands::Build {
            spec_pos: Some(PathBuf::from("foo.toml")),
            spec: None,
            install: false,
        }));
        assert!(!command_requires_live_root(&Commands::Search {
            query: "foo".to_string(),
            files: false,
        }));
    }

    #[test]
    fn should_delegate_live_rootfs_installs_only_for_live_root_when_non_root() {
        assert_eq!(
            should_delegate_live_rootfs_installs(Path::new("/")),
            !crate::fakeroot::is_root()
        );
        assert!(!should_delegate_live_rootfs_installs(Path::new(
            "/tmp/depot-test-rootfs"
        )));
    }
}
