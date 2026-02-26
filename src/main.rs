//! Depot - Not Your Average Package Manager
//! A source-based package manager for Linux

mod builder;
mod config;
mod cross;
mod db;
mod deps;
mod fakeroot;
mod index;
mod install;
mod locking;
mod package;
mod planner;
mod shell_helpers;
mod signing;
mod source;
mod staging;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::path::{Path, PathBuf};

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

fn install_staged_to_rootfs(
    pkg_spec: &package::PackageSpec,
    destdir: &Path,
    rootfs: &Path,
    config: &config::Config,
) -> Result<()> {
    std::fs::create_dir_all(&config.db_dir).with_context(|| {
        format!(
            "Failed to create database directory: {}",
            config.db_dir.display()
        )
    })?;
    let db_path = config.db_dir.join("packages.db");

    let is_update = db::get_package_version(&db_path, &pkg_spec.package.name)?.is_some();
    let staged_scripts_dir = install::scripts::staged_scripts_dir(destdir);
    let installed_scripts_dir =
        install::scripts::installed_scripts_dir(rootfs, &pkg_spec.package.name);

    if is_update {
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

    let new_files = staging::generate_manifest_with_dirs(destdir)?;
    let remove_paths =
        db::calculate_upgrade_paths(&db_path, &pkg_spec.package.name, &new_files.files)?;

    let tx_base = config.build_dir.join("tx");
    let tx = staging::install_atomic(
        destdir,
        rootfs,
        &tx_base,
        &remove_paths,
        &pkg_spec.build.flags.keep,
    )?;

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

    if is_update {
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

fn execute_install_plan_with_child_commands(
    plan: &planner::ExecutionPlan,
    rootfs: &Path,
    no_flags: bool,
    cross_prefix: Option<&str>,
    clean: bool,
    dry_run: bool,
    config: &config::Config,
) -> Result<()> {
    let summary = plan.summary();
    if summary.source_build_installs > 0
        && !ui::prompt_yes_no(
            &format!(
                "Plan will build {} package(s) from source before install. Continue?",
                summary.source_build_installs
            ),
            true,
        )?
    {
        anyhow::bail!("Aborted");
    }

    if plan.actionable_steps().next().is_none() {
        ui::info("Nothing to do.");
        return Ok(());
    }

    if !ui::prompt_yes_no("Proceed with executing install plan?", true)? {
        anyhow::bail!("Aborted");
    }

    if dry_run {
        ui::info("Dry run enabled, no install/build actions executed.");
        return Ok(());
    }

    let exe = std::env::current_exe().context("Failed to locate depot executable")?;

    for step in plan.actionable_steps() {
        let input_path = match &step.origin {
            planner::PlanOrigin::Source { path, .. } => path.clone(),
            planner::PlanOrigin::Binary { repo_name, record } => {
                let repo_cfg = config
                    .binary_repos
                    .get(repo_name)
                    .with_context(|| format!("Binary repo '{}' not found in config", repo_name))?;
                db::repo::fetch_binary_package_archive(
                    repo_name,
                    repo_cfg,
                    record,
                    &config.package_cache_dir,
                )?
            }
            planner::PlanOrigin::Installed => continue,
        };

        ui::info(format!(
            "Executing planned step: {} ({})",
            step.package,
            input_path.display()
        ));

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("-r").arg(rootfs);
        cmd.arg("--no-deps");
        cmd.arg("--yes");
        if no_flags {
            cmd.arg("--no-flags");
        }
        if let Some(p) = cross_prefix {
            cmd.arg("--cross-prefix").arg(p);
        }
        if clean {
            cmd.arg("--clean");
        }
        cmd.arg("install").arg(input_path);

        let status = cmd
            .status()
            .context("Failed to spawn planned install step")?;
        if !status.success() {
            anyhow::bail!("Planned install step for '{}' failed", step.package);
        }
    }
    Ok(())
}

#[derive(Parser)]
#[command(name = "Depot")]
#[command(about = "Depot - Source-based package manager for Linux", long_about = None)]
#[command(version)]
struct Cli {
    /// Custom root filesystem path
    #[arg(long, short = 'r', default_value = "/", global = true)]
    rootfs: PathBuf,

    /// Skip dependency checks
    #[arg(long, global = true)]
    no_deps: bool,

    /// Do not export CFLAGS/CXXFLAGS/LDFLAGS to build commands
    #[arg(
        long,
        global = true,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_value_t = false,
        default_missing_value = "true"
    )]
    no_flags: bool,

    /// Cross-compilation prefix (e.g., x86_64-linux-musl, aarch64-linux-gnu)
    #[arg(long, global = true)]
    cross_prefix: Option<String>,

    /// Clean build workspace after successful install/build
    #[arg(long, global = true)]
    clean: bool,

    /// Automatically answer yes to prompts and pick the default provider choice
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    /// Show what would happen without performing builds/installs
    #[arg(long, global = true)]
    dry_run: bool,

    /// Build/install only the lib32 companion package path (skip primary package output)
    #[arg(long, global = true)]
    lib32_only: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build and install a package from a spec file
    Install {
        /// Path to package spec (.toml) or package archive (.tar.zst)
        #[arg(value_name = "SPEC_OR_ARCHIVE")]
        spec_or_archive: PathBuf,

        /// Explicitly specify path to package spec (.toml file)
        #[arg(short, long = "spec", visible_alias = "package", alias = "p")]
        spec: Option<PathBuf>,
    },
    /// Remove an installed package
    Remove {
        /// Package name to remove
        package: String,
    },
    /// Build a package without installing
    Build {
        /// Path to package spec (.toml file)
        #[arg(value_name = "SPEC")]
        spec_pos: Option<PathBuf>,

        /// Explicitly specify path to package spec (.toml file)
        #[arg(short, long = "spec", visible_alias = "package", alias = "p")]
        spec: Option<PathBuf>,

        /// Install package to rootfs after creating package archive(s)
        #[arg(long)]
        install: bool,
    },
    /// Show information about a package
    Info {
        /// Path to package spec or installed package name
        package: String,
    },
    /// Search configured source and binary repos by package name or provides
    Search {
        /// Search query
        query: String,
        /// Search repository file lists (binary repo metadata) by path substring
        #[arg(long)]
        files: bool,
    },
    /// Show which installed package owns a filesystem path
    Owns {
        /// Path to query (absolute or relative to rootfs)
        path: PathBuf,
    },
    /// List installed packages
    List,
    /// Create a detached minisign signature for a .zst file
    Sign {
        /// Path to the .zst file to sign
        file: PathBuf,
    },
    /// Repository management
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Show current configuration
    Config,
    /// Create a new package specification interactively
    MakeSpec {
        /// Output file path (defaults to <name>.toml)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum RepoCommands {
    /// Create a repository database from a directory of packages
    Create {
        /// Directory containing .depot.pkg.tar.zst files
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Sync git mirrors configured in /etc/depot.d/mirrors.toml into /usr/src/depot
    Sync,
    /// Sync source repos configured in /etc/depot.d/repos.toml into /usr/src/depot
    Update {
        /// Update only one source repo by name
        name: Option<String>,
    },
    /// List configured source and binary repos
    List,
    /// Add or update a repo entry in /etc/depot.d/repos.toml
    Add {
        /// Repo name (e.g. vertex)
        name: String,
        /// Source git URL or binary repo base URL
        url: String,
        /// Repo kind
        #[arg(long, value_enum, default_value_t = RepoKindArg::Source)]
        kind: RepoKindArg,
        /// Optional source repo subdirectory to index (repeatable)
        #[arg(long = "subdir")]
        subdirs: Vec<String>,
        /// Repo priority (lower = higher priority)
        #[arg(long, default_value_t = 0)]
        priority: i32,
        /// Add repo as disabled
        #[arg(long)]
        disabled: bool,
        /// Binary repo architecture table entry to add/update (defaults to this machine's arch)
        #[arg(long)]
        arch: Option<String>,
        /// Binary repo DB filename/path (relative to repo URL)
        #[arg(long = "repo-db", default_value = "repo.db.zst")]
        repo_db: String,
        /// Allow unsigned repo metadata for this binary repo
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Remove a repo entry from /etc/depot.d/repos.toml
    Remove {
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Enable a repo entry in /etc/depot.d/repos.toml
    Enable {
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Disable a repo entry in /etc/depot.d/repos.toml
    Disable {
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Query binary repo metadata for the package that owns a file path
    Owns {
        /// Path to query (absolute or relative install path)
        path: PathBuf,
    },
    /// Show status of configured git mirrors
    Status,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RepoKindArg {
    Source,
    Binary,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    ui::set_assume_yes(cli.yes);

    match cli.command {
        Commands::Install {
            spec_or_archive,
            spec,
        } => {
            warn_if_running_as_root_for_build("install", &cli.rootfs);
            let mut spec_path = spec.unwrap_or(spec_or_archive);

            // Load configuration early so we can use the configured repo clone dir
            let config = config::Config::for_rootfs(&cli.rootfs);

            if !cli.no_deps {
                let request_path_like = spec_path.exists();
                let is_archive_request = request_path_like
                    && spec_path
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .ends_with(".tar.zst");
                if !is_archive_request {
                    let target = if request_path_like {
                        planner::InstallTarget::SpecPath(spec_path.clone())
                    } else {
                        planner::InstallTarget::PackageName(spec_path.to_string_lossy().to_string())
                    };
                    let local_sibling_root = spec_path
                        .parent()
                        .and_then(|p| p.parent())
                        .map(Path::to_path_buf);
                    let plan = planner::build_install_plan(
                        &config,
                        &cli.rootfs,
                        target,
                        planner::PlannerOptions {
                            assume_yes: cli.yes,
                            prefer_binary: config.repo_settings.prefer_binary,
                            local_sibling_root,
                        },
                    )?;
                    print_plan_summary(&plan);
                    execute_install_plan_with_child_commands(
                        &plan,
                        &cli.rootfs,
                        cli.no_flags,
                        cli.cross_prefix.as_deref(),
                        cli.clean,
                        cli.dry_run,
                        &config,
                    )?;
                    if cli.clean {
                        clean_build_workspace(&config)?;
                    }
                    return Ok(());
                }
            }

            let mut install_lock = locking::open_lock(&config)?;
            let install_lock_path = locking::lock_path(&config);
            let _install_lock_guard =
                locking::try_write(&mut install_lock, &install_lock_path, "install")?;

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
                    binary_repos
                        .sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

                    for (repo_name, repo_cfg) in binary_repos {
                        match db::repo::find_binary_repo_package(
                            repo_name,
                            repo_cfg,
                            &cli.rootfs,
                            &config.package_cache_dir,
                            &name,
                        ) {
                            Ok(Some(rec)) => {
                                let archive = db::repo::fetch_binary_package_archive(
                                    repo_name,
                                    repo_cfg,
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

            let (pkg_spec, staging_dir): (package::PackageSpec, Option<tempfile::TempDir>) =
                if spec_path.to_string_lossy().ends_with(".tar.zst") {
                    // Install from archive
                    ui::info(format!("Detected package archive: {}", spec_path.display()));
                    let tmp_dir = tempfile::TempDir::new()?;
                    let extract_dir = tmp_dir.path().to_path_buf();

                    // Extract metadata.toml first to get spec
                    let file = fs::File::open(&spec_path)?;
                    let zstd_decoder = zstd::stream::read::Decoder::new(file)?;
                    let mut archive = tar::Archive::new(zstd_decoder);

                    let mut metadata_content = String::new();
                    for entry in archive.entries()? {
                        let mut entry = entry?;
                        if entry.path()?.to_string_lossy() == ".metadata.toml" {
                            use std::io::Read;
                            entry.read_to_string(&mut metadata_content)?;
                            break;
                        }
                    }

                    if metadata_content.is_empty() {
                        anyhow::bail!(
                            "Package archive does not contain .metadata.toml: {}",
                            spec_path.display()
                        );
                    }

                    let file = fs::File::open(&spec_path)?;
                    let zstd_decoder = zstd::stream::read::Decoder::new(file)?;
                    let mut archive = tar::Archive::new(zstd_decoder);
                    archive.unpack(&extract_dir)?;

                    let metadata: toml::Value = toml::from_str(&metadata_content)?;

                    // Create a minimal spec from metadata
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
                            runtime: if let Some(deps) = metadata
                                .get("dependencies")
                                .and_then(|v| v.get("runtime"))
                                .and_then(|v| v.as_array())
                            {
                                deps.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(String::from)
                                    .collect()
                            } else {
                                Vec::new()
                            },
                            test: if let Some(deps) = metadata
                                .get("dependencies")
                                .and_then(|v| v.get("test"))
                                .and_then(|v| v.as_array())
                            {
                                deps.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(String::from)
                                    .collect()
                            } else {
                                Vec::new()
                            },
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

                    (spec, Some(tmp_dir))
                } else {
                    // Install from spec (normal build)
                    let mut pkg_spec = package::PackageSpec::from_file(&spec_path)?;
                    pkg_spec.apply_config(&config);
                    (pkg_spec, None)
                };

            if cli.lib32_only && staging_dir.is_some() {
                anyhow::bail!("--lib32-only is only supported when installing from a package spec");
            }

            ui::info(format!(
                "Package: {} v{}-{}",
                pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
            ));

            if cli.dry_run {
                ui::info("Dry run enabled, stopping before install/build work.");
                return Ok(());
            }

            if !ui::prompt_yes_no(
                &format!(
                    "Proceed with install for {} v{}-{}?",
                    pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
                ),
                true,
            )? {
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

            // Check dependencies and prompt for auto-install if needed
            if !cli.no_deps {
                let needs_test_deps =
                    matches!(pkg_spec.build.build_type, package::BuildType::Autotools);
                deps::print_dep_status(&pkg_spec, &db_path)?;

                // Collect all missing dependencies (build + runtime)
                let mut missing = deps::check_build_deps(&pkg_spec, &db_path)?;
                let missing_runtime = deps::check_runtime_deps(&pkg_spec, &db_path)?;

                for dep in missing_runtime {
                    if !missing.contains(&dep) {
                        missing.push(dep);
                    }
                }
                if needs_test_deps {
                    let missing_test = deps::check_test_deps(&pkg_spec, &db_path)?;
                    for dep in missing_test {
                        if !missing.contains(&dep) {
                            missing.push(dep);
                        }
                    }
                }

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
                    if ui::prompt_yes_no("Attempt to install them now?", true)? {
                        // Build package index for fast lookups
                        let pkg_index = index::PackageIndex::build_with_repo_dir(Some(
                            config.repo_clone_dir.clone(),
                        ));

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
                                cmd.arg("-r").arg(&cli.rootfs);

                                if cli.no_deps {
                                    cmd.arg("--no-deps");
                                }
                                if cli.no_flags {
                                    cmd.arg("--no-flags");
                                }
                                if let Some(ref p) = cli.cross_prefix {
                                    cmd.arg("--cross-prefix").arg(p);
                                }
                                if cli.clean {
                                    cmd.arg("--clean");
                                }

                                cmd.arg("install").arg(&dep_spec_path);
                                cmd.env("DEPOT_DEPCHAIN", &new_chain);

                                let status = cmd.status()?;

                                if !status.success() {
                                    anyhow::bail!("Failed to install dependency: {}", dep);
                                }
                            } else {
                                anyhow::bail!(
                                    "Could not find package spec for dependency: {}",
                                    dep
                                );
                            }
                        }
                    }
                }

                // Enforce required dependencies before building/installing.
                deps::require_build_deps(&pkg_spec, &db_path)?;
                deps::require_runtime_deps(&pkg_spec, &db_path)?;
                if needs_test_deps {
                    deps::require_test_deps(&pkg_spec, &db_path)?;
                }
            }

            let cross_config = cli
                .cross_prefix
                .as_ref()
                .map(|p| cross::CrossConfig::from_prefix(p))
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

                if !cli.lib32_only {
                    builder::build(
                        &pkg_spec,
                        &src_dir,
                        &destdir,
                        cross_config.as_ref(),
                        !cli.no_flags,
                    )?;

                    // 3.1 Copy license files into staged tree
                    staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;
                    install::scripts::stage_scripts_from_spec_dir(&pkg_spec, &destdir)?;
                }

                destdir
            };

            if !cli.lib32_only {
                // 4. Stage (clean .la files, etc.)
                staging::process(&destdir, &pkg_spec)?;

                // 5-6. Install/update to rootfs and register in DB
                for out in pkg_spec.outputs() {
                    let mut spec_for_out = pkg_spec.clone();
                    let output_name = out.name.clone();
                    spec_for_out.package = out;
                    spec_for_out.alternatives = pkg_spec.alternatives_for_output(&output_name);
                    spec_for_out.dependencies = pkg_spec.dependencies_for_output(&output_name);
                    let out_destdir =
                        output_destdir_for(&destdir, &pkg_spec.package.name, &output_name);
                    install_staged_to_rootfs(&spec_for_out, &out_destdir, &cli.rootfs, &config)?;

                    ui::success(format!(
                        "Successfully installed {} v{}",
                        spec_for_out.package.name, spec_for_out.package.version
                    ));
                    // TODO(snapper): create post-install snapshot after install commit succeeds.
                }
            }

            if let Some(src_dir) = built_src_dir.as_deref()
                && let Some((lib32_spec, lib32_destdir)) = build_lib32_companion_package(
                    &pkg_spec,
                    src_dir,
                    &config,
                    cross_config.as_ref(),
                    !cli.no_flags,
                    cli.lib32_only,
                )?
            {
                install_staged_to_rootfs(&lib32_spec, &lib32_destdir, &cli.rootfs, &config)?;
                ui::success(format!(
                    "Successfully installed {} v{}",
                    lib32_spec.package.name, lib32_spec.package.version
                ));
            }

            if cli.clean {
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

            // Check build dependencies
            if !cli.no_deps {
                let needs_test_deps =
                    matches!(pkg_spec.build.build_type, package::BuildType::Autotools);
                deps::print_dep_status(&pkg_spec, &db_path)?;
                let mut missing = deps::check_build_deps(&pkg_spec, &db_path)?;
                if needs_test_deps {
                    for dep in deps::check_test_deps(&pkg_spec, &db_path)? {
                        if !missing.contains(&dep) {
                            missing.push(dep);
                        }
                    }
                }
                if !missing.is_empty() {
                    ui::warn(format!(
                        "Missing build dependencies: {}",
                        missing.join(", ")
                    ));
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
                    if !ui::prompt_yes_no("Install missing build dependencies first?", true)? {
                        anyhow::bail!("Aborted");
                    }
                    if cli.dry_run {
                        ui::info("Dry run enabled, stopping before dependency installation/build.");
                        return Ok(());
                    }
                    execute_install_plan_with_child_commands(
                        &dep_plan,
                        &cli.rootfs,
                        cli.no_flags,
                        cli.cross_prefix.as_deref(),
                        cli.clean,
                        cli.dry_run,
                        &config,
                    )?;
                }
                deps::require_build_deps(&pkg_spec, &db_path)?;
                if needs_test_deps {
                    deps::require_test_deps(&pkg_spec, &db_path)?;
                }
            } else if cli.dry_run {
                ui::info("Dry run enabled, stopping before build.");
                return Ok(());
            }

            let mut build_lock = locking::open_lock(&config)?;
            let build_lock_path = locking::lock_path(&config);
            let _build_lock_guard = locking::try_write(&mut build_lock, &build_lock_path, "build")?;

            if !ui::prompt_yes_no(
                &format!(
                    "Proceed with build for {} v{}-{}?",
                    pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
                ),
                true,
            )? {
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
                if !ui::prompt_yes_no("Install built package(s) to rootfs now?", true)? {
                    anyhow::bail!("Aborted");
                }
                if !cli.lib32_only {
                    for out in pkg_spec.outputs() {
                        let mut spec_for_out = pkg_spec.clone();
                        let output_name = out.name.clone();
                        spec_for_out.package = out;
                        spec_for_out.alternatives = pkg_spec.alternatives_for_output(&output_name);
                        spec_for_out.dependencies = pkg_spec.dependencies_for_output(&output_name);
                        let out_destdir =
                            output_destdir_for(&destdir, &pkg_spec.package.name, &output_name);
                        install_staged_to_rootfs(
                            &spec_for_out,
                            &out_destdir,
                            &cli.rootfs,
                            &config,
                        )?;
                        ui::success(format!(
                            "Successfully installed {} v{}",
                            spec_for_out.package.name, spec_for_out.package.version
                        ));
                        // TODO(snapper): create post-install snapshot after --install commit succeeds.
                    }
                }
                if let Some((lib32_spec, lib32_destdir)) = &lib32_install_bundle {
                    install_staged_to_rootfs(lib32_spec, lib32_destdir, &cli.rootfs, &config)?;
                    ui::success(format!(
                        "Successfully installed {} v{}",
                        lib32_spec.package.name, lib32_spec.package.version
                    ));
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
        Commands::Sign { file } => {
            let sig_path = signing::sign_zst_file_detached(&cli.rootfs, &file)?;
            ui::success(format!(
                "Created detached signature: {}",
                sig_path.display()
            ));
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
