use crate::cli::{Cli, Commands, RepoCommands, RepoKindArg};
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
use url::Url;
use walkdir::WalkDir;

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
        Commands::Install { .. } | Commands::Remove { .. } | Commands::Update { .. }
    )
}

fn should_reexec_with_sudo(cli: &Cli) -> bool {
    !crate::fakeroot::is_root()
        && rootfs_is_system_root(&cli.rootfs)
        && command_requires_live_root(&cli.command)
}

fn should_delegate_live_rootfs_installs(rootfs: &Path) -> bool {
    !crate::fakeroot::is_root() && rootfs_is_system_root(rootfs)
}

fn install_test_deps_enabled(cli_test_deps: bool, config: &config::Config) -> bool {
    cli_test_deps || config.install_test_deps
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

#[derive(Clone, Copy)]
struct ChildInstallCommandOptions<'a> {
    no_deps: bool,
    assume_yes: bool,
    no_flags: bool,
    cross_prefix: Option<&'a str>,
    clean: bool,
    install_test_deps: bool,
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
    if options.install_test_deps {
        cmd.arg("--test-deps");
    }
    cmd.arg("install");
    cmd.args(install_requests);
    if let Some(dep_chain) = options.dep_chain {
        cmd.env("DEPOT_DEPCHAIN", dep_chain);
    }

    let status = cmd.status().with_context(|| {
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
            install_test_deps: options.install_test_deps,
            dep_chain: None,
        },
    )
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

fn package_spec_from_archive_metadata(metadata: &toml::Value) -> package::PackageSpec {
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
            license: parse_licenses_from_toml(metadata),
        },
        packages: Vec::new(),
        alternatives: package::Alternatives::default(),
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: package::Build {
            build_type: package::BuildType::Bin,
            flags: package::BuildFlags {
                keep: parse_keep_list(metadata),
                ..package::BuildFlags::default()
            },
        },
        dependencies: package::Dependencies {
            build: Vec::new(),
            runtime: parse_dependency_list(metadata, "runtime"),
            test: Vec::new(),
            optional: parse_dependency_list(metadata, "optional"),
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
    if let Some(conflicts) = metadata.get("conflicts").and_then(|v| v.as_array()) {
        spec.alternatives.conflicts = conflicts
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }

    spec
}

fn load_package_spec_from_staging(staged_dir: &Path) -> Result<package::PackageSpec> {
    let metadata_path = staged_dir.join(".metadata.toml");
    let metadata_content = fs::read_to_string(&metadata_path)
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    let metadata: toml::Value = toml::from_str(&metadata_content)
        .with_context(|| format!("Failed to parse {}", metadata_path.display()))?;
    Ok(package_spec_from_archive_metadata(&metadata))
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
            conflicts: record.conflicts.clone(),
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

fn load_package_spec_from_staging_or_repo_record(
    staged_dir: &Path,
    record: &db::repo::BinaryRepoPackageRecord,
) -> Result<package::PackageSpec> {
    let metadata_path = staged_dir.join(".metadata.toml");
    if metadata_path.exists() {
        load_package_spec_from_staging(staged_dir)
    } else {
        Ok(package_spec_from_repo_record(record))
    }
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
    archive.set_preserve_permissions(true);
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

    Ok((package_spec_from_archive_metadata(&metadata), tmp_dir))
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
    archive.set_preserve_permissions(true);
    archive.unpack(&extract_dir).with_context(|| {
        format!(
            "Failed to extract package archive {} into {}",
            archive_path.display(),
            extract_dir.display()
        )
    })?;
    Ok(tmp_dir)
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

fn should_install_test_deps(pkg_spec: &package::PackageSpec, install_test_deps: bool) -> bool {
    install_test_deps && !pkg_spec.build.flags.skip_tests && !pkg_spec.dependencies.test.is_empty()
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
    rootfs: &Path,
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

    let db_path = config.installed_db_path(rootfs);
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
    let plans = plan_package_outputs_for_install(pkg_spec, destdir, rootfs, config)?;
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

fn scan_package_specs(dir: &Path) -> Result<Vec<PathBuf>> {
    let root = dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve scan root {}", dir.display()))?;
    if !root.is_dir() {
        anyhow::bail!("Scan root is not a directory: {}", root.display());
    }

    let mut specs = Vec::new();
    for entry in WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"))
    {
        let entry = entry.with_context(|| format!("Failed to walk {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let candidate = path.to_path_buf();
        if package::PackageSpec::from_file(&candidate).is_ok() {
            specs.push(candidate);
        }
    }
    specs.sort();
    Ok(specs)
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

#[derive(Clone, Copy)]
struct UpdateCommandOptions<'a> {
    rootfs: &'a Path,
    no_deps: bool,
    no_flags: bool,
    cross_prefix: Option<&'a str>,
    clean: bool,
    dry_run: bool,
    assume_yes: bool,
    install_test_deps: bool,
}

#[derive(Debug, Clone)]
enum UpdateOrigin {
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
struct UpdateCandidate {
    package: String,
    installed_version: String,
    installed_revision: u32,
    installed_completed_at: Option<i64>,
    candidate_version: String,
    candidate_revision: u32,
    candidate_completed_at: Option<i64>,
    runtime_dependencies: Vec<String>,
    provides: Vec<String>,
    conflicts: Vec<String>,
    repo_priority: i32,
    origin: UpdateOrigin,
}

#[derive(Debug, Clone)]
struct SourceUpdateCandidate {
    repo_name: String,
    repo_priority: i32,
    path: PathBuf,
    completed_at: Option<i64>,
    spec: package::PackageSpec,
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

fn compare_completed_at(left: Option<i64>, right: Option<i64>) -> Ordering {
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

fn update_candidate_is_preferred(
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

fn sync_source_repositories_for_update(config: &config::Config) -> Result<()> {
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

fn collect_best_source_update_candidates(
    config: &config::Config,
    target_names: &HashSet<String>,
) -> Result<HashMap<String, SourceUpdateCandidate>> {
    let mut best: HashMap<String, SourceUpdateCandidate> = HashMap::new();

    for (repo_name, repo_priority, root) in configured_source_scan_roots(config) {
        if !root.exists() {
            continue;
        }

        for spec_path in scan_package_specs(&root)? {
            let mut spec = package::PackageSpec::from_file(&spec_path)?;
            spec.apply_config(config);
            if !target_names.contains(&spec.package.name) {
                continue;
            }

            let candidate = SourceUpdateCandidate {
                repo_name: repo_name.clone(),
                repo_priority,
                path: spec_path.clone(),
                completed_at: path_modified_unix_timestamp(&spec_path)?,
                spec,
            };

            let replace = match best.get(&candidate.spec.package.name) {
                Some(current) => match compare_package_release(
                    &candidate.spec.package.version,
                    candidate.spec.package.revision,
                    &current.spec.package.version,
                    current.spec.package.revision,
                ) {
                    Ordering::Greater => true,
                    Ordering::Less => false,
                    Ordering::Equal => {
                        match compare_completed_at(candidate.completed_at, current.completed_at) {
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
                        }
                    }
                },
                None => true,
            };

            if replace {
                best.insert(candidate.spec.package.name.clone(), candidate);
            }
        }
    }

    Ok(best)
}

fn collect_best_binary_update_candidates(
    config: &config::Config,
    rootfs: &Path,
    target_names: &HashSet<String>,
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
            if !target_names.contains(&record.name) {
                continue;
            }

            let replace = match best.get(&record.name) {
                Some((current_priority, current)) => match compare_package_release(
                    &record.version,
                    record.revision,
                    &current.version,
                    current.revision,
                ) {
                    Ordering::Greater => true,
                    Ordering::Less => false,
                    Ordering::Equal => {
                        match compare_completed_at(record.completed_at, current.completed_at) {
                            Ordering::Greater => true,
                            Ordering::Less => false,
                            Ordering::Equal => {
                                if repo_cfg.priority != *current_priority {
                                    repo_cfg.priority < *current_priority
                                } else if repo_name != &current.repo_name {
                                    repo_name < &current.repo_name
                                } else {
                                    record.filename < current.filename
                                }
                            }
                        }
                    }
                },
                None => true,
            };

            if replace {
                best.insert(record.name.clone(), (repo_cfg.priority, record));
            }
        }
    }

    Ok(best)
}

fn select_update_candidate(
    installed: &db::InstalledPackageRecord,
    installed_completed_at: Option<i64>,
    source_candidates: &HashMap<String, SourceUpdateCandidate>,
    binary_candidates: &HashMap<String, (i32, db::repo::BinaryRepoPackageRecord)>,
    prefer_binary: bool,
) -> Option<UpdateCandidate> {
    let mut best: Option<UpdateCandidate> = None;

    if let Some(candidate) = source_candidates.get(&installed.name)
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
            package: installed.name.clone(),
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

    if let Some((repo_priority, record)) = binary_candidates.get(&installed.name)
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
            package: installed.name.clone(),
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

fn collect_update_candidates(
    config: &config::Config,
    rootfs: &Path,
    requested_packages: &[String],
) -> Result<Vec<UpdateCandidate>> {
    let db_path = config.installed_db_path(rootfs);
    let installed = db::list_installed_package_records(&db_path)?;
    if installed.is_empty() {
        return Ok(Vec::new());
    }

    let installed_by_name: HashMap<_, _> = installed
        .iter()
        .cloned()
        .map(|record| (record.name.clone(), record))
        .collect();

    let target_names: HashSet<String> = if requested_packages.is_empty() {
        installed_by_name.keys().cloned().collect()
    } else {
        requested_packages.iter().cloned().collect()
    };

    for package in requested_packages {
        if !installed_by_name.contains_key(package) {
            ui::warn(format!("Package '{}' is not installed; skipping", package));
        }
    }

    let source_candidates = if config.repo_settings.prefer_binary {
        HashMap::new()
    } else {
        collect_best_source_update_candidates(config, &target_names)?
    };
    let binary_candidates = collect_best_binary_update_candidates(config, rootfs, &target_names)?;

    let mut updates = Vec::new();
    let mut targets: Vec<_> = target_names.into_iter().collect();
    targets.sort();
    for target in targets {
        let Some(installed) = installed_by_name.get(&target) else {
            continue;
        };
        let installed_completed_at = installed_package_completed_at(installed, &db_path, rootfs)?;
        if let Some(candidate) = select_update_candidate(
            installed,
            installed_completed_at,
            &source_candidates,
            &binary_candidates,
            config.repo_settings.prefer_binary,
        ) {
            updates.push(candidate);
        }
    }

    Ok(updates)
}

fn collect_missing_update_dependencies(
    candidates: &[UpdateCandidate],
    db_path: &Path,
) -> Result<Vec<String>> {
    let mut planned_provides = HashSet::new();
    for candidate in candidates {
        planned_provides.insert(candidate.package.clone());
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

fn run_update_install_command(
    program: &Path,
    request: &Path,
    options: UpdateCommandOptions<'_>,
) -> Result<()> {
    run_install_command_with_program(
        program,
        &[request.to_path_buf()],
        options.rootfs,
        ChildInstallCommandOptions {
            no_deps: true,
            assume_yes: true,
            no_flags: options.no_flags,
            cross_prefix: options.cross_prefix,
            clean: options.clean,
            install_test_deps: options.install_test_deps,
            dep_chain: None,
        },
    )
}

fn run_update_command(
    packages: &[String],
    config: &config::Config,
    options: UpdateCommandOptions<'_>,
) -> Result<()> {
    sync_source_repositories_for_update(config)?;

    let updates = collect_update_candidates(config, options.rootfs, packages)?;
    if updates.is_empty() {
        ui::info("All installed packages are up to date.");
        return Ok(());
    }

    let targets: Vec<String> = updates
        .iter()
        .map(|candidate| {
            let summary = format!(
                "{} v{}-{} -> v{}-{}",
                candidate.package,
                candidate.installed_version,
                candidate.installed_revision,
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
        .map(|candidate| InstallConflictSubject {
            package: candidate.package.clone(),
            provides: candidate.provides.clone(),
            conflicts: candidate.conflicts.clone(),
        })
        .collect();
    validate_no_transaction_conflicts(&conflict_subjects)?;

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

    resolve_installed_conflicts_for_subjects(
        &conflict_subjects,
        options.rootfs,
        config,
        options.dry_run,
    )?;

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
            },
        )?;
        print_plan_summary(&dep_plan);

        if options.dry_run {
            ui::info("Dry run enabled, stopping before dependency installation/update.");
            return Ok(());
        }

        execute_install_plan_with_child_commands(
            &dep_plan,
            options.rootfs,
            config,
            InstallPlanExecutionOptions {
                no_flags: options.no_flags,
                cross_prefix: options.cross_prefix,
                clean: options.clean,
                dry_run: false,
                confirm_installation: false,
                install_test_deps: options.install_test_deps,
            },
        )?;
    } else if options.dry_run {
        ui::info("Dry run enabled, stopping before update.");
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to locate depot executable")?;
    for (idx, candidate) in updates.iter().enumerate() {
        ui::info(format!(
            "[{}/{}] updating {} v{}-{} -> v{}-{}",
            idx + 1,
            updates.len(),
            candidate.package,
            candidate.installed_version,
            candidate.installed_revision,
            candidate.candidate_version,
            candidate.candidate_revision
        ));
        let request = candidate_request_path(candidate, config, options.rootfs)?;
        run_update_install_command(&exe, &request, options)?;
    }

    if options.clean {
        clean_build_workspace(config)?;
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VersionPattern {
    prefix: String,
    suffix: String,
}

#[derive(Debug, Clone)]
enum CheckStatus {
    UpdateAvailable { latest: String, source: String },
    UpToDate { source: String },
    Unknown { reason: String },
}

fn strip_known_archive_suffixes(input: &str) -> &str {
    for suffix in [
        ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".txz", ".tbz2", ".zip", ".tar",
        ".git",
    ] {
        if let Some(stripped) = input.strip_suffix(suffix) {
            return stripped;
        }
    }
    input
}

fn extract_version_patterns(raw: &str) -> Vec<VersionPattern> {
    let mut patterns = HashSet::new();
    let mut start = 0usize;

    while let Some(rel_idx) = raw[start..].find("$version") {
        let idx = start + rel_idx;
        let prefix_start = raw[..idx]
            .rfind(['/', '#', '?', '&', '='])
            .map(|pos| pos + 1)
            .unwrap_or(0);
        let suffix_end = raw[idx + "$version".len()..]
            .find(['/', '#', '?', '&', '='])
            .map(|pos| idx + "$version".len() + pos)
            .unwrap_or(raw.len());
        let prefix = raw[prefix_start..idx].to_string();
        let suffix = strip_known_archive_suffixes(&raw[idx + "$version".len()..suffix_end]);
        patterns.insert(VersionPattern {
            prefix,
            suffix: suffix.to_string(),
        });
        start = idx + "$version".len();
    }

    let mut out: Vec<_> = patterns.into_iter().collect();
    out.sort_by(|a, b| {
        a.prefix
            .cmp(&b.prefix)
            .then_with(|| a.suffix.cmp(&b.suffix))
    });
    out
}

fn is_version_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-')
}

fn looks_like_version(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= 64
        && candidate.chars().all(is_version_char)
        && candidate.chars().any(|ch| ch.is_ascii_digit())
}

fn match_version_pattern<'a>(value: &'a str, pattern: &VersionPattern) -> Option<&'a str> {
    if !value.starts_with(&pattern.prefix) || !value.ends_with(&pattern.suffix) {
        return None;
    }

    let start = pattern.prefix.len();
    let end = value.len().saturating_sub(pattern.suffix.len());
    if end <= start {
        return None;
    }

    let candidate = &value[start..end];
    looks_like_version(candidate).then_some(candidate)
}

fn compare_version_fallback(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut li = 0usize;
    let mut ri = 0usize;

    while li < left.len() && ri < right.len() {
        let lch = left[li] as char;
        let rch = right[ri] as char;
        let l_digit = lch.is_ascii_digit();
        let r_digit = rch.is_ascii_digit();

        if l_digit && r_digit {
            let l_start = li;
            let r_start = ri;
            while li < left.len() && (left[li] as char).is_ascii_digit() {
                li += 1;
            }
            while ri < right.len() && (right[ri] as char).is_ascii_digit() {
                ri += 1;
            }

            let l_num = &left[l_start..li];
            let r_num = &right[r_start..ri];
            let l_trimmed = std::str::from_utf8(l_num)
                .unwrap_or_default()
                .trim_start_matches('0');
            let r_trimmed = std::str::from_utf8(r_num)
                .unwrap_or_default()
                .trim_start_matches('0');

            let l_cmp = if l_trimmed.is_empty() { "0" } else { l_trimmed };
            let r_cmp = if r_trimmed.is_empty() { "0" } else { r_trimmed };
            match l_cmp.len().cmp(&r_cmp.len()) {
                Ordering::Equal => match l_cmp.cmp(r_cmp) {
                    Ordering::Equal => {}
                    non_eq => return non_eq,
                },
                non_eq => return non_eq,
            }
            continue;
        }

        match lch.to_ascii_lowercase().cmp(&rch.to_ascii_lowercase()) {
            Ordering::Equal => {
                li += 1;
                ri += 1;
            }
            non_eq => return non_eq,
        }
    }

    left.len().cmp(&right.len())
}

fn compare_versions_for_updates(left: &str, right: &str) -> Ordering {
    let left_semver = left.trim_start_matches('v');
    let right_semver = right.trim_start_matches('v');
    if let (Ok(left), Ok(right)) = (
        semver::Version::parse(left_semver),
        semver::Version::parse(right_semver),
    ) {
        return left.cmp(&right);
    }

    if left.len() == 8
        && right.len() == 8
        && left.chars().all(|ch| ch.is_ascii_digit())
        && right.chars().all(|ch| ch.is_ascii_digit())
    {
        return left.cmp(right);
    }

    compare_version_fallback(left, right)
}

fn best_newer_version<'a>(
    current: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let mut best: Option<&str> = None;
    for candidate in candidates {
        if compare_versions_for_updates(candidate, current) != Ordering::Greater {
            continue;
        }
        if let Some(existing) = best
            && compare_versions_for_updates(candidate, existing) != Ordering::Greater
        {
            continue;
        }
        best = Some(candidate);
    }
    best.map(str::to_string)
}

fn remote_git_repository_from_source_url(expanded_url: &str) -> Option<String> {
    if let Some((base, _)) = source_git_url_parts(expanded_url) {
        return Some(base);
    }

    let parsed = Url::parse(expanded_url).ok()?;
    let host = parsed.host_str()?;
    let segments: Vec<_> = parsed.path_segments()?.collect();
    if segments.len() < 3 {
        return None;
    }
    let owner = segments[0];
    let repo = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    match segments[2] {
        "releases" | "archive" => {
            Some(format!("{}://{}/{owner}/{repo}.git", parsed.scheme(), host))
        }
        _ => None,
    }
}

fn source_git_url_parts(url: &str) -> Option<(String, String)> {
    if let Some((base, rev)) = url.split_once('#') {
        let lower = base.to_ascii_lowercase();
        let is_archive = lower.ends_with(".tar.gz")
            || lower.ends_with(".tgz")
            || lower.ends_with(".tar.xz")
            || lower.ends_with(".txz")
            || lower.ends_with(".tar.bz2")
            || lower.ends_with(".tbz2")
            || lower.ends_with(".zip")
            || lower.ends_with(".tar");
        if is_archive {
            return None;
        }
        let resolved_rev = if rev.trim().is_empty() { "HEAD" } else { rev };
        return Some((base.to_string(), resolved_rev.to_string()));
    }

    url.to_ascii_lowercase()
        .ends_with(".git")
        .then(|| (url.to_string(), "HEAD".to_string()))
}

fn list_remote_refs(url: &str) -> Result<Vec<String>> {
    let mut remote = git2::Remote::create_detached(url)
        .with_context(|| format!("Failed to create detached git remote for {}", url))?;
    remote
        .connect(Direction::Fetch)
        .with_context(|| format!("Failed to connect to git remote {}", url))?;
    let refs = remote
        .list()
        .with_context(|| format!("Failed to list refs for git remote {}", url))?
        .iter()
        .map(|head| head.name().trim_end_matches("^{}").to_string())
        .collect();
    remote.disconnect()?;
    Ok(refs)
}

fn candidate_versions_from_refs(refs: &[String], patterns: &[VersionPattern]) -> Vec<String> {
    let mut versions = HashSet::new();

    for name in refs {
        let short = name
            .strip_prefix("refs/tags/")
            .or_else(|| name.strip_prefix("refs/heads/"))
            .unwrap_or(name.as_str());

        for pattern in patterns {
            if let Some(candidate) = match_version_pattern(short, pattern) {
                versions.insert(candidate.to_string());
            }
        }
    }

    let mut out: Vec<_> = versions.into_iter().collect();
    out.sort_by(|a, b| compare_versions_for_updates(a, b));
    out
}

fn source_check_status(
    spec: &package::PackageSpec,
    source: &package::Source,
) -> Result<CheckStatus> {
    let patterns = extract_version_patterns(&source.url);
    if patterns.is_empty() {
        anyhow::bail!("source URL does not contain $version");
    }

    let expanded_url = spec.expand_vars(&source.url);
    let repo_url = remote_git_repository_from_source_url(&expanded_url)
        .ok_or_else(|| anyhow::anyhow!("could not derive a git remote from {}", expanded_url))?;
    let refs = list_remote_refs(&repo_url)?;
    let candidates = candidate_versions_from_refs(&refs, &patterns);

    if candidates.is_empty() {
        anyhow::bail!("no matching remote refs found in {}", repo_url);
    }

    let source_label = format!("git refs {}", repo_url);
    if let Some(latest) =
        best_newer_version(&spec.package.version, candidates.iter().map(String::as_str))
    {
        Ok(CheckStatus::UpdateAvailable {
            latest,
            source: source_label,
        })
    } else {
        Ok(CheckStatus::UpToDate {
            source: source_label,
        })
    }
}

fn check_package_spec(spec_path: &Path) -> CheckStatus {
    let spec = match package::PackageSpec::from_file(spec_path) {
        Ok(spec) => spec,
        Err(err) => {
            return CheckStatus::Unknown {
                reason: err.to_string(),
            };
        }
    };

    let mut best_update: Option<(String, String)> = None;
    let mut last_up_to_date_source: Option<String> = None;
    let mut reasons = Vec::new();

    for source in spec.sources() {
        match source_check_status(&spec, source) {
            Ok(CheckStatus::UpdateAvailable { latest, source }) => {
                let replace = match &best_update {
                    Some((current_best, _)) => {
                        compare_versions_for_updates(&latest, current_best) == Ordering::Greater
                    }
                    None => true,
                };
                if replace {
                    best_update = Some((latest, source));
                }
            }
            Ok(CheckStatus::UpToDate { source }) => {
                if last_up_to_date_source.is_none() {
                    last_up_to_date_source = Some(source);
                }
            }
            Ok(CheckStatus::Unknown { reason }) => reasons.push(reason),
            Err(err) => reasons.push(err.to_string()),
        }
    }

    if let Some((latest, source)) = best_update {
        return CheckStatus::UpdateAvailable { latest, source };
    }
    if let Some(source) = last_up_to_date_source {
        return CheckStatus::UpToDate { source };
    }

    CheckStatus::Unknown {
        reason: reasons
            .into_iter()
            .next()
            .unwrap_or_else(|| "no versioned sources found".to_string()),
    }
}

fn run_check_command(dir: &Path) -> Result<()> {
    let scan_root = dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve check root {}", dir.display()))?;
    let specs = scan_package_specs(&scan_root)?;
    if specs.is_empty() {
        ui::info(format!(
            "No depot package specs found under {}",
            scan_root.display()
        ));
        return Ok(());
    }

    for spec_path in specs {
        let spec = package::PackageSpec::from_file(&spec_path)?;
        match check_package_spec(&spec_path) {
            CheckStatus::UpdateAvailable { latest, source } => ui::warn(format!(
                "{} {} -> {} [{}] ({})",
                spec.package.name,
                spec.package.version,
                latest,
                source,
                spec_path.display()
            )),
            CheckStatus::UpToDate { source } => ui::info(format!(
                "{} {} is up to date [{}] ({})",
                spec.package.name,
                spec.package.version,
                source,
                spec_path.display()
            )),
            CheckStatus::Unknown { reason } => ui::warn(format!(
                "{} {} could not be checked: {} ({})",
                spec.package.name,
                spec.package.version,
                reason,
                spec_path.display()
            )),
        }
    }

    Ok(())
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
    install_test_deps: bool,
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

    let mut conflict_subjects = Vec::new();
    for step in &actionable_steps {
        match &step.origin {
            planner::PlanOrigin::Source { path, .. } => {
                let mut spec = package::PackageSpec::from_file(path)
                    .with_context(|| format!("Failed to parse spec {}", path.display()))?;
                spec.apply_config(config);
                conflict_subjects.extend(install_conflict_subjects_for_spec(
                    &spec,
                    true,
                    spec.build.flags.build_32,
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

    if should_delegate_live_rootfs_installs(rootfs) {
        let mut install_requests = Vec::new();
        for step in actionable_steps {
            match &step.origin {
                planner::PlanOrigin::Source { path, .. } => {
                    install_requests.push(path.clone());
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
                    install_requests.push(archive_path);
                }
                planner::PlanOrigin::Installed => {}
            }
        }
        run_child_install_command(&install_requests, rootfs, options)?;
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
                let spec = load_package_spec_from_staging_or_repo_record(staged.path(), record)?;
                let plans = plan_package_outputs_for_install(&spec, staged.path(), rootfs, config)?;
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

    for archive_path in archive_paths {
        ui::info(format!(
            "Installing package from: {}",
            archive_path.display()
        ));

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

    if confirm_installation && !ui::prompt_package_action("installation", &install_targets, true)? {
        anyhow::bail!("Aborted");
    }

    ui::info(format!(
        "Installing {} binary archive payload(s)",
        archive_paths.len()
    ));

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
    let installed = install_planned_packages_to_rootfs(&transaction_plans, options.rootfs, config)?;
    for pkg in installed {
        ui::success(format!(
            "Successfully installed {} v{}",
            pkg.name, pkg.version
        ));
    }
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

    ui::info(format!("Installing package from: {}", spec_path.display()));

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

    if staging_dir.is_none() {
        ui::info(format!(
            "Package: {} v{}-{}",
            pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
        ));
    }

    let mut conflict_subjects = install_conflict_subjects_for_spec(
        &pkg_spec,
        !options.lib32_only,
        staging_dir.is_none() && (options.lib32_only || pkg_spec.build.flags.build_32),
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
    let db_path = config.installed_db_path(options.rootfs);

    if staging_dir.is_none()
        && (options.no_deps || !should_install_test_deps(&pkg_spec, options.install_test_deps))
    {
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
        let missing = if should_install_test_deps(&pkg_spec, options.install_test_deps) {
            merge_missing_dependencies(missing, deps::check_test_deps(&pkg_spec, &db_path)?)
        } else {
            missing
        };
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

                let mut dep_spec_paths = Vec::new();
                for dep in missing {
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
                        assume_yes: false,
                        no_flags: options.no_flags,
                        cross_prefix: options.cross_prefix,
                        clean: options.clean,
                        install_test_deps: options.install_test_deps,
                        dep_chain: Some(&new_chain),
                    },
                )?;
            }
        }

        // Enforce required dependencies before building/installing.
        deps::require_build_deps(&pkg_spec, &db_path)?;
        deps::require_runtime_deps(&pkg_spec, &db_path)?;
        if should_install_test_deps(&pkg_spec, options.install_test_deps) {
            deps::require_test_deps(&pkg_spec, &db_path)?;
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
            options.lib32_only,
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

    let cli_test_deps = cli.test_deps;
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
            let install_test_deps = install_test_deps_enabled(cli_test_deps, &config);
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
                    include_test_deps: install_test_deps,
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
                        install_test_deps,
                    },
                )?;
            }

            let mut ran_direct_install = false;
            let direct_install_options = DirectInstallOptions {
                rootfs: &cli.rootfs,
                no_deps: cli.no_deps,
                no_flags: cli.no_flags,
                cross_prefix: cli.cross_prefix.as_deref(),
                clean: cli.clean,
                dry_run: cli.dry_run,
                lib32_only: cli.lib32_only,
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
            let removal_targets = vec![package.clone()];
            if !ui::prompt_package_action("removal", &removal_targets, true)? {
                anyhow::bail!("Aborted");
            }
            remove_installed_package_with_hooks(&package, &cli.rootfs, &config)?;
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
            let install_test_deps = install_test_deps_enabled(cli_test_deps, &config);

            // Apply system overrides
            pkg_spec.apply_config(&config);

            // Ensure database directory exists
            std::fs::create_dir_all(&config.db_dir).with_context(|| {
                format!(
                    "Failed to create database directory: {}",
                    config.db_dir.display()
                )
            })?;
            let db_path = config.installed_db_path(&cli.rootfs);

            if cli.no_deps || !should_install_test_deps(&pkg_spec, install_test_deps) {
                maybe_disable_tests_for_missing_deps(&mut pkg_spec, &db_path)?;
            }

            // Check build dependencies
            if !cli.no_deps {
                deps::print_dep_status(&pkg_spec, &db_path)?;
                let missing = merge_missing_dependencies(
                    deps::check_build_deps(&pkg_spec, &db_path)?,
                    deps::check_runtime_deps(&pkg_spec, &db_path)?,
                );
                let missing = if should_install_test_deps(&pkg_spec, install_test_deps) {
                    merge_missing_dependencies(missing, deps::check_test_deps(&pkg_spec, &db_path)?)
                } else {
                    missing
                };
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
                            include_test_deps: install_test_deps,
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
                            install_test_deps,
                        },
                    )?;
                }
                deps::require_build_deps(&pkg_spec, &db_path)?;
                deps::require_runtime_deps(&pkg_spec, &db_path)?;
                if should_install_test_deps(&pkg_spec, install_test_deps) {
                    deps::require_test_deps(&pkg_spec, &db_path)?;
                }
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
                        run_child_install_command(
                            &created_files,
                            &cli.rootfs,
                            InstallPlanExecutionOptions {
                                no_flags: cli.no_flags,
                                cross_prefix: cli.cross_prefix.as_deref(),
                                clean: cli.clean,
                                dry_run: cli.dry_run,
                                confirm_installation: false,
                                install_test_deps,
                            },
                        )?;
                        if cli.clean {
                            clean_build_workspace(&config)?;
                        }
                        return Ok(());
                    }

                    let mut transaction_plans = Vec::new();
                    if !cli.lib32_only {
                        let output_plans = plan_package_outputs_for_install(
                            &pkg_spec,
                            &destdir,
                            &cli.rootfs,
                            &config,
                        )?;
                        transaction_plans.extend(output_plans);
                    }
                    if let Some((lib32_spec, lib32_destdir)) = &lib32_install_bundle {
                        let staged =
                            plan_staged_install(lib32_spec, lib32_destdir, &cli.rootfs, &config)?;
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
        Commands::Update { packages } => {
            if cli.lib32_only {
                anyhow::bail!("--lib32-only is not supported with 'update'");
            }
            let config = config::Config::for_rootfs(&cli.rootfs);
            run_update_command(
                &packages,
                &config,
                UpdateCommandOptions {
                    rootfs: &cli.rootfs,
                    no_deps: cli.no_deps,
                    no_flags: cli.no_flags,
                    cross_prefix: cli.cross_prefix.as_deref(),
                    clean: cli.clean,
                    dry_run: cli.dry_run,
                    assume_yes: cli.yes,
                    install_test_deps: install_test_deps_enabled(cli_test_deps, &config),
                },
            )?;
        }
        Commands::Check { dir } => {
            run_check_command(&dir)?;
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
                let db_path = config.installed_db_path(&cli.rootfs);
                deps::print_dep_status(&pkg_spec, &db_path)?;
            } else {
                let config = config::Config::for_rootfs(&cli.rootfs);
                let info_lock = locking::open_lock(&config)?;
                let info_lock_path = locking::lock_path(&config);
                let _info_lock_guard = locking::try_read(&info_lock, &info_lock_path, "info")?;
                let db_path = config.installed_db_path(&cli.rootfs);
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
            let db_path = config.installed_db_path(&cli.rootfs);
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
            let db_path = config.installed_db_path(&cli.rootfs);
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
                    "Indexed {} spec(s) from {} TOML file(s): package rows={} provides rows={} conflicts rows={} dependency rows={} ignored_toml={}",
                    stats.specs_indexed,
                    stats.toml_files_scanned,
                    stats.package_rows,
                    stats.provides_rows,
                    stats.conflicts_rows,
                    stats.dependency_rows,
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
            println!("Install Test Deps: {}", config.install_test_deps);
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
            completed_at: None,
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
            conflicts: Vec::new(),
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

        let staged = extract_package_archive_to_staging(&cfg, &archive_path)?;

        assert!(staged.path().starts_with(staging_temp_root(&cfg)));
        assert!(staged.path().join("usr/bin/hello").exists());
        Ok(())
    }

    #[test]
    fn direct_archive_install_requests_batch_multiple_archives() -> Result<()> {
        fn write_archive(
            archive_path: &Path,
            package_name: &str,
            conflicts: &[&str],
            payload_path: &str,
            payload: &[u8],
        ) -> Result<()> {
            let file = fs::File::create(archive_path)
                .with_context(|| format!("Failed to create {}", archive_path.display()))?;
            let encoder = zstd::stream::write::Encoder::new(file, 3)
                .context("Failed to create zstd encoder")?;
            let mut tar = tar::Builder::new(encoder);

            let mut payload_header = tar::Header::new_gnu();
            payload_header.set_path(payload_path)?;
            payload_header.set_size(payload.len() as u64);
            payload_header.set_mode(0o755);
            payload_header.set_cksum();
            tar.append(&payload_header, payload)?;

            let conflicts_toml = if conflicts.is_empty() {
                String::new()
            } else {
                format!(
                    "conflicts = [{}]\n",
                    conflicts
                        .iter()
                        .map(|conflict| format!("\"{conflict}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let metadata = format!(
                "name = \"{package_name}\"\nversion = \"1.0\"\nrevision = 1\ndescription = \"test\"\nhomepage = \"https://example.test\"\nlicense = \"MIT\"\n{conflicts_toml}\n[dependencies]\nruntime = []\noptional = []\n"
            );
            let mut meta_header = tar::Header::new_gnu();
            meta_header.set_path(".metadata.toml")?;
            meta_header.set_size(metadata.len() as u64);
            meta_header.set_mode(0o644);
            meta_header.set_cksum();
            tar.append(&meta_header, metadata.as_bytes())?;

            let encoder = tar.into_inner()?;
            encoder.finish()?;
            Ok(())
        }

        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
        let archive_a = pkg_dir.path().join("alpha-1.0-1-x86_64.depot.pkg.tar.zst");
        let archive_b = pkg_dir.path().join("beta-1.0-1-x86_64.depot.pkg.tar.zst");
        write_archive(&archive_a, "alpha", &[], "usr/bin/alpha", b"alpha")?;
        write_archive(&archive_b, "beta", &[], "usr/bin/beta", b"beta")?;

        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("var/cache/depot/build");
        cfg.db_dir = rootfs.path().join("var/lib/depot");

        let installed = run_direct_archive_install_requests(
            DirectInstallOptions {
                rootfs: rootfs.path(),
                no_deps: true,
                no_flags: false,
                cross_prefix: None,
                clean: false,
                dry_run: false,
                lib32_only: false,
                install_test_deps: false,
            },
            &cfg,
            &[archive_a, archive_b],
            false,
        )?;

        assert!(installed);
        assert!(rootfs.path().join("usr/bin/alpha").exists());
        assert!(rootfs.path().join("usr/bin/beta").exists());
        let db_path = cfg.installed_db_path(rootfs.path());
        assert_eq!(
            db::get_package_version(&db_path, "alpha")?,
            Some("1.0".into())
        );
        assert_eq!(
            db::get_package_version(&db_path, "beta")?,
            Some("1.0".into())
        );
        Ok(())
    }

    #[test]
    fn direct_archive_install_rejects_conflicting_archives_in_same_batch() -> Result<()> {
        fn write_archive(
            archive_path: &Path,
            package_name: &str,
            conflicts: &[&str],
        ) -> Result<()> {
            let file = fs::File::create(archive_path)
                .with_context(|| format!("Failed to create {}", archive_path.display()))?;
            let encoder = zstd::stream::write::Encoder::new(file, 3)
                .context("Failed to create zstd encoder")?;
            let mut tar = tar::Builder::new(encoder);

            let payload = package_name.as_bytes();
            let mut payload_header = tar::Header::new_gnu();
            payload_header.set_path(format!("usr/bin/{package_name}"))?;
            payload_header.set_size(payload.len() as u64);
            payload_header.set_mode(0o755);
            payload_header.set_cksum();
            tar.append(&payload_header, payload)?;

            let conflicts_toml = if conflicts.is_empty() {
                String::new()
            } else {
                format!(
                    "conflicts = [{}]\n",
                    conflicts
                        .iter()
                        .map(|conflict| format!("\"{conflict}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let metadata = format!(
                "name = \"{package_name}\"\nversion = \"1.0\"\nrevision = 1\ndescription = \"test\"\nhomepage = \"https://example.test\"\nlicense = \"MIT\"\n{conflicts_toml}\n[dependencies]\nruntime = []\noptional = []\n"
            );
            let mut meta_header = tar::Header::new_gnu();
            meta_header.set_path(".metadata.toml")?;
            meta_header.set_size(metadata.len() as u64);
            meta_header.set_mode(0o644);
            meta_header.set_cksum();
            tar.append(&meta_header, metadata.as_bytes())?;

            let encoder = tar.into_inner()?;
            encoder.finish()?;
            Ok(())
        }

        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
        let archive_a = pkg_dir.path().join("alpha-1.0-1-x86_64.depot.pkg.tar.zst");
        let archive_b = pkg_dir.path().join("beta-1.0-1-x86_64.depot.pkg.tar.zst");
        write_archive(&archive_a, "alpha", &["beta"])?;
        write_archive(&archive_b, "beta", &[])?;

        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("var/cache/depot/build");
        cfg.db_dir = rootfs.path().join("var/lib/depot");

        let err = run_direct_archive_install_requests(
            DirectInstallOptions {
                rootfs: rootfs.path(),
                no_deps: true,
                no_flags: false,
                cross_prefix: None,
                clean: false,
                dry_run: false,
                lib32_only: false,
                install_test_deps: false,
            },
            &cfg,
            &[archive_a, archive_b],
            false,
        )
        .expect_err("conflicting archives should be rejected");

        assert!(
            err.to_string()
                .contains("Cannot install conflicting packages in the same transaction")
        );
        Ok(())
    }

    #[test]
    fn collect_conflicting_installed_packages_matches_by_name_and_provide() -> Result<()> {
        let removals = collect_conflicting_installed_packages(
            &[InstallConflictSubject {
                package: "beta".into(),
                provides: Vec::new(),
                conflicts: vec!["alpha".into(), "editor".into()],
            }],
            &[InstalledConflictPackage {
                name: "alpha".into(),
                provides: vec!["editor".into()],
            }],
        )?;

        assert_eq!(
            removals.get("alpha"),
            Some(&BTreeSet::from(["beta".to_string()]))
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn binary_archive_install_preserves_setuid_permissions() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
        let archive_path = pkg_dir.path().join("sudo-1.0-1-x86_64.depot.pkg.tar.zst");

        let file = fs::File::create(&archive_path)
            .with_context(|| format!("Failed to create {}", archive_path.display()))?;
        let encoder =
            zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
        let mut tar = tar::Builder::new(encoder);
        let payload = b"sudo";
        let mut header = tar::Header::new_gnu();
        header.set_path("bin/sudo").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o4755);
        header.set_cksum();
        tar.append(&header, &payload[..]).unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("var/cache/depot/build");
        cfg.db_dir = rootfs.path().join("var/lib/depot");

        let staged = extract_package_archive_to_staging(&cfg, &archive_path)?;
        let staged_mode = fs::metadata(staged.path().join("bin/sudo"))?
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(staged_mode, 0o4755);

        let record = db::repo::BinaryRepoPackageRecord {
            repo_name: "core".into(),
            name: "sudo".into(),
            version: "1.0".into(),
            revision: 1,
            completed_at: None,
            filename: archive_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default()
                .to_string(),
            size: payload.len() as u64,
            sha256: String::new(),
            sha512: String::new(),
            description: Some("sudo".into()),
            homepage: Some("https://example.test".into()),
            license: Some("ISC".into()),
            provides: Vec::new(),
            conflicts: Vec::new(),
            runtime_dependencies: Vec::new(),
            optional_dependencies: Vec::new(),
        };
        let spec = package_spec_from_repo_record(&record);
        let installed =
            install_package_outputs_to_rootfs(&spec, staged.path(), rootfs.path(), &cfg)?;

        assert_eq!(installed.len(), 1);
        let root_mode = fs::metadata(rootfs.path().join("bin/sudo"))?
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(root_mode, 0o4755);
        Ok(())
    }

    #[test]
    fn binary_archive_install_honors_keep_paths_from_metadata() -> Result<()> {
        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
        let archive_path = pkg_dir
            .path()
            .join("filesystem-1.0-3-x86_64.depot.pkg.tar.zst");

        let file = fs::File::create(&archive_path)
            .with_context(|| format!("Failed to create {}", archive_path.display()))?;
        let encoder =
            zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
        let mut tar = tar::Builder::new(encoder);

        let payload = b"package-fstab";
        let mut fstab_header = tar::Header::new_gnu();
        fstab_header.set_path("etc/fstab").unwrap();
        fstab_header.set_size(payload.len() as u64);
        fstab_header.set_mode(0o644);
        fstab_header.set_cksum();
        tar.append(&fstab_header, &payload[..]).unwrap();

        let metadata = br#"name = "filesystem"
version = "1.0.1"
revision = 3
description = "Base filesystem"
homepage = "https://example.test"
license = "Unlicense"
keep = ["etc/fstab"]

[dependencies]
runtime = []
optional = []
"#;
        let mut meta_header = tar::Header::new_gnu();
        meta_header.set_path(".metadata.toml").unwrap();
        meta_header.set_size(metadata.len() as u64);
        meta_header.set_mode(0o644);
        meta_header.set_cksum();
        tar.append(&meta_header, &metadata[..]).unwrap();

        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.build_dir = rootfs.path().join("var/cache/depot/build");
        cfg.db_dir = rootfs.path().join("var/lib/depot");

        fs::create_dir_all(rootfs.path().join("etc"))?;
        fs::write(rootfs.path().join("etc/fstab"), "existing-fstab")?;

        let (spec, staged) = load_package_archive_into_staging(&cfg, &archive_path)?;
        assert_eq!(spec.build.flags.keep, vec!["etc/fstab".to_string()]);

        let installed =
            install_package_outputs_to_rootfs(&spec, staged.path(), rootfs.path(), &cfg)?;
        assert_eq!(installed.len(), 1);
        assert_eq!(
            fs::read_to_string(rootfs.path().join("etc/fstab"))?,
            "existing-fstab"
        );
        assert_eq!(
            fs::read_to_string(rootfs.path().join("etc/fstab.depotnew"))?,
            "package-fstab"
        );
        Ok(())
    }

    #[test]
    fn plan_staged_install_reads_updates_from_rootfs_installed_db() -> Result<()> {
        let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
        let mut cfg = config::Config::for_rootfs(rootfs.path());
        cfg.db_dir = rootfs.path().join("home/vertex/.local/share/depot");

        let installed_db = cfg.installed_db_path(rootfs.path());
        fs::create_dir_all(
            installed_db
                .parent()
                .context("Installed DB path should have a parent")?,
        )?;

        let existing_dest = rootfs.path().join("installed");
        fs::create_dir_all(existing_dest.join("usr/bin"))?;
        fs::write(existing_dest.join("usr/bin/tool"), "old")?;

        let spec = package::PackageSpec {
            package: package::PackageInfo {
                name: "filesystem".into(),
                version: "1.0.1".into(),
                revision: 3,
                description: "Base filesystem".into(),
                homepage: "https://example.test".into(),
                license: vec!["Unlicense".into()],
            },
            packages: Vec::new(),
            alternatives: package::Alternatives::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: package::Build {
                build_type: package::BuildType::Bin,
                flags: package::BuildFlags::default(),
            },
            dependencies: package::Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };
        db::register_package(&installed_db, &spec, &existing_dest)?;

        let staged_dest = rootfs.path().join("staged");
        fs::create_dir_all(staged_dest.join("usr/bin"))?;
        fs::write(staged_dest.join("usr/bin/tool"), "new")?;

        let plan = plan_staged_install(&spec, &staged_dest, rootfs.path(), &cfg)?;
        assert!(plan.is_update);
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
    fn update_candidate_prefers_binary_when_versions_match_and_config_does() {
        let installed = db::InstalledPackageRecord {
            name: "pkg".into(),
            version: "1.0.0".into(),
            revision: 1,
            completed_at: None,
        };
        let source_spec = package::PackageSpec {
            package: package::PackageInfo {
                name: "pkg".into(),
                version: "1.1.0".into(),
                revision: 1,
                description: "test".into(),
                homepage: "https://example.test".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: package::Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![package::Source {
                url: "https://example.test/pkg-$version.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "pkg-$version".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: package::Build {
                build_type: package::BuildType::Custom,
                flags: package::BuildFlags::default(),
            },
            dependencies: package::Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };
        let source_candidates = HashMap::from([(
            "pkg".to_string(),
            SourceUpdateCandidate {
                repo_name: "source".into(),
                repo_priority: 5,
                path: PathBuf::from("/tmp/pkg.toml"),
                completed_at: None,
                spec: source_spec,
            },
        )]);
        let binary_candidates = HashMap::from([(
            "pkg".to_string(),
            (
                0,
                db::repo::BinaryRepoPackageRecord {
                    repo_name: "binary".into(),
                    name: "pkg".into(),
                    version: "1.1.0".into(),
                    revision: 1,
                    completed_at: None,
                    filename: "pkg-1.1.0-1-x86_64.depot.pkg.tar.zst".into(),
                    size: 1,
                    sha256: String::new(),
                    sha512: String::new(),
                    description: None,
                    homepage: None,
                    license: None,
                    provides: Vec::new(),
                    conflicts: Vec::new(),
                    runtime_dependencies: Vec::new(),
                    optional_dependencies: Vec::new(),
                },
            ),
        )]);

        let selected = select_update_candidate(
            &installed,
            None,
            &source_candidates,
            &binary_candidates,
            true,
        )
        .expect("expected update candidate");
        assert!(matches!(selected.origin, UpdateOrigin::Binary { .. }));
    }

    #[test]
    fn select_update_candidate_uses_newer_timestamp_when_versions_match() {
        let installed = db::InstalledPackageRecord {
            name: "pkg".into(),
            version: "1.0.0".into(),
            revision: 1,
            completed_at: Some(100),
        };
        let source_spec = package::PackageSpec {
            package: package::PackageInfo {
                name: "pkg".into(),
                version: "1.0.0".into(),
                revision: 1,
                description: "test".into(),
                homepage: "https://example.test".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: package::Alternatives::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: package::Build {
                build_type: package::BuildType::Custom,
                flags: package::BuildFlags::default(),
            },
            dependencies: package::Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };
        let source_candidates = HashMap::from([(
            "pkg".to_string(),
            SourceUpdateCandidate {
                repo_name: "source".into(),
                repo_priority: 5,
                path: PathBuf::from("/tmp/pkg.toml"),
                completed_at: Some(200),
                spec: source_spec,
            },
        )]);

        let selected = select_update_candidate(
            &installed,
            Some(100),
            &source_candidates,
            &HashMap::new(),
            true,
        )
        .expect("expected update candidate");
        assert_eq!(selected.candidate_version, "1.0.0");
        assert_eq!(selected.candidate_completed_at, Some(200));
    }

    #[test]
    fn collect_update_candidates_skips_source_when_prefer_binary_is_enabled() -> Result<()> {
        let temp = tempfile::tempdir().context("Failed to create temp dir")?;
        let rootfs = temp.path().join("rootfs");
        let repo_clones = temp.path().join("repos");
        let build_dir = temp.path().join("build");
        let db_dir = rootfs.join("var/lib/depot");
        fs::create_dir_all(&db_dir)?;
        fs::create_dir_all(&repo_clones)?;
        fs::create_dir_all(&build_dir)?;

        let mut config = config::Config::for_rootfs(&rootfs);
        config.repo_clone_dir = repo_clones.clone();
        config.build_dir = build_dir;
        config.db_dir = db_dir.clone();
        config.repo_settings.prefer_binary = true;
        config.binary_repos.clear();
        config.source_repos.insert(
            "private".into(),
            config::SourceRepo {
                url: "https://example.test/private.git".into(),
                enabled: true,
                priority: 0,
                subdirs: Vec::new(),
            },
        );

        let installed_spec = package::PackageSpec {
            package: package::PackageInfo {
                name: "pkg".into(),
                version: "1.0.0".into(),
                revision: 1,
                description: "pkg".into(),
                homepage: "https://example.test".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: package::Alternatives::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: package::Build {
                build_type: package::BuildType::Bin,
                flags: package::BuildFlags::default(),
            },
            dependencies: package::Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };
        let dest = temp.path().join("dest");
        fs::create_dir_all(dest.join("usr/bin"))?;
        fs::write(dest.join("usr/bin/pkg"), "pkg")?;
        db::register_package(&config.installed_db_path(&rootfs), &installed_spec, &dest)?;

        let updates = collect_update_candidates(&config, &rootfs, &[])?;
        assert!(updates.is_empty());
        Ok(())
    }

    #[test]
    fn collect_missing_update_dependencies_skips_planned_provides_and_installed_deps() -> Result<()>
    {
        let temp = tempfile::tempdir().context("Failed to create temp dir")?;
        let db_path = temp.path().join("packages.db");

        let libc_spec = package::PackageSpec {
            package: package::PackageInfo {
                name: "glibc".into(),
                version: "1.0".into(),
                revision: 1,
                description: "glibc".into(),
                homepage: "https://example.test".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: package::Alternatives::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: package::Build {
                build_type: package::BuildType::Bin,
                flags: package::BuildFlags::default(),
            },
            dependencies: package::Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };
        let dest = temp.path().join("dest");
        fs::create_dir_all(dest.join("usr/lib"))?;
        fs::write(dest.join("usr/lib/libc.so"), "glibc")?;
        db::register_package(&db_path, &libc_spec, &dest)?;

        let missing = collect_missing_update_dependencies(
            &[
                UpdateCandidate {
                    package: "pkg".into(),
                    installed_version: "1.0".into(),
                    installed_revision: 1,
                    installed_completed_at: None,
                    candidate_version: "2.0".into(),
                    candidate_revision: 1,
                    candidate_completed_at: None,
                    runtime_dependencies: vec!["glibc".into(), "helper-virtual".into()],
                    provides: Vec::new(),
                    conflicts: Vec::new(),
                    repo_priority: 0,
                    origin: UpdateOrigin::Source {
                        repo_name: "source".into(),
                        path: PathBuf::from("/tmp/pkg.toml"),
                    },
                },
                UpdateCandidate {
                    package: "helper".into(),
                    installed_version: "1.0".into(),
                    installed_revision: 1,
                    installed_completed_at: None,
                    candidate_version: "2.0".into(),
                    candidate_revision: 1,
                    candidate_completed_at: None,
                    runtime_dependencies: Vec::new(),
                    provides: vec!["helper-virtual".into()],
                    conflicts: Vec::new(),
                    repo_priority: 0,
                    origin: UpdateOrigin::Source {
                        repo_name: "source".into(),
                        path: PathBuf::from("/tmp/helper.toml"),
                    },
                },
                UpdateCandidate {
                    package: "tool".into(),
                    installed_version: "1.0".into(),
                    installed_revision: 1,
                    installed_completed_at: None,
                    candidate_version: "2.0".into(),
                    candidate_revision: 1,
                    candidate_completed_at: None,
                    runtime_dependencies: vec!["newdep".into()],
                    provides: Vec::new(),
                    conflicts: Vec::new(),
                    repo_priority: 0,
                    origin: UpdateOrigin::Source {
                        repo_name: "source".into(),
                        path: PathBuf::from("/tmp/tool.toml"),
                    },
                },
            ],
            &db_path,
        )?;

        assert_eq!(missing, vec!["newdep".to_string()]);
        Ok(())
    }

    #[test]
    fn validate_no_transaction_conflicts_rejects_conflicting_updates() {
        let err = validate_no_transaction_conflicts(&[
            InstallConflictSubject {
                package: "alpha".into(),
                provides: Vec::new(),
                conflicts: vec!["beta".into()],
            },
            InstallConflictSubject {
                package: "beta".into(),
                provides: Vec::new(),
                conflicts: Vec::new(),
            },
        ])
        .expect_err("conflicting update set should be rejected");

        assert!(
            err.to_string()
                .contains("Cannot install conflicting packages in the same transaction")
        );
    }

    #[test]
    fn compare_versions_for_updates_handles_semver_and_date_versions() {
        assert_eq!(
            compare_versions_for_updates("10.8.4", "10.8.3"),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions_for_updates("20260202", "20251231"),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions_for_updates("1.10", "1.9"),
            Ordering::Greater
        );
    }

    #[test]
    fn extract_version_patterns_handles_git_and_release_urls() {
        let git_patterns =
            extract_version_patterns("https://codeberg.org/Limine/limine.git#v$version");
        assert!(git_patterns.contains(&VersionPattern {
            prefix: "v".into(),
            suffix: String::new(),
        }));

        let release_patterns = extract_version_patterns(
            "https://github.com/Mic92/iana-etc/releases/download/$version/iana-etc-$version.tar.gz",
        );
        assert!(release_patterns.contains(&VersionPattern {
            prefix: String::new(),
            suffix: String::new(),
        }));
    }

    #[test]
    fn candidate_versions_from_refs_matches_version_patterns() {
        let refs = vec![
            "refs/tags/v10.8.3".to_string(),
            "refs/tags/v10.8.4".to_string(),
            "refs/heads/main".to_string(),
        ];
        let patterns = extract_version_patterns("https://codeberg.org/Limine/limine.git#v$version");
        let candidates = candidate_versions_from_refs(&refs, &patterns);

        assert_eq!(candidates, vec!["10.8.3".to_string(), "10.8.4".to_string()]);
        assert_eq!(
            best_newer_version("10.8.3", candidates.iter().map(String::as_str)),
            Some("10.8.4".to_string())
        );
    }

    #[test]
    fn remote_git_repository_from_github_release_url_maps_to_repo_git_url() {
        let repo_url = remote_git_repository_from_source_url(
            "https://github.com/Mic92/iana-etc/releases/download/20260202/iana-etc-20260202.tar.gz",
        );
        assert_eq!(
            repo_url,
            Some("https://github.com/Mic92/iana-etc.git".to_string())
        );
    }

    #[test]
    #[cfg(unix)]
    fn child_install_command_batches_multiple_requests_in_one_invocation() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().context("Failed to create temp dir")?;
        let script_path = temp.path().join("capture-child-install.sh");
        let args_path = temp.path().join("args.txt");
        let env_path = temp.path().join("env.txt");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nprintf '%s' \"${{DEPOT_DEPCHAIN:-}}\" > \"{}\"\n",
            args_path.display(),
            env_path.display()
        );
        fs::write(&script_path, script)
            .with_context(|| format!("Failed to write {}", script_path.display()))?;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("Failed to chmod {}", script_path.display()))?;

        let requests = vec![
            PathBuf::from("/tmp/pkg-a.toml"),
            PathBuf::from("/tmp/pkg-b.toml"),
        ];
        let rootfs = Path::new("/");
        run_install_command_with_program(
            &script_path,
            &requests,
            rootfs,
            ChildInstallCommandOptions {
                no_deps: false,
                assume_yes: false,
                no_flags: true,
                cross_prefix: Some("x86_64-linux-musl"),
                clean: true,
                install_test_deps: true,
                dep_chain: Some("parent"),
            },
        )?;

        let captured_args = fs::read_to_string(&args_path)
            .with_context(|| format!("Failed to read {}", args_path.display()))?;
        assert_eq!(
            captured_args.lines().collect::<Vec<_>>(),
            vec![
                "-r",
                "/",
                "--no-flags",
                "--cross-prefix",
                "x86_64-linux-musl",
                "--clean",
                "--test-deps",
                "install",
                "/tmp/pkg-a.toml",
                "/tmp/pkg-b.toml",
            ]
        );
        assert_eq!(fs::read_to_string(&env_path)?, "parent");
        Ok(())
    }

    #[test]
    fn rootfs_is_system_root_detects_live_rootfs() {
        assert!(rootfs_is_system_root(Path::new("/")));
        assert!(!rootfs_is_system_root(Path::new("/tmp/depot-test-rootfs")));
    }

    #[test]
    fn command_requires_live_root_for_install_remove_and_update() {
        assert!(command_requires_live_root(&Commands::Install {
            spec_or_archive: vec![PathBuf::from("foo")],
            spec: None,
        }));
        assert!(command_requires_live_root(&Commands::Remove {
            package: "foo".to_string(),
        }));
        assert!(command_requires_live_root(&Commands::Update {
            packages: vec!["foo".to_string()],
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
