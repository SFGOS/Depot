use crate::cli::BootstrapArgs;
use crate::{config, source, system_state, ui};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sys_mount::{Mount, MountFlags, Unmount, UnmountFlags};
use url::Url;

const TEMP_LAYER: &str = "temp";
const BASE_LAYER: &str = "base";
const DEVEL_LAYER: &str = "devel";
const FILESYSTEM_PACKAGE: &str = "filesystem";
const BOOTSTRAP_DIR: &str = "bootstrap/lbi";
const DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS: &str = "DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS";
const CHAPTER7_RETIRED_PACKAGES: &[&str] = &["llvm-clang-pass1"];
const BOOTSTRAP_CHROOT_SHIM_DIR: &str = "/tmp/depot-bootstrap-tools";
const BOOTSTRAP_CHROOT_PATH: &str = "/system/tools/bin:/system/binaries:/system/systembinaries:/tmp/depot-bootstrap-tools:/bin:/usr/bin:/sbin:/usr/sbin";
const BOOK_FETCH_CACHE_BUST_PARAM: &str = "depot_refresh";

#[derive(Debug, Clone, PartialEq, Eq)]
struct BookPackage {
    chapter: u8,
    section: String,
    title: String,
    name: String,
    version: String,
    layer: String,
    page_url: String,
    recipe_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BookOperationKind {
    ResetTargetTreeOwnership,
    CreateVirtualFilesystemLinkTargets,
    CopyBuildProfile,
    EnterChroot,
    CreateEssentialSystemFiles,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BookOperation {
    section: String,
    title: String,
    kind: BookOperationKind,
    recipe_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BookStep {
    Package(BookPackage),
    Operation(BookOperation),
}

#[derive(Debug, Clone)]
struct GeneratedRecipe {
    package: BookPackage,
    spec_path: PathBuf,
    progress_path: PathBuf,
}

#[derive(Debug, Clone)]
struct PageRecipe {
    input_files: Vec<String>,
    source_urls: Vec<String>,
    extract_dir: Option<String>,
    commands: Vec<String>,
    dependencies: Vec<String>,
    license: String,
    description: String,
}

#[derive(Debug, Clone)]
struct ManifestEntry {
    url: String,
    output_name: String,
}

#[derive(Debug, Clone, Default)]
struct SourceManifest {
    entries: Vec<ManifestEntry>,
    blake2b_512: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapBuildMode {
    Host,
    Cross,
    Chroot,
}

#[derive(Debug, Clone)]
struct BootstrapInstallInvocation {
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

pub(crate) fn run(args: BootstrapArgs) -> Result<()> {
    let config = config::Config::for_rootfs(&args.sysroot);
    let (target, arch) = bootstrap_target_arch(&args, &config)?;
    ensure_lbi_layout_for_fresh_bootstrap(&args.sysroot, &config, &target, &arch)?;
    let pdf = load_book_pdf(&args)?;
    let text =
        pdf_extract::extract_text_from_mem(&pdf).context("Failed to extract text from book PDF")?;
    let steps = parse_book_steps(&text, &args.book_url)?;
    let packages = packages_from_steps(&steps);
    if packages.is_empty() {
        anyhow::bail!("No package sections were found in the Linux by Intent book");
    }

    let recipes = generate_recipes(&args, &config, &packages, &target, &arch)?;
    let recipes_by_id = recipes
        .iter()
        .map(|recipe| (recipe.package.recipe_id.as_str(), recipe))
        .collect::<BTreeMap<_, _>>();
    let progress_root = bootstrap_progress_root(&config);

    for (idx, step) in steps.iter().enumerate() {
        if step_requires_root(step)
            && !step_is_done(step, &recipes_by_id, &progress_root, &args.sysroot, &config)
            && !crate::fakeroot::is_root()
        {
            ensure_root_for_bootstrap()?;
            return Ok(());
        }

        match step {
            BookStep::Package(package) => {
                let recipe = recipes_by_id
                    .get(package.recipe_id.as_str())
                    .copied()
                    .with_context(|| format!("Missing generated recipe for {}", package.title))?;
                let mode = build_mode_for_package(package);
                ui::merge_package(&package.layer, &package.name);
                install_recipe(&args.sysroot, &config, recipe, mode, &target)?;
                system_state::set_stage(&config, stage_for_step(step))?;
            }
            BookStep::Operation(operation) => {
                run_operation_step(&args.sysroot, &config, operation, &target, &arch)?;
                system_state::set_stage(&config, stage_for_step(step))?;
            }
        }

        if step_completes_chapter(step, steps.get(idx + 1), 7) {
            retire_packages_after_chapter7(&args.sysroot, &config)?;
            system_state::set_stage(&config, "bootstrap-chapter7-cleanup".to_string())?;
        }
    }

    let layers = packages_by_layer(&packages);
    for layer in [TEMP_LAYER, BASE_LAYER, DEVEL_LAYER] {
        let packages = bootstrap_layer_packages_for_state(&layers, layer);
        system_state::set_layer_packages(&config, layer.to_string(), &packages)?;
    }

    system_state::set_stage(&config, "bootstrap-complete".to_string())?;
    ui::success(format!(
        "Bootstrapped {} Linux by Intent package(s) through chapter 9",
        packages.len()
    ));
    Ok(())
}

fn bootstrap_target_arch(
    args: &BootstrapArgs,
    config: &config::Config,
) -> Result<(String, String)> {
    let state = system_state::load(config)?;
    let target = args
        .target
        .clone()
        .or(state.target)
        .unwrap_or_else(|| "x86_64-lbi-linux-musl".to_string());
    let arch = args
        .arch
        .clone()
        .or(state.arch)
        .unwrap_or_else(|| crate::cross::target_arch_from_triple(&target).to_string());
    Ok((target, arch))
}

fn ensure_lbi_layout_for_fresh_bootstrap(
    sysroot: &Path,
    config: &config::Config,
    target: &str,
    arch: &str,
) -> Result<()> {
    let state = system_state::load(config)?;
    system_state::ensure_lbi_layout_paths(sysroot)?;
    if state.stage.is_some()
        || !state.layers.is_empty()
        || sysroot.join("etc/depot.d/build.toml").exists()
    {
        ui::info("Resuming existing bootstrap state");
        return Ok(());
    }

    ui::info("Initializing Linux by Intent sysroot layout");
    system_state::init_lbi_layout(sysroot, config, target, Some(arch), false)?;
    Ok(())
}

fn install_recipe(
    sysroot: &Path,
    config: &config::Config,
    recipe: &GeneratedRecipe,
    mode: BootstrapBuildMode,
    target: &str,
) -> Result<()> {
    if bootstrap_package_is_complete(sysroot, config, recipe)? {
        ui::info(format!(
            "Skipping already completed package {} ({})",
            recipe.package.name, recipe.package.section
        ));
        return Ok(());
    }

    prepare_bootstrap_package_install(sysroot, config, &recipe.package)?;
    let exe = std::env::current_exe().context("Failed to locate depot executable")?;
    let invocation = bootstrap_install_invocation(sysroot, recipe, mode, target, &exe)?;
    let mut cmd = Command::new(&exe);
    cmd.args(&invocation.args);
    for (key, value) in &invocation.env {
        cmd.env(key, value);
    }
    let status = cmd
        .status()
        .with_context(|| format!("Failed to run depot install for {}", recipe.package.name))?;
    if !status.success() {
        anyhow::bail!(
            "depot install failed for {} with status {status}",
            recipe.package.name
        );
    }
    if let Some(parent) = recipe.progress_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut progress = format!(
        "section = \"{}\"\npackage = \"{}\"\nlayer = \"{}\"\n",
        recipe.package.section, recipe.package.name, recipe.package.layer
    );
    if let Some(revision) = bootstrap_recipe_revision(&recipe.package.name) {
        progress.push_str(&format!("recipe_revision = {revision}\n"));
    }
    fs::write(&recipe.progress_path, progress)
        .with_context(|| format!("Failed to write {}", recipe.progress_path.display()))?;
    Ok(())
}

fn bootstrap_package_is_complete(
    sysroot: &Path,
    config: &config::Config,
    recipe: &GeneratedRecipe,
) -> Result<bool> {
    if package_is_retired(config, &recipe.package.name) {
        return Ok(true);
    }

    if !recipe.progress_path.exists() {
        return Ok(false);
    }

    if let Some(revision) = bootstrap_recipe_revision(&recipe.package.name) {
        let progress = fs::read_to_string(&recipe.progress_path)
            .with_context(|| format!("Failed to read {}", recipe.progress_path.display()))?;
        let expected = format!("recipe_revision = {revision}");
        if !progress.lines().any(|line| line.trim() == expected) {
            ui::info(format!(
                "Reinstalling {} because its bootstrap recipe changed",
                recipe.package.name
            ));
            return Ok(false);
        }
    }

    let db_path = config.installed_db_path(sysroot);
    let replaced = crate::db::get_all_replaces(&db_path).with_context(|| {
        format!(
            "Failed to inspect replacement metadata for {}",
            recipe.package.name
        )
    })?;
    if replaced.contains(&recipe.package.name) {
        return Ok(true);
    }

    let files =
        crate::db::get_package_files(&db_path, &recipe.package.name).with_context(|| {
            format!(
                "Failed to inspect installed files for {}",
                recipe.package.name
            )
        })?;
    if files.is_empty() {
        ui::info(format!(
            "Reinstalling {} because bootstrap progress exists but no installed files were recorded",
            recipe.package.name
        ));
        return Ok(false);
    }

    for rel_path in files {
        if crate::staging::is_purged_payload_path(&rel_path) {
            continue;
        }
        let disk_path = sysroot.join(&rel_path);
        if disk_path.symlink_metadata().is_err() {
            ui::info(format!(
                "Reinstalling {} because {} is missing from the sysroot",
                recipe.package.name, rel_path
            ));
            return Ok(false);
        }
    }

    for rel_path in bootstrap_required_payload_paths(&recipe.package.name) {
        let disk_path = sysroot.join(rel_path);
        if !disk_path.exists() {
            ui::info(format!(
                "Reinstalling {} because required bootstrap payload {} is missing from the sysroot",
                recipe.package.name, rel_path
            ));
            return Ok(false);
        }
    }

    Ok(true)
}

fn bootstrap_required_payload_paths(package: &str) -> &'static [&'static str] {
    match package {
        "bmake" => &["system/binaries/bmake", "system/share/mk/sys.mk"],
        "byacc" => &["system/binaries/yacc"],
        "llvm-clang-pass1" => &[
            "system/tools/bin/llvm-config",
            "system/tools/bin/llvm-tblgen",
            "system/tools/bin/clang-tblgen",
        ],
        "ubase" => &["system/binaries/id"],
        _ => &[],
    }
}

fn bootstrap_recipe_revision(package: &str) -> Option<u32> {
    match package {
        "bmake" => Some(2),
        "byacc" => Some(1),
        "llvm" => Some(3),
        "ubase" => Some(1),
        _ => None,
    }
}

const BSDDIFFUTILS_SBASE_HANDOFF_PATHS: &[&str] = &[
    "system/binaries/cmp",
    "system/documentation/man-pages/man1/cmp.1",
];

const BSDGREP_SBASE_HANDOFF_PATHS: &[&str] = &[
    "system/binaries/grep",
    "system/documentation/man-pages/man1/grep.1",
];

fn prepare_bootstrap_package_install(
    sysroot: &Path,
    config: &config::Config,
    package: &BookPackage,
) -> Result<()> {
    let handoff_paths = match package.name.as_str() {
        "bsddiffutils" => BSDDIFFUTILS_SBASE_HANDOFF_PATHS,
        "bsdgrep" => BSDGREP_SBASE_HANDOFF_PATHS,
        _ => return Ok(()),
    };

    if handoff_paths.is_empty() {
        return Ok(());
    }

    let db_path = config.installed_db_path(sysroot);
    if !db_path.exists() {
        return Ok(());
    }

    let mut conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("Failed to open package database {}", db_path.display()))?;
    let tx = conn.transaction()?;

    for rel_path in handoff_paths {
        let owner = match tx.query_row(
            "SELECT p.name FROM files f JOIN packages p ON f.package_id = p.id WHERE f.path = ?1",
            rusqlite::params![rel_path],
            |row| row.get::<_, String>(0),
        ) {
            Ok(owner) => owner,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(err) => return Err(err).context("Failed to query package file ownership"),
        };

        if owner != "sbase" {
            continue;
        }

        tx.execute(
            "DELETE FROM files WHERE path = ?1 AND package_id = (SELECT id FROM packages WHERE name = 'sbase')",
            rusqlite::params![rel_path],
        )
        .with_context(|| format!("Failed to clear sbase ownership for {rel_path}"))?;

        let disk_path = sysroot.join(rel_path);
        if disk_path.exists() {
            fs::remove_file(&disk_path)
                .with_context(|| format!("Failed to remove {}", disk_path.display()))?;
        }
        ui::info(format!(
            "Handed off {} from sbase to {}",
            rel_path, package.name
        ));
    }

    tx.commit()
        .with_context(|| format!("Failed to update package database {}", db_path.display()))?;
    Ok(())
}

fn bootstrap_install_invocation(
    sysroot: &Path,
    recipe: &GeneratedRecipe,
    mode: BootstrapBuildMode,
    target: &str,
    depot_exe: &Path,
) -> Result<BootstrapInstallInvocation> {
    let mut args = vec![
        OsString::from("install"),
        OsString::from("-r"),
        sysroot.as_os_str().to_os_string(),
        OsString::from("--yes"),
        OsString::from("--no-deps"),
    ];
    if mode == BootstrapBuildMode::Cross {
        args.push(OsString::from("--cross-prefix"));
        args.push(OsString::from(target));
    }
    args.push(recipe.spec_path.as_os_str().to_os_string());

    let mut env = Vec::new();
    if matches!(mode, BootstrapBuildMode::Cross | BootstrapBuildMode::Chroot) {
        env.push((
            OsString::from("PATH"),
            bootstrap_tool_path(
                sysroot,
                std::env::var_os("PATH").as_deref(),
                mode == BootstrapBuildMode::Chroot,
            )?,
        ));
    }
    env.push((
        OsString::from("DEPOT_LBI_SYSROOT"),
        bootstrap_sysroot_env_path(sysroot)?.into_os_string(),
    ));
    env.push((
        OsString::from("LWI_MAKE_FLAGS"),
        OsString::from(bootstrap_parallel_makeflags()),
    ));
    env.push((
        OsString::from("LWI_MAKE_JOBS"),
        OsString::from(bootstrap_parallel_make_jobs()),
    ));
    env.push((
        OsString::from(DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS),
        OsString::from("1"),
    ));
    if mode == BootstrapBuildMode::Chroot {
        env.extend([
            (OsString::from("DEPOT_LBI_CHROOT"), OsString::from("1")),
            (
                OsString::from("DEPOT_LBI_CHROOT_ROOT"),
                sysroot.as_os_str().to_os_string(),
            ),
            (
                OsString::from("DEPOT_LBI_DEPOT_EXE"),
                depot_exe.as_os_str().to_os_string(),
            ),
        ]);
    }

    Ok(BootstrapInstallInvocation { args, env })
}

fn bootstrap_sysroot_env_path(sysroot: &Path) -> Result<PathBuf> {
    if sysroot.is_absolute() {
        Ok(sysroot.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("Failed to resolve current directory for bootstrap sysroot")?
            .join(sysroot))
    }
}

fn bootstrap_tool_path(
    sysroot: &Path,
    inherited_path: Option<&OsStr>,
    include_target_paths: bool,
) -> Result<OsString> {
    let mut paths = vec![
        sysroot.join("system/tools/bin"),
        sysroot.join("system/tools/sbin"),
    ];
    if include_target_paths {
        paths.extend([
            sysroot.join("system/binaries"),
            sysroot.join("system/systembinaries"),
        ]);
    }
    if let Some(inherited) = inherited_path {
        paths.extend(std::env::split_paths(inherited));
    }
    std::env::join_paths(paths).context("Failed to construct bootstrap PATH")
}

fn bootstrap_parallel_makeflags() -> String {
    if let Ok(value) = std::env::var("LWI_MAKE_FLAGS") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    format!("-j{}", bootstrap_parallel_make_jobs())
}

fn bootstrap_parallel_make_jobs() -> String {
    if let Ok(value) = std::env::var("LWI_MAKE_JOBS") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .to_string()
}

fn bootstrap_progress_root(config: &config::Config) -> PathBuf {
    config.db_dir.join(BOOTSTRAP_DIR).join("progress")
}

fn retired_package_progress_path(config: &config::Config, package: &str) -> PathBuf {
    bootstrap_progress_root(config).join(format!("retired-{package}.done"))
}

fn package_is_retired(config: &config::Config, package: &str) -> bool {
    retired_package_progress_path(config, package).exists()
}

fn step_is_done(
    step: &BookStep,
    recipes: &BTreeMap<&str, &GeneratedRecipe>,
    progress_root: &Path,
    sysroot: &Path,
    config: &config::Config,
) -> bool {
    match step {
        BookStep::Package(package) => {
            recipes
                .get(package.recipe_id.as_str())
                .is_some_and(|recipe| {
                    bootstrap_package_is_complete(sysroot, config, recipe).unwrap_or(false)
                })
        }
        BookStep::Operation(operation) => {
            operation_is_complete(progress_root, operation, sysroot, config).unwrap_or(false)
        }
    }
}

fn step_chapter(step: &BookStep) -> Option<u8> {
    section_chapter(step.section())
}

fn step_completes_chapter(step: &BookStep, next: Option<&BookStep>, chapter: u8) -> bool {
    step_chapter(step) == Some(chapter) && next.and_then(step_chapter) != Some(chapter)
}

fn step_requires_root(step: &BookStep) -> bool {
    match step {
        BookStep::Package(package) => build_mode_for_package(package) == BootstrapBuildMode::Chroot,
        BookStep::Operation(_) => true,
    }
}

fn build_mode_for_package(package: &BookPackage) -> BootstrapBuildMode {
    if use_cross_toolchain_by_default(package) {
        return BootstrapBuildMode::Cross;
    }

    match package.chapter {
        5 => BootstrapBuildMode::Host,
        6 => BootstrapBuildMode::Cross,
        7..=9 => BootstrapBuildMode::Chroot,
        _ => BootstrapBuildMode::Host,
    }
}

fn stage_for_step(step: &BookStep) -> String {
    match step {
        BookStep::Package(package) => format!("bootstrap-{}", package.recipe_id),
        BookStep::Operation(operation) => format!("bootstrap-{}", operation.recipe_id),
    }
}

fn operation_progress_path(progress_root: &Path, operation: &BookOperation) -> PathBuf {
    progress_root.join(format!("{}.done", operation.recipe_id))
}

fn operation_is_complete(
    progress_root: &Path,
    operation: &BookOperation,
    sysroot: &Path,
    config: &config::Config,
) -> Result<bool> {
    if !operation_progress_path(progress_root, operation).exists() {
        return Ok(false);
    }

    match operation.kind {
        BookOperationKind::CreateEssentialSystemFiles => {
            filesystem_package_is_registered(sysroot, config)
        }
        _ => Ok(true),
    }
}

fn retire_packages_after_chapter7(sysroot: &Path, config: &config::Config) -> Result<()> {
    let db_path = config.installed_db_path(sysroot);
    let installed = crate::db::get_installed_packages(&db_path).with_context(|| {
        format!(
            "Failed to inspect installed packages in {}",
            db_path.display()
        )
    })?;
    let needs_removal = CHAPTER7_RETIRED_PACKAGES
        .iter()
        .any(|package| installed.contains(*package));

    if needs_removal && !crate::fakeroot::is_root() {
        ensure_root_for_bootstrap()?;
    }

    for package in CHAPTER7_RETIRED_PACKAGES {
        if installed.contains(*package) {
            ui::info(format!(
                "Retiring bootstrap-only package {package} after chapter 7"
            ));
            crate::commands::remove_installed_package_with_hooks(package, sysroot, config)
                .with_context(|| format!("Failed to retire bootstrap-only package {package}"))?;
        }

        let progress_path = retired_package_progress_path(config, package);
        if let Some(parent) = progress_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::write(
            &progress_path,
            "reason = \"chapter7-complete\"\nretired = true\n",
        )
        .with_context(|| format!("Failed to write {}", progress_path.display()))?;
    }

    Ok(())
}

fn run_operation_step(
    sysroot: &Path,
    config: &config::Config,
    operation: &BookOperation,
    target: &str,
    arch: &str,
) -> Result<()> {
    let progress_path = operation_progress_path(&bootstrap_progress_root(config), operation);
    if operation_is_complete(&bootstrap_progress_root(config), operation, sysroot, config)? {
        ui::info(format!(
            "Skipping already completed bootstrap step {} ({})",
            operation.title, operation.section
        ));
        return Ok(());
    }

    ui::info(format!("Running bootstrap step: {}", operation.title));
    match operation.kind {
        BookOperationKind::ResetTargetTreeOwnership => reset_target_tree_ownership(sysroot)?,
        BookOperationKind::CreateVirtualFilesystemLinkTargets => {
            create_virtual_filesystem_link_targets(sysroot)?
        }
        BookOperationKind::CopyBuildProfile => copy_build_profile(sysroot, target, arch)?,
        BookOperationKind::EnterChroot => mark_chroot_entry(sysroot)?,
        BookOperationKind::CreateEssentialSystemFiles => {
            create_essential_system_files(sysroot)?;
            register_filesystem_package(sysroot, config)?;
        }
    }

    if let Some(parent) = progress_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(
        &progress_path,
        format!(
            "section = \"{}\"\noperation = \"{}\"\n",
            operation.section,
            operation_slug(operation.kind)
        ),
    )
    .with_context(|| format!("Failed to write {}", progress_path.display()))?;
    Ok(())
}

fn reset_target_tree_ownership(sysroot: &Path) -> Result<()> {
    let status = Command::new("chown")
        .arg("-R")
        .arg("0:0")
        .arg(sysroot)
        .status()
        .with_context(|| format!("Failed to run chown for {}", sysroot.display()))?;
    if !status.success() {
        anyhow::bail!(
            "Failed to reset target tree ownership for {}: chown exited with {}",
            sysroot.display(),
            status
        );
    }
    Ok(())
}

fn create_virtual_filesystem_link_targets(sysroot: &Path) -> Result<()> {
    system_state::ensure_lbi_layout_paths(sysroot)?;
    for rel in [
        "system/devices/pts",
        "system/devices/shm",
        "system/temporary",
    ] {
        let path = sysroot.join(rel);
        fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
    }
    Ok(())
}

fn copy_build_profile(sysroot: &Path, target: &str, arch: &str) -> Result<()> {
    let profile_dir = sysroot.join("etc/profile.d");
    fs::create_dir_all(&profile_dir)
        .with_context(|| format!("Failed to create {}", profile_dir.display()))?;
    let profile_path = profile_dir.join("lbi-build.sh");
    let content = format!(
        r#"# Generated by `depot bootstrap`.
export LBI_TARGET="{target}"
export LBI_ARCH="{arch}"
export CHOST="$LBI_TARGET"
export CARCH="$LBI_ARCH"
export LBI_ROOT="/"
export LBI_SOURCES="/sources"
export PATH="/system/tools/bin:/system/binaries:/system/systembinaries:/bin:/usr/bin:/sbin:/usr/sbin"

lbi_configure() {{
    ./configure \
        --target="$LBI_TARGET" \
        --host="$LBI_TARGET" \
        --prefix=/system \
        --bindir=/system/binaries \
        --sbindir=/system/systembinaries \
        --libdir=/system/libraries \
        --includedir=/system/headers \
        --sysconfdir=/system/configuration \
        --localstatedir=/system/variable \
        --datarootdir=/system/share \
        --mandir=/system/documentation/man-pages \
        --infodir=/system/documentation/info \
        "$@"
}}
"#
    );
    fs::write(&profile_path, content)
        .with_context(|| format!("Failed to write {}", profile_path.display()))?;
    Ok(())
}

fn mark_chroot_entry(sysroot: &Path) -> Result<()> {
    let marker = sysroot.join("var/lib/depot/bootstrap-chroot-ready");
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(&marker, "ready\n").with_context(|| format!("Failed to write {}", marker.display()))
}

fn create_essential_system_files(sysroot: &Path) -> Result<()> {
    write_if_missing(
        &sysroot.join("etc/passwd"),
        "root:x:0:0:root:/system/charlie:/bin/oksh\n\
bin:x:1:1:bin:/dev/null:/system/binaries/false\n\
daemon:x:6:6:Daemon User:/dev/null:/system/binaries/false\n\
messagebus:x:18:18:D-Bus Message Daemon User:/run/dbus:/system/binaries/false\n\
uuidd:x:80:80:UUID Generation Daemon User:/dev/null:/system/binaries/false\n\
nobody:x:65534:65534:Unprivileged User:/dev/null:/system/binaries/false\n",
    )?;
    write_if_missing(
        &sysroot.join("etc/group"),
        "root:x:0:\n\
bin:x:1:daemon\n\
sys:x:2:\n\
kmem:x:3:\n\
tape:x:4:\n\
tty:x:5:\n\
daemon:x:6:\n\
floppy:x:7:\n\
disk:x:8:\n\
lp:x:9:\n\
dialout:x:10:\n\
audio:x:11:\n\
video:x:12:\n\
utmp:x:13:\n\
clock:x:14:\n\
cdrom:x:15:\n\
adm:x:16:\n\
messagebus:x:18:\n\
input:x:24:\n\
mail:x:34:\n\
kvm:x:61:\n\
uuidd:x:80:\n\
wheel:x:97:\n\
users:x:999:\n\
nogroup:x:65534:\n",
    )?;
    write_if_missing(
        &sysroot.join("etc/hosts"),
        "127.0.0.1 localhost\n::1 localhost\n",
    )?;
    write_if_missing(
        &sysroot.join("etc/fstab"),
        "# file system mount point type options dump pass\n",
    )?;
    write_if_missing(
        &sysroot.join("etc/shells"),
        "/bin/sh\n/system/binaries/sh\n",
    )?;
    Ok(())
}

fn filesystem_package_is_registered(sysroot: &Path, config: &config::Config) -> Result<bool> {
    let installed = crate::db::get_installed_packages(&config.installed_db_path(sysroot))?;
    Ok(installed.contains(FILESYSTEM_PACKAGE))
}

fn register_filesystem_package(sysroot: &Path, config: &config::Config) -> Result<()> {
    ui::info("Registering system layout as package filesystem");
    system_state::ensure_lbi_layout_paths(sysroot)?;
    create_essential_system_files(sysroot)?;
    let staged = tempfile::tempdir().context("Failed to create filesystem package staging dir")?;
    system_state::ensure_lbi_layout_paths(staged.path())?;
    create_essential_system_files(staged.path())?;
    crate::db::register_package(
        &config.installed_db_path(sysroot),
        &filesystem_package_spec(),
        staged.path(),
    )
    .context("Failed to register filesystem package")
}

fn filesystem_package_spec() -> crate::package::PackageSpec {
    crate::package::PackageSpec {
        package: crate::package::PackageInfo {
            name: FILESYSTEM_PACKAGE.to_string(),
            real_name: None,
            version: "1.0".to_string(),
            revision: 1,
            description: "Linux by Intent system layout".to_string(),
            homepage: "https://www.vertexlinux.net/lbi/".to_string(),
            abi_breaking: false,
            license: vec!["MIT".to_string()],
        },
        packages: Vec::new(),
        alternatives: crate::package::Alternatives::default(),
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: crate::package::Build {
            build_type: crate::package::BuildType::Meta,
            flags: crate::package::BuildFlags::default(),
        },
        dependencies: crate::package::Dependencies {
            groups: vec![BASE_LAYER.to_string()],
            ..crate::package::Dependencies::default()
        },
        package_alternatives: BTreeMap::new(),
        package_dependencies: BTreeMap::new(),
        spec_dir: PathBuf::new(),
    }
}

fn write_if_missing(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RootTransition {
    AlreadyRoot,
    Reexec(PathBuf),
}

fn ensure_root_for_bootstrap() -> Result<()> {
    match root_transition(
        crate::fakeroot::is_root(),
        std::env::var_os("PATH").as_deref(),
    )? {
        RootTransition::AlreadyRoot => Ok(()),
        RootTransition::Reexec(helper) => {
            let exe = std::env::current_exe().context("Failed to locate depot executable")?;
            ui::info(format!(
                "Re-executing bootstrap through {} for root-owned and chroot steps",
                helper.display()
            ));
            let status = Command::new(&helper)
                .arg(exe)
                .args(std::env::args_os().skip(1))
                .status()
                .with_context(|| {
                    format!(
                        "Failed to re-execute depot bootstrap via {}",
                        helper.display()
                    )
                })?;
            if status.success() {
                std::process::exit(0);
            }
            anyhow::bail!(
                "depot bootstrap via {} failed with status {}",
                helper.display(),
                status
            );
        }
    }
}

fn root_transition(is_root: bool, path: Option<&OsStr>) -> Result<RootTransition> {
    if is_root {
        return Ok(RootTransition::AlreadyRoot);
    }
    let Some(helper) = privilege_helper(path) else {
        anyhow::bail!(
            "Bootstrap needs root for ownership and chroot steps, but neither sudo nor doas was found in PATH"
        );
    };
    Ok(RootTransition::Reexec(helper))
}

fn privilege_helper(path: Option<&OsStr>) -> Option<PathBuf> {
    for name in ["sudo", "doas"] {
        if let Some(found) = find_executable_in_path(name, path) {
            return Some(found);
        }
    }
    None
}

fn find_executable_in_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(metadata) = fs::metadata(&candidate) else {
                continue;
            };
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(candidate);
    }
    None
}

#[derive(Default)]
struct BootstrapChrootMountGuard {
    mounted: Vec<Mount>,
    created_files: Vec<PathBuf>,
}

impl BootstrapChrootMountGuard {
    fn mount_path(
        &mut self,
        source: &Path,
        target: &Path,
        fstype: Option<&str>,
        flags: MountFlags,
        data: Option<&str>,
    ) -> Result<()> {
        let mut builder = Mount::builder().flags(flags);
        if let Some(fstype) = fstype {
            builder = builder.fstype(fstype);
        }
        if let Some(data) = data {
            builder = builder.data(data);
        }
        let mount = builder
            .mount(source, target)
            .with_context(|| format!("Failed to mount {}", target.display()))?;
        self.mounted.push(mount);
        Ok(())
    }

    fn prepare_file_mount_target(&mut self, target: &Path) -> Result<()> {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        match target.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_dir() => {
                anyhow::bail!("Mount target is a directory: {}", target.display());
            }
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::File::create(target)
                    .with_context(|| format!("Failed to create {}", target.display()))?;
                self.created_files.push(target.to_path_buf());
                Ok(())
            }
            Err(err) => Err(err).with_context(|| format!("Failed to inspect {}", target.display())),
        }
    }

    fn mount_host_resolver_config(&mut self, rootfs: &Path) -> Result<bool> {
        let source = Path::new("/etc/resolv.conf");
        if !source.is_file() {
            return Ok(false);
        }

        let target = rootfs.join("etc/resolv.conf");
        self.prepare_file_mount_target(&target)?;
        self.mount_path(source, &target, None, MountFlags::BIND, None)?;
        Ok(true)
    }
}

impl Drop for BootstrapChrootMountGuard {
    fn drop(&mut self) {
        for mount in self.mounted.iter().rev() {
            if mount.unmount(UnmountFlags::empty()).is_ok() {
                continue;
            }
            let _ = mount.unmount(UnmountFlags::DETACH);
        }
        for path in self.created_files.iter().rev() {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn run_bootstrap_chroot(
    rootfs: &Path,
    sources: &Path,
    destdir: &Path,
    workdir: &str,
    script: &str,
) -> Result<()> {
    if !crate::fakeroot::is_root() {
        anyhow::bail!("internal bootstrap-chroot requires root");
    }
    if !sources.is_dir() {
        anyhow::bail!(
            "Bootstrap source workspace is not a directory: {}",
            sources.display()
        );
    }
    fs::create_dir_all(destdir)
        .with_context(|| format!("Failed to create {}", destdir.display()))?;

    let _mounts = mount_bootstrap_chroot(rootfs, sources, destdir)?;
    install_bootstrap_chroot_tool_shims(rootfs)?;
    let command = format!(
        "cd {} && exec /bin/sh {}",
        shell_quote(workdir),
        shell_quote(script)
    );
    let mut cmd = Command::new("chroot");
    cmd.arg(rootfs)
        .arg("/bin/sh")
        .arg("-lc")
        .arg(command)
        .env("DEPOT_LBI_INSIDE_CHROOT", "1")
        .env("DESTDIR", "/destdir")
        .env("DEPOT_PRIMARY_DESTDIR", "/destdir")
        .env("DEPOT_LBI_SYSROOT", "/")
        .env("DEPOT_STARBUILD_WORKDIR", "/sources")
        .env("LBI_ROOT", "/destdir")
        .env("LBI_SYSROOT", "/")
        .env("LBI_SOURCES", "/sources")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    apply_bootstrap_chroot_tool_env(&mut cmd);
    let status = crate::interrupts::command_status(&mut cmd)
        .with_context(|| format!("Failed to execute bootstrap build in {}", rootfs.display()))?;
    if !status.success() {
        anyhow::bail!("Bootstrap chroot build failed with status {}", status);
    }
    Ok(())
}

fn bootstrap_chroot_tool_env() -> &'static [(&'static str, &'static str)] {
    &[
        ("CC", "cc"),
        ("CXX", "c++"),
        ("AR", "ar"),
        ("RANLIB", "ranlib"),
        ("NM", "nm"),
        ("STRIP", "strip"),
        ("PATH", BOOTSTRAP_CHROOT_PATH),
    ]
}

fn apply_bootstrap_chroot_tool_env(cmd: &mut Command) {
    for (key, value) in bootstrap_chroot_tool_env() {
        cmd.env(key, value);
    }
    cmd.env_remove("CROSS_COMPILE");
}

fn install_bootstrap_chroot_tool_shims(rootfs: &Path) -> Result<()> {
    let shim_dir = rootfs.join(BOOTSTRAP_CHROOT_SHIM_DIR.trim_start_matches('/'));
    fs::create_dir_all(&shim_dir)
        .with_context(|| format!("Failed to create {}", shim_dir.display()))?;

    let id_path = shim_dir.join("id");
    fs::write(
        &id_path,
        r#"#!/bin/sh
case "${1:-}" in
    "" )
        printf '%s\n' 'uid=0(root) gid=0(root) groups=0(root)'
        ;;
    -u )
        printf '%s\n' 0
        ;;
    -g )
        printf '%s\n' 0
        ;;
    -G )
        printf '%s\n' 0
        ;;
    -un )
        printf '%s\n' root
        ;;
    -gn )
        printf '%s\n' root
        ;;
    * )
        printf '%s\n' "id: unsupported bootstrap option: $1" >&2
        exit 1
        ;;
esac
"#,
    )
    .with_context(|| format!("Failed to write {}", id_path.display()))?;
    make_executable(&id_path)?;
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).with_context(|| format!("Failed to chmod {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn mount_bootstrap_chroot(
    rootfs: &Path,
    sources: &Path,
    destdir: &Path,
) -> Result<BootstrapChrootMountGuard> {
    for rel in [
        "proc", "dev", "dev/pts", "sys", "run", "tmp", "sources", "destdir",
    ] {
        let path = rootfs.join(rel);
        fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
    }
    set_world_writable_sticky(&rootfs.join("tmp"))?;

    let mut guard = BootstrapChrootMountGuard::default();
    guard.mount_path(
        Path::new("proc"),
        &rootfs.join("proc"),
        Some("proc"),
        MountFlags::NODEV | MountFlags::NOEXEC | MountFlags::NOSUID,
        None,
    )?;
    guard.mount_path(
        Path::new("/dev"),
        &rootfs.join("dev"),
        None,
        MountFlags::BIND,
        None,
    )?;
    guard.mount_path(
        Path::new("sysfs"),
        &rootfs.join("sys"),
        Some("sysfs"),
        MountFlags::NODEV | MountFlags::NOEXEC | MountFlags::NOSUID,
        None,
    )?;
    if let Err(_err) = guard.mount_path(
        Path::new("devpts"),
        &rootfs.join("dev/pts"),
        Some("devpts"),
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some("gid=5,mode=620"),
    ) {
        guard.mount_path(
            Path::new("devpts"),
            &rootfs.join("dev/pts"),
            Some("devpts"),
            MountFlags::NOSUID | MountFlags::NOEXEC,
            None,
        )?;
    }
    guard.mount_path(
        Path::new("/run"),
        &rootfs.join("run"),
        None,
        MountFlags::BIND,
        None,
    )?;
    guard.mount_path(
        sources,
        &rootfs.join("sources"),
        None,
        MountFlags::BIND,
        None,
    )?;
    guard.mount_path(
        destdir,
        &rootfs.join("destdir"),
        None,
        MountFlags::BIND,
        None,
    )?;
    if guard.mount_host_resolver_config(rootfs)? {
        ui::info("Mounted host resolver configuration for bootstrap chroot");
    }
    Ok(guard)
}

#[cfg(unix)]
fn set_world_writable_sticky(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o1777);
    fs::set_permissions(path, perms)
        .with_context(|| format!("Failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_world_writable_sticky(_path: &Path) -> Result<()> {
    Ok(())
}

fn shell_quote(input: &str) -> String {
    if input.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", input.replace('\'', "'\\''"))
}

fn load_book_pdf(args: &BootstrapArgs) -> Result<Vec<u8>> {
    if let Some(path) = &args.book_pdf {
        return fs::read(path).with_context(|| format!("Failed to read {}", path.display()));
    }

    ui::info(format!("Fetching Linux by Intent book: {}", args.book_url));
    let ua = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let client = source::build_blocking_client(&ua, Some(Duration::from_secs(60)))?;
    let fetch_url = fresh_book_fetch_url(&args.book_url)?;
    let mut response = client
        .get(fetch_url.as_str())
        .send()
        .with_context(|| format!("Failed to fetch {}", args.book_url))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("HTTP error fetching {}: {}", args.book_url, status);
    }
    let mut body = Vec::new();
    response
        .copy_to(&mut body)
        .with_context(|| format!("Failed to read {}", args.book_url))?;
    Ok(body)
}

fn generate_recipes(
    args: &BootstrapArgs,
    config: &config::Config,
    packages: &[BookPackage],
    target: &str,
    arch: &str,
) -> Result<Vec<GeneratedRecipe>> {
    let root = config.db_dir.join(BOOTSTRAP_DIR);
    let recipe_root = root.join("recipes");
    let page_root = root.join("pages");
    let progress_root = root.join("progress");
    fs::create_dir_all(&recipe_root)
        .with_context(|| format!("Failed to create {}", recipe_root.display()))?;
    fs::create_dir_all(&page_root)
        .with_context(|| format!("Failed to create {}", page_root.display()))?;
    fs::create_dir_all(&progress_root)
        .with_context(|| format!("Failed to create {}", progress_root.display()))?;

    let manifest = load_source_manifest(args, &root)?;
    let mut generated = Vec::new();
    for package in packages {
        let (actual_page_url, html) = fetch_package_page(package)?;
        let mut package = package.clone();
        package.page_url = actual_page_url;

        let page_path = page_root.join(format!("{}.html", package.recipe_id));
        fs::write(&page_path, &html)
            .with_context(|| format!("Failed to write {}", page_path.display()))?;

        let page_recipe = parse_page_recipe(&html, &package)?;
        let recipe_dir = recipe_root.join(&package.layer).join(&package.recipe_id);
        fs::create_dir_all(&recipe_dir)
            .with_context(|| format!("Failed to create {}", recipe_dir.display()))?;
        let spec_path = recipe_dir.join(format!("{}.toml", package.name));
        let build_path = recipe_dir.join("build.sh");
        write_generated_recipe(
            &spec_path,
            &build_path,
            &package,
            &page_recipe,
            &manifest,
            target,
            arch,
        )?;
        generated.push(GeneratedRecipe {
            package: package.clone(),
            spec_path,
            progress_path: progress_root.join(format!("{}.done", package.recipe_id)),
        });
    }
    Ok(generated)
}

fn load_source_manifest(args: &BootstrapArgs, root: &Path) -> Result<SourceManifest> {
    let manifest_url = book_base_url(&args.book_url)?.join("scripts/sources.manifest")?;
    let b2sums_url = book_base_url(&args.book_url)?.join("scripts/sources.b2sums")?;
    let ua = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let client = source::build_blocking_client(&ua, Some(Duration::from_secs(60)))?;
    let manifest_fetch_url = fresh_book_fetch_url(manifest_url.as_str())?;
    let b2sums_fetch_url = fresh_book_fetch_url(b2sums_url.as_str())?;
    ui::info(format!(
        "Fetching Linux by Intent source manifest: {manifest_url}"
    ));
    let body = client
        .get(manifest_fetch_url.as_str())
        .send()
        .with_context(|| format!("Failed to fetch {manifest_url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error fetching {manifest_url}"))?
        .text()
        .with_context(|| format!("Failed to read {manifest_url}"))?;
    let path = root.join("sources.manifest");
    fs::write(&path, &body).with_context(|| format!("Failed to write {}", path.display()))?;
    ui::info(format!(
        "Fetching Linux by Intent BLAKE2 source manifest: {b2sums_url}"
    ));
    let b2sums = client
        .get(b2sums_fetch_url.as_str())
        .send()
        .with_context(|| format!("Failed to fetch {b2sums_url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error fetching {b2sums_url}"))?
        .text()
        .with_context(|| format!("Failed to read {b2sums_url}"))?;
    let b2sums_path = root.join("sources.b2sums");
    fs::write(&b2sums_path, &b2sums)
        .with_context(|| format!("Failed to write {}", b2sums_path.display()))?;
    Ok(SourceManifest {
        entries: parse_source_manifest(&body),
        blake2b_512: parse_source_b2sums(&b2sums)?,
    })
}

fn fetch_page(url: &str) -> Result<String> {
    let ua = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let client = source::build_blocking_client(&ua, Some(Duration::from_secs(60)))?;
    let fetch_url = fresh_book_fetch_url(url)?;
    client
        .get(fetch_url.as_str())
        .send()
        .with_context(|| format!("Failed to fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error fetching {url}"))?
        .text()
        .with_context(|| format!("Failed to read {url}"))
}

fn fresh_book_fetch_url(url: &str) -> Result<String> {
    let mut parsed = Url::parse(url).with_context(|| format!("Invalid book fetch URL: {url}"))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before UNIX epoch")?
        .as_secs()
        .to_string();
    parsed
        .query_pairs_mut()
        .append_pair(BOOK_FETCH_CACHE_BUST_PARAM, &stamp);
    Ok(parsed.to_string())
}

fn fetch_package_page(package: &BookPackage) -> Result<(String, String)> {
    match fetch_page(&package.page_url) {
        Ok(html) => Ok((package.page_url.clone(), html)),
        Err(primary_err) => {
            let Some(fallback_url) = fallback_package_page_url(package)? else {
                return Err(primary_err);
            };

            ui::info(format!(
                "Primary page fetch failed for {}; trying fallback {}",
                package.page_url, fallback_url
            ));

            fetch_page(&fallback_url)
                .map(|html| (fallback_url, html))
                .with_context(|| {
                    format!(
                        "Failed to fetch package page for {}. Primary URL was {}; fallback URL was tried after primary error: {primary_err}",
                        package.name,
                        package.page_url
                    )
                })
        }
    }
}

fn fallback_package_page_url(package: &BookPackage) -> Result<Option<String>> {
    if !package.page_url.ends_with("-stage2.html") {
        return Ok(None);
    }

    let mut url = Url::parse(&package.page_url)
        .with_context(|| format!("Invalid package page URL: {}", package.page_url))?;

    let path = url.path().to_string();
    let fallback_path = path.replace(
        &format!("{}-stage2.html", package.name),
        &format!("{}.html", package.name),
    );

    if fallback_path == path {
        return Ok(None);
    }

    url.set_path(&fallback_path);
    Ok(Some(url.to_string()))
}

fn rewrite_make_flags(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let indent_len = line.len() - trimmed.len();
            let indent = &line[..indent_len];

            if trimmed == "make" {
                format!("{indent}make ${{LWI_MAKE_FLAGS:-}}")
            } else if let Some(rest) = trimmed.strip_prefix("make ") {
                if rest.contains("LWI_MAKE_FLAGS") || rest.contains("MAKEFLAGS") {
                    line.to_string()
                } else {
                    format!("{indent}make ${{LWI_MAKE_FLAGS:-}} {rest}")
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rewrite_parallel_job_counts(input: &str) -> String {
    input
        .replace("\"$(nproc)\"", "\"${LWI_MAKE_JOBS}\"")
        .replace("'$(nproc)'", "\"${LWI_MAKE_JOBS}\"")
        .replace("$(nproc)", "${LWI_MAKE_JOBS}")
}

fn write_generated_recipe(
    spec_path: &Path,
    build_path: &Path,
    package: &BookPackage,
    recipe: &PageRecipe,
    manifest: &SourceManifest,
    target: &str,
    arch: &str,
) -> Result<()> {
    let archive_inputs = recipe
        .input_files
        .iter()
        .filter(|file| is_archive_filename(file))
        .cloned()
        .collect::<Vec<_>>();

    let primary_source = archive_inputs.first();
    let mut spec = String::new();
    spec.push_str("[package]\n");
    spec.push_str(&format!("name = \"{}\"\n", toml_escape(&package.name)));
    spec.push_str(&format!(
        "version = \"{}\"\n",
        toml_escape(&package.version)
    ));
    spec.push_str("revision = 1\n");
    spec.push_str(&format!(
        "description = \"{}\"\n",
        toml_escape(&recipe.description)
    ));
    spec.push_str(&format!(
        "homepage = \"{}\"\n",
        toml_escape(&package.page_url)
    ));
    spec.push_str(&format!(
        "license = \"{}\"\n\n",
        toml_escape(&recipe.license)
    ));
    let provides = bootstrap_package_provides(package);
    let replacements = bootstrap_package_replacements(package);
    if !provides.is_empty() || !replacements.is_empty() {
        spec.push_str("[alternatives]\n");
        write_toml_string_array(&mut spec, "provides", &provides);
        write_toml_static_array(&mut spec, "replaces", replacements);
        spec.push('\n');
    }
    if !recipe.dependencies.is_empty() {
        spec.push_str("[dependencies]\n");
        write_toml_string_array(&mut spec, "runtime", &recipe.dependencies);
        spec.push('\n');
    }
    spec.push_str("[build]\n");
    spec.push_str("type = \"custom\"\n\n");
    spec.push_str("[build.flags]\n");
    spec.push_str("skip_tests = true\n");
    spec.push_str("no_flags = true\n");
    if bootstrap_preserves_static_archives(package) {
        spec.push_str("no_delete_static = true\n");
    }
    spec.push_str("no_strip = true\n");
    spec.push_str(
        "passthrough_env = [\"DEPOT_LBI_CHROOT\", \"DEPOT_LBI_CHROOT_ROOT\", \"DEPOT_LBI_DEPOT_EXE\", \"DEPOT_LBI_SYSROOT\", \"LBI_CCACHE\", \"LWI_MAKE_FLAGS\", \"LWI_MAKE_JOBS\"]\n\n",
    );

    if let Some(primary) = primary_source {
        let source_url = source_url_for_input(primary, package, recipe, &manifest.entries)?;
        let checksum = source_checksum_for_input(primary, &source_url, manifest);
        spec.push_str("[[source]]\n");
        spec.push_str(&format!("url = \"{}\"\n", toml_escape(&source_url)));
        spec.push_str(&format!("sha256 = \"{}\"\n", toml_escape(&checksum)));
        spec.push_str(&format!(
            "extract_dir = \"{}\"\n\n",
            toml_escape(
                recipe
                    .extract_dir
                    .as_deref()
                    .unwrap_or_else(|| strip_archive_suffix(primary))
            )
        ));
    }

    for input in &recipe.input_files {
        if Some(input) == primary_source || input.trim().is_empty() {
            continue;
        }
        let source_url = source_url_for_input(input, package, recipe, &manifest.entries)?;
        let checksum = source_checksum_for_input(input, &source_url, manifest);
        spec.push_str("[[manual_sources]]\n");
        spec.push_str(&format!("url = \"{}\"\n", toml_escape(&source_url)));
        spec.push_str(&format!("sha256 = \"{}\"\n", toml_escape(&checksum)));
        spec.push_str(&format!("dest = \"{}\"\n\n", toml_escape(input)));
    }

    let build_script = generated_build_script(package, recipe, target, arch);
    fs::write(spec_path, spec)
        .with_context(|| format!("Failed to write {}", spec_path.display()))?;
    fs::write(build_path, build_script)
        .with_context(|| format!("Failed to write {}", build_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(build_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(build_path, perms)?;
    }
    Ok(())
}

fn bootstrap_preserves_static_archives(package: &BookPackage) -> bool {
    matches!(package.name.as_str(), "musl" | "rustc")
        || package.name.starts_with("llvm")
        || package.name.starts_with("musl-")
}

fn write_toml_string_array(spec: &mut String, key: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    spec.push_str(key);
    spec.push_str(" = [");
    for (idx, value) in values.iter().enumerate() {
        if idx > 0 {
            spec.push_str(", ");
        }
        spec.push_str(&format!("\"{}\"", toml_escape(value)));
    }
    spec.push_str("]\n");
}

fn write_toml_static_array(spec: &mut String, key: &str, values: &[&str]) {
    if values.is_empty() {
        return;
    }
    spec.push_str(key);
    spec.push_str(" = [");
    for (idx, value) in values.iter().enumerate() {
        if idx > 0 {
            spec.push_str(", ");
        }
        spec.push_str(&format!("\"{}\"", toml_escape(value)));
    }
    spec.push_str("]\n");
}

fn bootstrap_package_provides(package: &BookPackage) -> Vec<String> {
    let provides: &[&str] = match package.name.as_str() {
        "musl-libc-pass2" | "musl" => &["musl", "libc"],
        "llvm-clang-pass1" => &["llvm", "clang", "lld", "cc", "c++"],
        "llvm-runtimes" => &["libunwind", "libcxxabi", "libcxx"],
        "llvm-clang-pass2" | "llvm" => &[
            "llvm",
            "clang",
            "lld",
            "cc",
            "c++",
            "compiler-rt",
            "libunwind",
            "libcxxabi",
            "libcxx",
        ],
        "byacc" => &["yacc"],
        "dash" => &["sh"],
        "libressl" => &["openssl", "libssl", "libcrypto"],
        "make" => &["gmake"],
        "oksh" => &["ksh"],
        "om4" => &["m4"],
        "pigz" => &["gzip", "gunzip", "zcat"],
        "python" => &["python3", "pip"],
        "rustc" => &["rust", "cargo"],
        "samurai" => &["ninja", "samu"],
        "zlib-ng" => &["zlib", "libz"],
        _ => &[],
    };
    provides
        .iter()
        .filter(|provided| **provided != package.name)
        .map(|provided| (*provided).to_string())
        .collect()
}

fn bootstrap_package_replacements(package: &BookPackage) -> &'static [&'static str] {
    match package.name.as_str() {
        "musl-libc-pass2" => &["musl-libc-headers"],
        "musl" => &["musl-libc-pass2", "musl-libc-headers", "musl-libc"],
        "llvm-clang-pass2" => &["llvm-runtimes"],
        "llvm" => &["llvm-clang-pass2", "llvm-clang-pass1", "llvm-runtimes"],
        _ => &[],
    }
}

fn generated_build_script(
    package: &BookPackage,
    recipe: &PageRecipe,
    target: &str,
    arch: &str,
) -> String {
    let mut script = String::new();
    script.push_str("#!/bin/sh\nset -eu\n\n");
    script.push_str(&format!("# Generated from {}\n", package.page_url));
    script.push_str("export LBI_ROOT=\"${DESTDIR:?DESTDIR is required}\"\n");
    script.push_str("export LBI_SYSROOT=\"${DEPOT_LBI_SYSROOT:-$LBI_ROOT}\"\n");
    script.push_str(&format!(
        "export LBI_TARGET=\"${{LBI_TARGET:-{target}}}\"\n"
    ));
    script.push_str(&format!("export LBI_ARCH=\"${{LBI_ARCH:-{arch}}}\"\n"));
    script.push_str("export LBI_SOURCES=\"${DEPOT_STARBUILD_WORKDIR:-$PWD}\"\n");
    script.push_str(
        r#"if [ -z "${LWI_MAKE_JOBS:-}" ]; then
    jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1)"
    case "$jobs" in
        ""|*[!0-9]*) jobs=1 ;;
    esac
    export LWI_MAKE_JOBS="$jobs"
else
    export LWI_MAKE_JOBS
fi
if [ -z "${LWI_MAKE_FLAGS:-}" ]; then
    if [ -n "${MAKEFLAGS:-}" ]; then
        export LWI_MAKE_FLAGS="$MAKEFLAGS"
    else
        export LWI_MAKE_FLAGS="-j${LWI_MAKE_JOBS}"
    fi
else
    export LWI_MAKE_FLAGS
fi
"#,
    );
    script.push_str("export LWI_CFLAGS=\"${LWI_CFLAGS:-${CFLAGS:-}}\"\n");
    script.push_str("export LWI_CXXFLAGS=\"${LWI_CXXFLAGS:-$LWI_CFLAGS}\"\n");
    script.push_str("export LBI_CUSTOM_LDFLAGS=\"${LBI_CUSTOM_LDFLAGS:-${LDFLAGS:-}}\"\n\n");
    if uses_llvm_cmake_ccache(package) {
        script.push_str(
            r#"if [ -z "${LBI_CCACHE:-}" ]; then
    LBI_CCACHE="$(command -v ccache)" || {
        echo "depot: LLVM bootstrap requires ccache in PATH for the CMake compiler launcher" >&2
        exit 1
    }
fi
export LBI_CCACHE

"#,
        );
    }
    let uses_cross_toolchain = use_cross_toolchain_by_default(package);
    if uses_cross_toolchain {
        script.push_str(
            r#"lbi_find_cross_tool() {
    for name in "$@"; do
        for dir in "$LBI_SYSROOT/system/tools/bin"; do
            if [ -x "$dir/$LBI_TARGET-$name" ]; then
                printf '%s\n' "$dir/$LBI_TARGET-$name"
                return 0
            fi
        done
        if command -v "$LBI_TARGET-$name" >/dev/null 2>&1; then
            command -v "$LBI_TARGET-$name"
            return 0
        fi
    done
    for name in "$@"; do
        for dir in "$LBI_SYSROOT/system/tools/bin"; do
            if [ -x "$dir/$name" ]; then
                printf '%s\n' "$dir/$name"
                return 0
            fi
        done
        if command -v "$name" >/dev/null 2>&1; then
            command -v "$name"
            return 0
        fi
    done
    echo "depot: could not find cross tool for $LBI_TARGET: $*" >&2
    return 1
}

export PATH="$LBI_SYSROOT/system/tools/bin:$PATH"
export CC="${CC:-$(lbi_find_cross_tool clang)}"
export CXX="${CXX:-$(lbi_find_cross_tool clang++)}"
export AR="${AR:-$(lbi_find_cross_tool ar llvm-ar)}"
export AS="${AS:-$CC}"
export NM="${NM:-$(lbi_find_cross_tool nm llvm-nm)}"
export RANLIB="${RANLIB:-$(lbi_find_cross_tool ranlib llvm-ranlib llvm-ar)}"
export OBJCOPY="${OBJCOPY:-$(lbi_find_cross_tool objcopy llvm-objcopy)}"
export OBJDUMP="${OBJDUMP:-$(lbi_find_cross_tool objdump llvm-objdump)}"
export STRIP="${STRIP:-$(lbi_find_cross_tool strip llvm-strip llvm-objcopy)}"
export LD="${LD:-$(lbi_find_cross_tool ld ld.lld lld)}"

"#,
        );
    }
    if package.name == "musl-libc-pass2" {
        script.push_str(
            r#"if [ -z "${LIBCC:-}" ]; then
    case "$LBI_ARCH" in
        x86_64|amd64) compiler_rt_arch=x86_64 ;;
        i?86) compiler_rt_arch=i386 ;;
        aarch64|arm64) compiler_rt_arch=aarch64 ;;
        *) compiler_rt_arch=$LBI_ARCH ;;
    esac

    LIBCC=$(
        for dir in \
            "$LBI_SYSROOT/system/tools/lib/clang" \
            "$LBI_SYSROOT/system/tools/lib" \
            "$LBI_SYSROOT/system/libraries/clang" \
            "$LBI_SYSROOT/system/lib/clang"
        do
            if [ -d "$dir" ]; then
                find "$dir" -type f -name "libclang_rt.builtins-${compiler_rt_arch}.a" 2>/dev/null
            fi
        done | head -n1
    )

    if [ -z "$LIBCC" ]; then
        candidate=$("$CC" -print-file-name="libclang_rt.builtins-${compiler_rt_arch}.a" 2>/dev/null || true)
        if [ -n "$candidate" ] && [ "$candidate" != "libclang_rt.builtins-${compiler_rt_arch}.a" ]; then
            LIBCC=$candidate
        fi
    fi

    if [ -z "$LIBCC" ]; then
        candidate=$("$CC" -print-libgcc-file-name 2>/dev/null || true)
        if [ -n "$candidate" ] && [ "$candidate" != "libgcc.a" ]; then
            LIBCC=$candidate
        fi
    fi

    if [ -z "$LIBCC" ]; then
        echo "depot: musl pass2 requires compiler-rt builtins for $compiler_rt_arch before building libc.so" >&2
        exit 1
    fi

    export LIBCC
fi

"#,
        );
    }
    let chroot_workdir = recipe
        .extract_dir
        .as_deref()
        .map(|dir| format!("/sources/{dir}"))
        .unwrap_or_else(|| "/sources".to_string());
    script.push_str(&format!(
        "export DEPOT_LBI_CHROOT_WORKDIR=\"${{DEPOT_LBI_CHROOT_WORKDIR:-{}}}\"\n",
        shell_double_quote_literal(&chroot_workdir)
    ));
    script.push_str(
        "export DEPOT_LBI_CHROOT_SCRIPT=\"${DEPOT_LBI_CHROOT_SCRIPT:-$DEPOT_LBI_CHROOT_WORKDIR/build.sh}\"\n",
    );
    script.push_str(
        r#"if [ "${DEPOT_LBI_CHROOT:-0}" = "1" ] && [ "${DEPOT_LBI_INSIDE_CHROOT:-0}" != "1" ]; then
    : "${DEPOT_LBI_CHROOT_ROOT:?DEPOT_LBI_CHROOT_ROOT is required}"
    : "${DEPOT_LBI_DEPOT_EXE:?DEPOT_LBI_DEPOT_EXE is required}"
    exec "$DEPOT_LBI_DEPOT_EXE" internal bootstrap-chroot \
        --rootfs "$DEPOT_LBI_CHROOT_ROOT" \
        --sources "$LBI_SOURCES" \
        --destdir "$DESTDIR" \
        --workdir "$DEPOT_LBI_CHROOT_WORKDIR" \
        --script "$DEPOT_LBI_CHROOT_SCRIPT"
fi

"#,
    );
    script.push_str(
        r#"lbi_configure() {
    ./configure \
        --target="$LBI_TARGET" \
        --host="$LBI_TARGET" \
        --prefix=/system \
        --bindir=/system/binaries \
        --sbindir=/system/systembinaries \
        --libdir=/system/libraries \
        --includedir=/system/headers \
        --sysconfdir=/system/configuration \
        --localstatedir=/system/variable \
        --datarootdir=/system/share \
        --mandir=/system/documentation/man-pages \
        --infodir=/system/documentation/info \
        "$@"
}

lbi_cmake() {
    build_dir="$1"
    shift
    cmake -S . -B "$build_dir" -G Ninja \
        -DCMAKE_SYSTEM_NAME=Linux \
        -DCMAKE_SYSTEM_PROCESSOR="$LBI_ARCH" \
        -DCMAKE_SYSROOT="$LBI_SYSROOT" \
        -DCMAKE_C_COMPILER_TARGET="$LBI_TARGET" \
        -DCMAKE_CXX_COMPILER_TARGET="$LBI_TARGET" \
        -DCMAKE_ASM_COMPILER_TARGET="$LBI_TARGET" \
        -DCMAKE_INSTALL_PREFIX=/system \
        -DCMAKE_INSTALL_BINDIR=/system/binaries \
        -DCMAKE_INSTALL_SBINDIR=/system/systembinaries \
        -DCMAKE_INSTALL_LIBDIR=/system/libraries \
        -DCMAKE_INSTALL_INCLUDEDIR=/system/headers \
        -DCMAKE_INSTALL_SYSCONFDIR=/system/configuration \
        -DCMAKE_INSTALL_LOCALSTATEDIR=/system/variable \
        -DCMAKE_INSTALL_DATAROOTDIR=/system/share \
        -DCMAKE_INSTALL_MANDIR=/system/documentation/man-pages \
        -DCMAKE_INSTALL_INFODIR=/system/documentation/info \
        "$@"
}

lbi_meson() {
    build_dir=build
    use_lbi_cross_file=1

    case "${1-}" in
        "") ;;
        -*) ;;
        *)
            build_dir=$1
            shift
            ;;
    esac

    for arg in "$@"; do
        case "$arg" in
            --cross-file|--cross-file=*) use_lbi_cross_file=0 ;;
        esac
    done

    case "$LBI_ARCH" in
        amd64) lbi_meson_cpu_family=x86_64 ;;
        i?86) lbi_meson_cpu_family=x86 ;;
        arm64) lbi_meson_cpu_family=aarch64 ;;
        *) lbi_meson_cpu_family=$LBI_ARCH ;;
    esac

    if [ "$use_lbi_cross_file" = "1" ]; then
        lbi_meson_cross_file="$PWD/.lbi-meson-cross.ini"
        cat > "$lbi_meson_cross_file" <<EOF
[binaries]
c = '${CC:-cc}'
cpp = '${CXX:-c++}'
ar = '${AR:-ar}'
strip = '${STRIP:-strip}'
pkg-config = 'pkg-config'

[host_machine]
system = 'linux'
cpu_family = '$lbi_meson_cpu_family'
cpu = '$LBI_ARCH'
endian = 'little'

[properties]
sys_root = '$LBI_SYSROOT'
needs_exe_wrapper = false

[built-in options]
c_args = ['--target=$LBI_TARGET', '--sysroot=$LBI_SYSROOT']
cpp_args = ['--target=$LBI_TARGET', '--sysroot=$LBI_SYSROOT']
c_link_args = ['--target=$LBI_TARGET', '--sysroot=$LBI_SYSROOT']
cpp_link_args = ['--target=$LBI_TARGET', '--sysroot=$LBI_SYSROOT']
EOF
        meson setup "$build_dir" --cross-file "$lbi_meson_cross_file" \
            --prefix=/system \
            --bindir=/system/binaries \
            --sbindir=/system/systembinaries \
            --libdir=/system/libraries \
            --libexecdir=/system/systembinaries \
            --includedir=/system/headers \
            --sysconfdir=/system/configuration \
            --localstatedir=/system/variable \
            --datadir=/system/share \
            --mandir=/system/documentation/man-pages \
            --infodir=/system/documentation/info \
            "$@"
    else
        meson setup "$build_dir" \
            --prefix=/system \
            --bindir=/system/binaries \
            --sbindir=/system/systembinaries \
            --libdir=/system/libraries \
            --libexecdir=/system/systembinaries \
            --includedir=/system/headers \
            --sysconfdir=/system/configuration \
            --localstatedir=/system/variable \
            --datadir=/system/share \
            --mandir=/system/documentation/man-pages \
            --infodir=/system/documentation/info \
            "$@"
    fi
}

"#,
    );
    script.push_str(
        r#"mkdir -p \
    "$LBI_ROOT/system/binaries" \
    "$LBI_ROOT/system/systembinaries" \
    "$LBI_ROOT/system/libraries" \
    "$LBI_ROOT/system/headers" \
    "$LBI_ROOT/system/configuration" \
    "$LBI_ROOT/system/variable" \
    "$LBI_ROOT/system/share" \
    "$LBI_ROOT/system/documentation/man-pages" \
    "$LBI_ROOT/system/documentation/info"

"#,
    );
    for command in &recipe.commands {
        let command = apply_bootstrap_package_overrides(package, command);
        let mut rewritten = rewrite_lbi_command(&command, recipe.extract_dir.as_deref());
        if uses_cross_toolchain {
            rewritten = rewrite_cross_tool_paths(&rewritten);
        }
        if package.chapter >= 7 {
            rewritten = restore_chroot_compiler_search_paths(&rewritten);
        }
        if package.name == "rustc" {
            rewritten = restore_rust_bootstrap_runtime_tool_paths(&rewritten);
        }
        if is_clang_driver_config_command(&rewritten) {
            rewritten = restore_clang_driver_config_runtime_paths(&rewritten);
        }
        if !rewritten.trim().is_empty() {
            script.push_str(&rewritten);
            script.push_str("\n\n");
        }
    }
    script
}

fn use_cross_toolchain_by_default(package: &BookPackage) -> bool {
    if !(5..=6).contains(&package.chapter) || package.name == "llvm-clang-pass1" {
        return false;
    }
    if package.chapter == 6 {
        return true;
    }
    section_at_or_after(&package.section, "5.4")
}

fn section_at_or_after(section: &str, threshold: &str) -> bool {
    let left = parse_section_numbers(section);
    let right = parse_section_numbers(threshold);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let max_len = left.len().max(right.len());
    for idx in 0..max_len {
        let l = *left.get(idx).unwrap_or(&0);
        let r = *right.get(idx).unwrap_or(&0);
        match l.cmp(&r) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    true
}

fn parse_section_numbers(section: &str) -> Vec<u32> {
    section
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
}

fn apply_bootstrap_package_overrides(package: &BookPackage, command: &str) -> String {
    if package.name == "llvm-clang-pass1" {
        let command = ensure_llvm_pass1_unsets_destdir(command);
        let command = ensure_llvm_cmake_uses_ccache(&command);
        let command = ensure_llvm_pass1_builtins_only(&command);
        let command = ensure_llvm_pass1_default_target_only(&command);
        let command = ensure_llvm_pass1_builtins_sysroot(&command);
        let command = ensure_llvm_pass1_default_sysroot(&command);
        ensure_llvm_pass1_fresh_build_dir(&command)
    } else if package.name == "llvm-runtimes" {
        let command = ensure_llvm_runtime_links_skip_missing_crt(command);
        ensure_llvm_runtimes_builds_compiler_rt_crt(&command)
    } else if package.name == "llvm-clang-pass2" {
        let command = ensure_llvm_cmake_uses_ccache(command);
        let command = ensure_llvm_runtime_links_skip_missing_crt(&command);
        let command = ensure_llvm_pass2_uses_pass1_native_tools(&command);
        ensure_llvm_pass2_resource_layout(&command)
    } else if matches!(package.name.as_str(), "musl-libc-pass2" | "musl") {
        ensure_musl_loader_paths(command)
    } else if package.name == "ncurses" {
        let command = if package.chapter == 6 {
            ensure_ncurses_host_tic_uses_build_toolchain(command)
        } else {
            command.to_string()
        };
        ensure_ncurses_progs_avoid_private_safe_fopen(&command)
    } else if package.chapter == 6 && package.name == "file" {
        ensure_file_uses_host_magic_compiler(command)
    } else if package.name == "gettext-tiny" {
        ensure_gettext_tiny_uses_musl_libintl_mode(command)
    } else if package.name == "shadow" {
        ensure_shadow_uses_staged_account_tools(command)
    } else if package.name == "bsddiffutils" {
        ensure_bsddiffutils_uses_lbi_header_layout(command)
    } else if package.name == "mandoc" {
        ensure_mandoc_postinstall_uses_staged_man(command)
    } else if package.name == "bmake" {
        ensure_bmake_install_avoids_double_destdir(command)
    } else if package.name == "byacc" {
        ensure_byacc_exposes_unprefixed_yacc(command)
    } else if package.name == "ubase" {
        ensure_ubase_moves_bin_payload(command)
    } else if package.name == "sqlite" {
        ensure_sqlite_uses_supported_autosetup_dirs(command)
    } else {
        command.to_string()
    }
}

fn ensure_ubase_moves_bin_payload(command: &str) -> String {
    if !command_runs_install_step(command) {
        return command.to_string();
    }

    format!(
        "{command}\n{}",
        r#"if [ -d "$DESTDIR/system/bin" ]; then
    mkdir -p "$DESTDIR/system/binaries"
    set -- "$DESTDIR/system/bin"/*
    if [ -e "$1" ] || [ -L "$1" ]; then
        for path do
            mv "$path" "$DESTDIR/system/binaries/"
        done
    fi
    rmdir "$DESTDIR/system/bin" 2>/dev/null || true
fi"#
    )
}

fn ensure_byacc_exposes_unprefixed_yacc(command: &str) -> String {
    if !command_runs_install_step(command) {
        return command.to_string();
    }

    format!(
        "{command}\n{}",
        r#"if [ ! -e "$DESTDIR/system/binaries/yacc" ] && [ -e "$DESTDIR/system/binaries/$LBI_TARGET-yacc" ]; then
    ln -sf "$LBI_TARGET-yacc" "$DESTDIR/system/binaries/yacc"
fi
if [ ! -e "$DESTDIR/system/documentation/man-pages/man1/yacc.1" ] \
    && [ -e "$DESTDIR/system/documentation/man-pages/man1/$LBI_TARGET-yacc.1" ]; then
    ln -sf "$LBI_TARGET-yacc.1" "$DESTDIR/system/documentation/man-pages/man1/yacc.1"
fi"#
    )
}

fn command_runs_install_step(command: &str) -> bool {
    command.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#')
            && (trimmed == "install"
                || trimmed.starts_with("install ")
                || trimmed.contains(" install")
                || trimmed.contains(" install "))
    })
}

fn ensure_gettext_tiny_uses_musl_libintl_mode(command: &str) -> String {
    command.replace("LIBINTL=musl", "LIBINTL=MUSL")
}

fn ensure_shadow_uses_staged_account_tools(command: &str) -> String {
    command.replace(
        "pwconv\ngrpconv\nmkdir -p /etc/default\nuseradd -D --gid 999\npasswd root",
        "\"$DESTDIR/system/systembinaries/pwconv\" -R /\nif ! grep -q '^[^:]*:[^:]*:999:' /etc/group; then\n    printf '%s\\n' 'users:x:999:' >> /etc/group\nfi\n\"$DESTDIR/system/systembinaries/grpconv\" -R /\nmkdir -p /etc/default\n\"$DESTDIR/system/systembinaries/useradd\" -D -R / --gid 999\n\"$DESTDIR/system/binaries/passwd\" -R / -d root",
    )
}

fn ensure_bsddiffutils_uses_lbi_header_layout(command: &str) -> String {
    command
        .replace(
            r#"sed -i \
    's|char\\s*\\*splice(char \\*, char \\*);|char\t*diff_splice(char *, char *);|' \
    src/diff/diff.h"#,
            r#"sed -i \
    's|char[[:space:]]*\*splice(char \*, char \*);|char    *diff_splice(char *, char *);|' \
    src/diff/diff.h"#,
        )
        .replace(
            r#"sed -i \
    's/\bsplice(/diff_splice(/g' \
    src/diff/diff.c src/diff/diffreg.c"#,
            r#"sed -i \
    -e 's|= splice(|= diff_splice(|g' \
    -e 's|^splice(char \*dir, char \*file)|diff_splice(char *dir, char *file)|' \
    src/diff/diff.c src/diff/diffreg.c"#,
        )
        .replace(
            r#"sed -i \
    's/u_char ch, \\*p1, \\*p2;/unsigned char ch, *p1, *p2;/' \
    src/cmp/regular.c"#,
            r#"sed -i \
    -e 's|u_char ch, \*p1, \*p2;|unsigned char ch, *p1, *p2;|' \
    src/cmp/regular.c"#,
        )
        .replace(
            "CPPFLAGS=\"--target=$LBI_TARGET --sysroot=$LBI_ROOT -include ../../include/sys/cdefs.h\"",
            "CPPFLAGS=\"--target=$LBI_TARGET --sysroot=$LBI_SYSROOT -I../../include -isystem $LBI_SYSROOT/system/headers -Wno-#warnings -include sys/cdefs.h\"",
        )
        .replace(
            "LDFLAGS=\"--target=$LBI_TARGET --sysroot=$LBI_ROOT $LBI_CUSTOM_LDFLAGS\"",
            "LDFLAGS=\"--target=$LBI_TARGET --sysroot=$LBI_SYSROOT -L$LBI_SYSROOT/system/libraries $LBI_CUSTOM_LDFLAGS\"",
        )
        .replace(
            r#"sed -i '' \
    '/#include <limits.h>/a char *fgetln(FILE *, size_t *);' \
    diff/diff.c"#,
            r##"awk '
    { print }
    $0 == "#include <limits.h>" {
        print "char *fgetln(FILE *, size_t *);"
    }
' diff/diff.c > diff/diff.c.new
mv diff/diff.c.new diff/diff.c"##,
        )
        .replace(
            r#"sed -i \
    '/#include <limits.h>/a char *fgetln(FILE *, size_t *);' \
    diff/diff.c"#,
            r##"awk '
    { print }
    $0 == "#include <limits.h>" {
        print "char *fgetln(FILE *, size_t *);"
    }
' diff/diff.c > diff/diff.c.new
mv diff/diff.c.new diff/diff.c"##,
        )
        .replace(
            r#"sed -i '' \
    '/#include <unistd.h>/a char *fgetln(FILE *, size_t *);' \
    diff3/diff3prog.c"#,
            r##"awk '
    { print }
    $0 == "#include <unistd.h>" {
        print "char *fgetln(FILE *, size_t *);"
    }
' diff3/diff3prog.c > diff3/diff3prog.c.new
mv diff3/diff3prog.c.new diff3/diff3prog.c"##,
        )
        .replace(
            r#"sed -i \
    '/#include <unistd.h>/a char *fgetln(FILE *, size_t *);' \
    diff3/diff3prog.c"#,
            r##"awk '
    { print }
    $0 == "#include <unistd.h>" {
        print "char *fgetln(FILE *, size_t *);"
    }
' diff3/diff3prog.c > diff3/diff3prog.c.new
mv diff3/diff3prog.c.new diff3/diff3prog.c"##,
        )
        .replace(
            r#"sed -i '' \
    '/include_directories: \[sysdefs\],/a \    link_with: [libcompat],' \
    diff3/meson.build"#,
            r#"awk '
    { print }
    index($0, "include_directories: [sysdefs],") {
        print "    link_with: [libcompat],"
    }
' diff3/meson.build > diff3/meson.build.new
mv diff3/meson.build.new diff3/meson.build"#,
        )
        .replace(
            r#"sed -i \
    '/include_directories: \[sysdefs\],/a \    link_with: [libcompat],' \
    diff3/meson.build"#,
            r#"awk '
    { print }
    index($0, "include_directories: [sysdefs],") {
        print "    link_with: [libcompat],"
    }
' diff3/meson.build > diff3/meson.build.new
mv diff3/meson.build.new diff3/meson.build"#,
        )
}

fn ensure_ncurses_host_tic_uses_build_toolchain(command: &str) -> String {
    if !command.contains("make -C progs tic") {
        return command.to_string();
    }

    r#"rm -rf build obj obj_s obj_g obj_x lib
mkdir -pv build
(
    cd build
    env \
        CC="${BUILD_CC:-cc}" \
        CXX="${BUILD_CXX:-c++}" \
        CPP="${BUILD_CPP:-cpp}" \
        AR="${BUILD_AR:-ar}" \
        AS="${BUILD_AS:-as}" \
        LD="${BUILD_LD:-ld}" \
        NM="${BUILD_NM:-nm}" \
        RANLIB="${BUILD_RANLIB:-ranlib}" \
        STRIP="${BUILD_STRIP:-strip}" \
        ../configure --prefix="$LBI_SYSROOT/system/tools" AWK=gawk
    make -C include
    make -C progs tic
    install -vm755 progs/tic "$LBI_SYSROOT/system/tools/bin/tic"
)
rm -rf obj obj_s obj_g obj_x lib"#
        .to_string()
}

fn ensure_ncurses_progs_avoid_private_safe_fopen(command: &str) -> String {
    if !command.contains("lbi_configure") {
        return command.to_string();
    }

    let mut rewritten = command.to_string();
    if !rewritten.contains("--enable-root-access") {
        rewritten = rewritten.replace(
            "    --with-shared \\",
            "    --with-shared \\\n    --enable-root-access \\",
        );
    }
    if !rewritten.contains("USE_ROOT_ACCESS") {
        rewritten =
            format!("export CPPFLAGS=\"${{CPPFLAGS:+$CPPFLAGS }}-DUSE_ROOT_ACCESS\"\n{rewritten}");
    }
    rewritten
}

fn ensure_sqlite_uses_supported_autosetup_dirs(command: &str) -> String {
    command.replace(
        "lbi_configure \\",
        "./configure \\\n    --prefix=/system \\\n    --bindir=/system/binaries \\\n    --libdir=/system/libraries \\\n    --includedir=/system/headers \\\n    --mandir=/system/documentation/man-pages \\",
    )
}

fn ensure_bmake_install_avoids_double_destdir(command: &str) -> String {
    command.replace(
        "MAKESYSPATH=mk \\\n./bmake -f Makefile install \\",
        "DESTDIR= \\\nMAKESYSPATH=mk \\\n./bmake -f Makefile install \\",
    )
}

fn ensure_mandoc_postinstall_uses_staged_man(command: &str) -> String {
    command.replace(
        "MANPAGER=cat man mandoc >/dev/null",
        "MANPATH=\"$DESTDIR/system/documentation/man-pages\" MANPAGER=cat \"$DESTDIR/system/binaries/man\" mandoc >/dev/null",
    )
}

fn ensure_file_uses_host_magic_compiler(command: &str) -> String {
    if command.contains("lbi_configure") && command.contains("--host=\"$LBI_TARGET\"") {
        return format!(
            r#"rm -rf build-host-file
mkdir -p build-host-file
(
    cd build-host-file
    env \
        CC="${{BUILD_CC:-cc}}" \
        CXX="${{BUILD_CXX:-c++}}" \
        CPP="${{BUILD_CPP:-cpp}}" \
        AR="${{BUILD_AR:-ar}}" \
        RANLIB="${{BUILD_RANLIB:-ranlib}}" \
        ../configure --prefix="$PWD/host-tools" --disable-shared --enable-static --disable-libseccomp
    make ${{LWI_MAKE_FLAGS:-}} -C src file
)

{command}"#
        );
    }

    if command.trim() == "make $LWI_MAKE_FLAGS" {
        "make $LWI_MAKE_FLAGS FILE_COMPILE=\"$PWD/build-host-file/src/file\"".to_string()
    } else {
        command.to_string()
    }
}

fn ensure_llvm_pass1_fresh_build_dir(command: &str) -> String {
    if command.contains("rm -rf build-llvm") {
        return command.to_string();
    }
    command.replace(
        "mkdir -p build-llvm\ncd build-llvm",
        "rm -rf build-llvm\nmkdir -p build-llvm\ncd build-llvm",
    )
}

fn ensure_llvm_pass1_unsets_destdir(command: &str) -> String {
    if command.contains("cmake -G Ninja \"../llvm\"")
        && command.contains("-DCMAKE_INSTALL_PREFIX=$LBI_ROOT/system/tools")
        && !command.contains("unset DESTDIR")
    {
        format!("unset DESTDIR\n{command}")
    } else {
        command.to_string()
    }
}

fn ensure_llvm_pass1_builtins_only(command: &str) -> String {
    if !command.contains("-DLLVM_ENABLE_RUNTIMES=\"compiler-rt\"") {
        return command.to_string();
    }

    let missing_flags = [
        "-DCOMPILER_RT_BUILD_CRT=OFF",
        "-DCOMPILER_RT_BUILD_MEMPROF=OFF",
        "-DCOMPILER_RT_BUILD_ORC=OFF",
        "-DCOMPILER_RT_BUILD_CTX_PROFILE=OFF",
        "-DCOMPILER_RT_INCLUDE_TESTS=OFF",
    ]
    .into_iter()
    .filter(|flag| !command.contains(flag))
    .map(|flag| format!("    {flag} \\"))
    .collect::<Vec<_>>();

    if missing_flags.is_empty() {
        return command.to_string();
    }

    let block = missing_flags.join("\n");
    let profile_anchor = "    -DCOMPILER_RT_BUILD_PROFILE=OFF \\";
    if command.contains(profile_anchor) {
        return command.replace(profile_anchor, &format!("{profile_anchor}\n{block}"));
    }

    let builtins_anchor = "    -DCOMPILER_RT_BUILD_BUILTINS=ON \\";
    command.replace(builtins_anchor, &format!("{builtins_anchor}\n{block}"))
}

fn ensure_llvm_pass1_default_target_only(command: &str) -> String {
    if !command.contains("-DLLVM_ENABLE_RUNTIMES=\"compiler-rt\"")
        || command.contains("-DCOMPILER_RT_DEFAULT_TARGET_ONLY=")
    {
        return command.to_string();
    }

    command.replace(
        "    -DCOMPILER_RT_BUILD_BUILTINS=ON \\",
        "    -DCOMPILER_RT_BUILD_BUILTINS=ON \\\n    -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON \\",
    )
}

fn ensure_llvm_pass1_builtins_sysroot(command: &str) -> String {
    if !command.contains("-DLLVM_ENABLE_RUNTIMES=\"compiler-rt\"")
        || command.contains("-DBUILTINS_CMAKE_ARGS=")
    {
        return command.to_string();
    }

    command.replace(
        "    -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON \\",
        "    -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON \\\n    -DBUILTINS_CMAKE_ARGS=\"-DCMAKE_C_FLAGS=--sysroot=$LBI_SYSROOT;-DCMAKE_ASM_FLAGS=--sysroot=$LBI_SYSROOT\" \\",
    )
}

fn ensure_llvm_pass1_default_sysroot(command: &str) -> String {
    if !command.contains("-DLLVM_ENABLE_RUNTIMES=\"compiler-rt\"") {
        return command.to_string();
    }

    command.replace(
        "-DDEFAULT_SYSROOT=$LBI_ROOT",
        "-DDEFAULT_SYSROOT=$LBI_SYSROOT",
    )
}

fn ensure_musl_loader_paths(command: &str) -> String {
    let command = command.replace(
        "$LBI_ROOT/usr/lib/ld-musl-${LBI_ARCH}.so.1",
        "$LBI_ROOT/system/libraries/ld-musl-${LBI_ARCH}.so.1",
    );

    command.replace(
        "ln -snf ./libc.so \\\n    \"$LBI_ROOT/system/libraries/ld-musl-${LBI_ARCH}.so.1\"",
        "rm -f \"$LBI_ROOT/system/libraries/ld-musl-${LBI_ARCH}.so.1\"\nln -sf ./libc.so \\\n    \"$LBI_ROOT/system/libraries/ld-musl-${LBI_ARCH}.so.1\"\nrm -f \"$LBI_ROOT/lib/ld-musl-${LBI_ARCH}.so.1\"\nrmdir \"$LBI_ROOT/lib\" 2>/dev/null || true",
    )
}

fn ensure_llvm_runtime_links_skip_missing_crt(command: &str) -> String {
    if !command.contains("-DLLVM_ENABLE_RUNTIMES=\"libunwind;libcxxabi;libcxx\"") {
        return command.to_string();
    }

    let command = ensure_cmake_linker_flag(command, "CMAKE_SHARED_LINKER_FLAGS", "-nostartfiles");
    ensure_cmake_linker_flag(&command, "CMAKE_MODULE_LINKER_FLAGS", "-nostartfiles")
}

fn ensure_llvm_cmake_uses_ccache(command: &str) -> String {
    let command = command
        .replace("    -DLLVM_CCACHE_BUILD=ON \\\n", "")
        .replace("    -DLLVM_CCACHE_BUILD=ON \\", "")
        .replace("-DLLVM_CCACHE_BUILD=ON", "");

    if command.contains("-DCMAKE_C_COMPILER_LAUNCHER=")
        && command.contains("-DCMAKE_CXX_COMPILER_LAUNCHER=")
        && command.contains("-DCMAKE_ASM_COMPILER_LAUNCHER=")
    {
        return command;
    }

    let launcher_flags = concat!(
        "    -DCMAKE_C_COMPILER_LAUNCHER=\"$LBI_CCACHE\" \\\n",
        "    -DCMAKE_CXX_COMPILER_LAUNCHER=\"$LBI_CCACHE\" \\\n",
        "    -DCMAKE_ASM_COMPILER_LAUNCHER=\"$LBI_CCACHE\" \\"
    );

    if command.contains("lbi_cmake build-llvm") {
        return command.replace(
            "lbi_cmake build-llvm \\",
            &format!("lbi_cmake build-llvm \\\n{launcher_flags}"),
        );
    }

    if command.contains("cmake -G Ninja \"../llvm\"") {
        return command.replace(
            "cmake -G Ninja \"../llvm\" \\",
            &format!("cmake -G Ninja \"../llvm\" \\\n{launcher_flags}"),
        );
    }

    command
}

fn uses_llvm_cmake_ccache(package: &BookPackage) -> bool {
    matches!(
        package.name.as_str(),
        "llvm-clang-pass1" | "llvm-clang-pass2"
    )
}

fn ensure_llvm_pass2_uses_pass1_native_tools(command: &str) -> String {
    let command = command.replace(
        "-DLLVM_NATIVE_TOOL_DIR=\"$LBI_ROOT/system/tools/bin\"",
        "-DLLVM_NATIVE_TOOL_DIR=\"$LBI_SYSROOT/system/tools/bin\"",
    );
    if command.contains("-DLLVM_CONFIG_PATH=") {
        return command;
    }

    command.replace(
        "-DLLVM_NATIVE_TOOL_DIR=\"$LBI_SYSROOT/system/tools/bin\" \\",
        "-DLLVM_NATIVE_TOOL_DIR=\"$LBI_SYSROOT/system/tools/bin\" \\\n    -DLLVM_CONFIG_PATH=\"$LBI_SYSROOT/system/tools/bin/llvm-config\" \\",
    )
}

fn ensure_llvm_pass2_resource_layout(command: &str) -> String {
    let command = command.replace(
        r#"mkdir -p "$LBI_ROOT/system/lib/clang/22/lib/$LBI_TARGET"

if [ -f "$LBI_ROOT/system/tools/lib/clang/22/lib/$LBI_TARGET/libclang_rt.builtins.a" ]; then
    ln -sf "/system/tools/lib/clang/22/lib/$LBI_TARGET/libclang_rt.builtins.a" \
        "$LBI_ROOT/system/lib/clang/22/lib/$LBI_TARGET/libclang_rt.builtins.a"
fi"#,
        r#"case "$LBI_ARCH" in
    x86_64|amd64) compiler_rt_arch=x86_64 ;;
    i?86) compiler_rt_arch=i386 ;;
    aarch64|arm64) compiler_rt_arch=aarch64 ;;
    *) compiler_rt_arch=$LBI_ARCH ;;
esac

mkdir -p \
    "$LBI_ROOT/system/libraries/clang/22/lib/linux" \
    "$LBI_ROOT/system/libraries/clang/22/lib/$LBI_TARGET"

if [ -d "$LBI_ROOT/system/lib/clang/22" ]; then
    cp -R "$LBI_ROOT/system/lib/clang/22/." "$LBI_ROOT/system/libraries/clang/22/"
    rm -rf "$LBI_ROOT/system/lib/clang/22"
    rmdir "$LBI_ROOT/system/lib/clang" "$LBI_ROOT/system/lib" 2>/dev/null || true
fi

builtins_name="libclang_rt.builtins-${compiler_rt_arch}.a"
builtins_src=$({ find \
    "$LBI_ROOT/system/libraries/clang/22/lib" \
    "$LBI_ROOT/system/lib/clang/22/lib" \
    -type f -name "$builtins_name" 2>/dev/null || true; } | head -n1)

if [ -z "$builtins_src" ]; then
    echo "depot: compiler-rt builtins archive $builtins_name was not installed" >&2
    exit 1
fi

clang_resource_root="$LBI_ROOT/system/libraries/clang/22"
mkdir -p "$clang_resource_root/lib/linux" "$clang_resource_root/lib/$LBI_TARGET"
if [ ! -f "$clang_resource_root/lib/linux/$builtins_name" ]; then
    install -m644 "$builtins_src" "$clang_resource_root/lib/linux/$builtins_name"
fi
ln -sf "../linux/$builtins_name" \
    "$clang_resource_root/lib/$LBI_TARGET/libclang_rt.builtins.a""#,
    );

    command.replace(
        r#"CRTBEGIN_OBJ=$(find "$LBI_ROOT/system/libraries/clang" \
    -type f \( -name 'crtbeginS.o' -o -name 'clang_rt.crtbegin*.o' \) | head -n1)
CRTEND_OBJ=$(find "$LBI_ROOT/system/libraries/clang" \
    -type f \( -name 'crtendS.o' -o -name 'clang_rt.crtend*.o' \) | head -n1)

CRT_DIR=$(dirname "$CRTBEGIN_OBJ")

if [ -n "$CRTBEGIN_OBJ" ] && [ -n "$CRTEND_OBJ" ]; then
    ln -sf "$(basename "$CRTBEGIN_OBJ")" "$CRT_DIR/crtbeginS.o"
    ln -sf "$(basename "$CRTEND_OBJ")" "$CRT_DIR/crtendS.o"

    ln -sf "${CRTBEGIN_OBJ#$LBI_ROOT/system}" "$LBI_ROOT/system/libraries/crtbeginS.o"
    ln -sf "${CRTEND_OBJ#$LBI_ROOT/system}" "$LBI_ROOT/system/libraries/crtendS.o"
fi"#,
        r#"CRTBEGIN_OBJ=$({ find \
    "$LBI_ROOT/system/libraries/clang" \
    "$LBI_ROOT/system/lib/clang" \
    -type f \( -name 'crtbeginS.o' -o -name 'clang_rt.crtbegin*.o' \) 2>/dev/null || true; } | head -n1)
CRTEND_OBJ=$({ find \
    "$LBI_ROOT/system/libraries/clang" \
    "$LBI_ROOT/system/lib/clang" \
    -type f \( -name 'crtendS.o' -o -name 'clang_rt.crtend*.o' \) 2>/dev/null || true; } | head -n1)

if [ -n "$CRTBEGIN_OBJ" ] && [ -n "$CRTEND_OBJ" ]; then
    CRT_DIR=$(dirname "$CRTBEGIN_OBJ")
    ln -sf "$(basename "$CRTBEGIN_OBJ")" "$CRT_DIR/crtbeginS.o"
    ln -sf "$(basename "$CRTEND_OBJ")" "$CRT_DIR/crtendS.o"

    install -m644 "$CRTBEGIN_OBJ" "$LBI_ROOT/system/libraries/crtbeginS.o"
    install -m644 "$CRTEND_OBJ" "$LBI_ROOT/system/libraries/crtendS.o"
fi"#,
    )
}

fn ensure_llvm_runtimes_builds_compiler_rt_crt(command: &str) -> String {
    let installs_runtimes = command.contains("install") && command.contains("build-runtimes");
    if !installs_runtimes || command.contains("build-compiler-rt-crt") {
        return command.to_string();
    }

    format!(
        r#"{command}

cd ../compiler-rt
rm -rf build-compiler-rt-crt
lbi_cmake build-compiler-rt-crt \
    -G Ninja \
    -DCMAKE_C_COMPILER="$CC" \
    -DCMAKE_CXX_COMPILER="$CXX" \
    -DCMAKE_ASM_COMPILER="$CC" \
    -DCMAKE_AR="$AR" \
    -DCMAKE_NM="$NM" \
    -DCMAKE_RANLIB="$RANLIB" \
    -DLLVM_CMAKE_DIR="$LBI_SYSROOT/system/tools/lib/cmake/llvm" \
    -DCMAKE_SYSROOT="$LBI_SYSROOT" \
    -DCMAKE_C_COMPILER_TARGET="$LBI_TARGET" \
    -DCMAKE_CXX_COMPILER_TARGET="$LBI_TARGET" \
    -DCMAKE_ASM_COMPILER_TARGET="$LBI_TARGET" \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DCMAKE_C_FLAGS="--target=$LBI_TARGET --sysroot=$LBI_SYSROOT $LWI_CFLAGS" \
    -DCMAKE_CXX_FLAGS="--target=$LBI_TARGET --sysroot=$LBI_SYSROOT ${{LWI_CXXFLAGS:-$LWI_CFLAGS}}" \
    -DCMAKE_ASM_FLAGS="--target=$LBI_TARGET --sysroot=$LBI_SYSROOT $LWI_CFLAGS" \
    -DCOMPILER_RT_INSTALL_PATH=/system/tools/lib/clang/22 \
    -DCOMPILER_RT_BUILD_BUILTINS=ON \
    -DCOMPILER_RT_BUILD_CRT=ON \
    -DCOMPILER_RT_BUILD_LIBFUZZER=OFF \
    -DCOMPILER_RT_BUILD_MEMPROF=OFF \
    -DCOMPILER_RT_BUILD_ORC=OFF \
    -DCOMPILER_RT_BUILD_PROFILE=OFF \
    -DCOMPILER_RT_BUILD_CTX_PROFILE=OFF \
    -DCOMPILER_RT_BUILD_SANITIZERS=OFF \
    -DCOMPILER_RT_BUILD_XRAY=OFF \
    -DCOMPILER_RT_DEFAULT_TARGET_ONLY=ON \
    -DCOMPILER_RT_INCLUDE_TESTS=OFF \
    -DLLVM_ENABLE_PER_TARGET_RUNTIME_DIR=OFF \
    -DCMAKE_BUILD_TYPE=Release

cmake --build build-compiler-rt-crt --target crt $LWI_MAKE_FLAGS

crt_resource_dir="$LBI_ROOT/system/tools/lib/clang/22/lib/$LBI_TARGET"
mkdir -p "$crt_resource_dir" "$LBI_ROOT/system/libraries"
crtbegin_obj=$(find build-compiler-rt-crt \
    -type f \( -name 'crtbeginS.o' -o -name 'clang_rt.crtbegin*.o' \) 2>/dev/null | head -n1)
crtend_obj=$(find build-compiler-rt-crt \
    -type f \( -name 'crtendS.o' -o -name 'clang_rt.crtend*.o' \) 2>/dev/null | head -n1)

if [ -z "$crtbegin_obj" ] || [ -z "$crtend_obj" ]; then
    echo "depot: compiler-rt CRT build did not produce crtbegin/crtend objects" >&2
    exit 1
fi

install -m644 "$crtbegin_obj" "$crt_resource_dir/crtbeginS.o"
install -m644 "$crtend_obj" "$crt_resource_dir/crtendS.o"
install -m644 "$crtbegin_obj" "$LBI_ROOT/system/libraries/crtbeginS.o"
install -m644 "$crtend_obj" "$LBI_ROOT/system/libraries/crtendS.o""#
    )
}

fn ensure_cmake_linker_flag(command: &str, variable: &str, flag: &str) -> String {
    let define = format!("-D{variable}=");
    if let Some(start) = command.find(&define) {
        let value_start = start + define.len();
        let Some(first) = command[value_start..].chars().next() else {
            return command.to_string();
        };

        let (content_start, content_end) = if first == '"' {
            let content_start = value_start + 1;
            let Some(rel_end) = command[content_start..].find('"') else {
                return command.to_string();
            };
            (content_start, content_start + rel_end)
        } else {
            let rel_end = command[value_start..]
                .find(|ch: char| ch.is_whitespace())
                .unwrap_or(command.len() - value_start);
            (value_start, value_start + rel_end)
        };

        let value = &command[content_start..content_end];
        if value.split_whitespace().any(|part| part == flag) {
            return command.to_string();
        }

        let mut out = String::with_capacity(command.len() + flag.len() + 1);
        out.push_str(&command[..content_start]);
        out.push_str(flag);
        if !value.is_empty() {
            out.push(' ');
            out.push_str(value);
        }
        out.push_str(&command[content_end..]);
        return out;
    }

    let inserted = format!("    -D{variable}=\"{flag}\" \\");
    let anchor = "    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \\";
    if command.contains(anchor) {
        command.replace(anchor, &format!("{anchor}\n{inserted}"))
    } else {
        command.to_string()
    }
}

fn parse_source_manifest(input: &str) -> Vec<ManifestEntry> {
    let mut entries = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let Some(url) = fields.next() else {
            continue;
        };
        let output_name = fields
            .next()
            .map(str::to_string)
            .unwrap_or_else(|| filename_from_url(url));
        if !output_name.is_empty() {
            entries.push(ManifestEntry {
                url: url.trim_start_matches("git+").to_string(),
                output_name,
            });
        }
    }
    entries
}

fn parse_source_b2sums(input: &str) -> Result<BTreeMap<String, String>> {
    let mut sums = BTreeMap::new();
    for (idx, line) in input.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut fields = trimmed.split_whitespace();
        let Some(hash) = fields.next() else {
            continue;
        };
        let Some(filename) = fields.next() else {
            anyhow::bail!("Malformed BLAKE2 source manifest line {}", idx + 1);
        };
        if hash.len() != 128 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
            anyhow::bail!("Invalid BLAKE2b-512 digest on line {}", idx + 1);
        }
        let filename = filename.trim_start_matches('*');
        if filename.is_empty() {
            anyhow::bail!("Empty filename in BLAKE2 source manifest line {}", idx + 1);
        }
        let hash = hash.to_ascii_lowercase();
        if let Some(existing) = sums.insert(filename.to_string(), hash.clone())
            && existing != hash
        {
            anyhow::bail!(
                "Conflicting BLAKE2 source checksums for {} on line {}",
                filename,
                idx + 1
            );
        }
    }
    Ok(sums)
}

fn source_checksum_for_input(input: &str, source_url: &str, manifest: &SourceManifest) -> String {
    if let Some(hash) = manifest.blake2b_512.get(input) {
        return format!("b2sum:{hash}");
    }

    let source_filename = filename_from_url(source_url);
    if source_filename != input
        && let Some(hash) = manifest.blake2b_512.get(&source_filename)
    {
        return format!("b2sum:{hash}");
    }

    "skip".to_string()
}

fn parse_page_recipe(html: &str, package: &BookPackage) -> Result<PageRecipe> {
    let prelude = html.split("<h2>").next().unwrap_or(html);
    let input_files = input_files_from_prelude(prelude);
    let source_urls = source_urls_from_prelude(prelude);
    let extract_dir = extract_dir_from_html(html);
    let mut commands = Vec::new();
    for (heading, code) in shell_code_blocks_by_heading(html) {
        let heading_lower = heading.to_ascii_lowercase();
        if heading_lower.contains("verify") || heading_lower.contains("quick verification") {
            continue;
        }
        let command = strip_book_extract_scaffolding(&code, extract_dir.as_deref());
        if !command.trim().is_empty() {
            commands.push(command);
        }
    }
    if commands.is_empty() {
        anyhow::bail!(
            "No shell command blocks were found for {} at {}",
            package.name,
            package.page_url
        );
    }

    Ok(PageRecipe {
        input_files,
        source_urls,
        extract_dir,
        commands,
        dependencies: dependency_names_from_items(list_items_after_heading(html, "Dependencies")),
        license: first_list_item_after_heading(html, "Licenses")
            .unwrap_or_else(|| "unknown".into()),
        description: lead_text(html).unwrap_or_else(|| package.title.clone()),
    })
}

fn shell_code_blocks_by_heading(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;
    let mut heading = String::new();

    loop {
        let h2_pos = rest.find("<h2");
        let pre_pos = rest.find("<pre");

        match (h2_pos, pre_pos) {
            (None, None) => break,

            (Some(h), Some(p)) if h < p => {
                let after_h2_tag = &rest[h..];
                let Some(h2_close) = after_h2_tag.find('>') else {
                    break;
                };
                let after = &after_h2_tag[h2_close + 1..];
                let Some(end) = after.find("</h2>") else {
                    break;
                };

                heading = clean_code_block(&after[..end]);
                rest = &after[end + "</h2>".len()..];
            }

            (Some(_), Some(p)) | (None, Some(p)) => {
                let after_pre = &rest[p..];
                let Some(pre_tag_end) = after_pre.find('>') else {
                    break;
                };

                let pre_body = &after_pre[pre_tag_end + 1..];
                let Some(pre_end) = pre_body.find("</pre>") else {
                    break;
                };

                let raw = &pre_body[..pre_end];

                let code = if let Some(code_start_tag) = raw.find("<code") {
                    let after_code_tag = &raw[code_start_tag..];
                    if let Some(code_tag_end) = after_code_tag.find('>') {
                        let code_body = &after_code_tag[code_tag_end + 1..];
                        let code_body = code_body
                            .split_once("</code>")
                            .map(|(before, _)| before)
                            .unwrap_or(code_body);

                        clean_code_block(code_body)
                    } else {
                        clean_code_block(raw)
                    }
                } else {
                    clean_code_block(raw)
                };

                out.push((heading.clone(), code));
                rest = &pre_body[pre_end + "</pre>".len()..];
            }

            (Some(h), None) => {
                let after_h2_tag = &rest[h..];
                let Some(h2_close) = after_h2_tag.find('>') else {
                    break;
                };
                let after = &after_h2_tag[h2_close + 1..];
                let Some(end) = after.find("</h2>") else {
                    break;
                };

                heading = clean_code_block(&after[..end]);
                rest = &after[end + "</h2>".len()..];
            }
        }
    }

    out
}

fn strip_book_extract_scaffolding(command: &str, extract_dir: Option<&str>) -> String {
    let mut out = Vec::new();
    let mut skip_continuation = false;
    for line in command.lines() {
        let trimmed = line.trim();
        if skip_continuation {
            skip_continuation = trimmed.ends_with('\\');
            continue;
        }
        if trimmed == "cd \"$LBI_SOURCES\""
            || trimmed == "cd /sources"
            || trimmed == "cd \"/sources\""
        {
            continue;
        }
        if extract_dir
            .is_some_and(|dir| trimmed == format!("cd {dir}") || trimmed == format!("rm -rf {dir}"))
        {
            continue;
        }
        if let Some(dir) = extract_dir
            && let Some(subdir) = cd_subdir_after_extract_dir(trimmed, dir)
        {
            let indent_len = line.len() - line.trim_start().len();
            out.push(format!("{}cd {subdir}", &line[..indent_len]));
            continue;
        }
        if is_book_archive_extract_command(trimmed) {
            skip_continuation = trimmed.ends_with('\\');
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

fn is_book_archive_extract_command(trimmed: &str) -> bool {
    if trimmed.contains("../") {
        return false;
    }

    let Some(command) = trimmed.split_whitespace().next() else {
        return false;
    };
    command == "unzip" || (command == "tar" && trimmed.split_whitespace().any(|arg| arg == "-xf"))
}

fn cd_subdir_after_extract_dir<'a>(trimmed: &'a str, extract_dir: &str) -> Option<&'a str> {
    let dir = trimmed.strip_prefix("cd ")?;
    let dir = dir.trim_matches('"').trim_matches('\'');
    let subdir = dir.strip_prefix(&format!("{extract_dir}/"))?;
    (!subdir.is_empty() && !subdir.starts_with('/')).then_some(subdir)
}

fn rewrite_lbi_command(command: &str, extract_dir: Option<&str>) -> String {
    let mut rewritten = command
        .replace(" /sources", " \"$LBI_SOURCES\"")
        .replace("cd \"/sources\"", "cd \"$LBI_SOURCES\"")
        .replace("/sources/", "$LBI_SOURCES/")
        .replace(
            "$LBI_ROOT/system/tools/bin/",
            "$LBI_SYSROOT/system/tools/bin/",
        )
        .replace(
            "$LBI_ROOT/system/tools/lib/",
            "$LBI_SYSROOT/system/tools/lib/",
        )
        .replace(
            "-DCMAKE_SYSROOT=\"$LBI_ROOT\"",
            "-DCMAKE_SYSROOT=\"$LBI_SYSROOT\"",
        )
        .replace(
            "-DCMAKE_FIND_ROOT_PATH=\"$LBI_ROOT;$LBI_ROOT/system\"",
            "-DCMAKE_FIND_ROOT_PATH=\"$LBI_SYSROOT;$LBI_SYSROOT/system\"",
        )
        .replace("--sysroot=$LBI_ROOT", "--sysroot=$LBI_SYSROOT")
        .replace("&gt;", ">")
        .replace("&lt;", "<");
    if let Some(dir) = extract_dir {
        rewritten = rewritten
            .replace(&format!("cd {dir}\n"), "")
            .replace(&format!("rm -rf {dir}\n"), "");
    }
    let rewritten = rewrite_absolute_system_paths(&rewritten);
    let rewritten = rewrite_make_flags(&rewritten);
    rewrite_parallel_job_counts(&rewritten)
}

fn rewrite_cross_tool_paths(input: &str) -> String {
    input
        .replace("$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-clang++", "$CXX")
        .replace("$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-clang", "$CC")
        .replace("$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-ar", "$AR")
        .replace("$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-nm", "$NM")
        .replace(
            "$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-ranlib",
            "$RANLIB",
        )
        .replace(
            "$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-objcopy",
            "$OBJCOPY",
        )
        .replace(
            "$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-objdump",
            "$OBJDUMP",
        )
        .replace("$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-strip", "$STRIP")
        .replace("$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-ld", "$LD")
}

fn restore_chroot_compiler_search_paths(input: &str) -> String {
    input
        .replace("-B$DESTDIR/system/", "-B/system/")
        .replace("-B${DESTDIR}/system/", "-B/system/")
        .replace("-I$DESTDIR/system/", "-I/system/")
        .replace("-I${DESTDIR}/system/", "-I/system/")
        .replace("-L$DESTDIR/system/", "-L/system/")
        .replace("-L${DESTDIR}/system/", "-L/system/")
        .replace("-isystem $DESTDIR/system/", "-isystem /system/")
        .replace("-isystem ${DESTDIR}/system/", "-isystem /system/")
}

fn is_clang_driver_config_command(input: &str) -> bool {
    input.contains("/configuration/clang/clang.cfg")
        || input.contains("/configuration/clang/clang++.cfg")
}

fn restore_clang_driver_config_runtime_paths(input: &str) -> String {
    input
        .replace("-B$DESTDIR/system/", "-B/system/")
        .replace("-B${DESTDIR}/system/", "-B/system/")
        .replace("-I$DESTDIR/system/", "-I/system/")
        .replace("-I${DESTDIR}/system/", "-I/system/")
        .replace("-L$DESTDIR/system/", "-L/system/")
        .replace("-L${DESTDIR}/system/", "-L/system/")
        .replace("-isystem $DESTDIR/system/", "-isystem /system/")
        .replace("-isystem ${DESTDIR}/system/", "-isystem /system/")
        .replace("rpath,$DESTDIR/system/", "rpath,/system/")
        .replace("rpath,${DESTDIR}/system/", "rpath,/system/")
}

fn restore_rust_bootstrap_runtime_tool_paths(input: &str) -> String {
    if !input.contains("bootstrap.toml") {
        return input.to_string();
    }

    input
        .replace("$DESTDIR/system", "/system")
        .replace("${DESTDIR}/system", "/system")
        .replace("$DESTDIR/system/binaries/cc", "/system/binaries/cc")
        .replace("${DESTDIR}/system/binaries/cc", "/system/binaries/cc")
        .replace("$DESTDIR/system/binaries/c++", "/system/binaries/c++")
        .replace("${DESTDIR}/system/binaries/c++", "/system/binaries/c++")
        .replace(
            "$DESTDIR/system/binaries/llvm-ar",
            "/system/binaries/llvm-ar",
        )
        .replace(
            "${DESTDIR}/system/binaries/llvm-ar",
            "/system/binaries/llvm-ar",
        )
        .replace(
            "$DESTDIR/system/binaries/llvm-ranlib",
            "/system/binaries/llvm-ranlib",
        )
        .replace(
            "${DESTDIR}/system/binaries/llvm-ranlib",
            "/system/binaries/llvm-ranlib",
        )
        .replace(
            "$DESTDIR/system/binaries/llvm-config",
            "/system/binaries/llvm-config",
        )
        .replace(
            "${DESTDIR}/system/binaries/llvm-config",
            "/system/binaries/llvm-config",
        )
}

fn rewrite_absolute_system_paths(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"/system") && is_system_path_boundary(input, i) {
            let prefix = &input[..i];

            let already_destdir = prefix.ends_with("$DESTDIR")
                || prefix.ends_with("${DESTDIR}")
                || prefix.ends_with("\"$DESTDIR")
                || prefix.ends_with("\"${DESTDIR}")
                || prefix.ends_with("$LBI_ROOT")
                || prefix.ends_with("${LBI_ROOT}")
                || prefix.ends_with("\"$LBI_ROOT\"")
                || prefix.ends_with("\"${LBI_ROOT}\"")
                || prefix.ends_with("$LBI_SYSROOT")
                || prefix.ends_with("${LBI_SYSROOT}")
                || prefix.ends_with("\"$LBI_SYSROOT\"")
                || prefix.ends_with("\"${LBI_SYSROOT}\"")
                || prefix_ends_with_shell_assignment(prefix)
                || prefix_ends_with_configure_path_option(prefix);

            if !already_destdir {
                out.push_str("$DESTDIR");
            }

            out.push_str("/system");
            i += "/system".len();
        } else {
            let ch = input[i..]
                .chars()
                .next()
                .expect("byte index must be inside string");
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    out
}

fn prefix_ends_with_configure_path_option(prefix: &str) -> bool {
    let token_start = prefix
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| {
            (ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '('))
                .then_some(idx + ch.len_utf8())
        })
        .unwrap_or(0);
    let token = &prefix[token_start..];
    let Some((option, _)) = token.split_once('=') else {
        return false;
    };
    matches!(
        option,
        "--prefix"
            | "--exec-prefix"
            | "--bindir"
            | "--sbindir"
            | "--libdir"
            | "--includedir"
            | "--sysconfdir"
            | "--localstatedir"
            | "--datarootdir"
            | "--datadir"
            | "--mandir"
            | "--infodir"
            | "--with-default-sys-path"
    )
}

fn is_system_path_boundary(input: &str, start: usize) -> bool {
    let after = start + "/system".len();
    matches!(
        input.as_bytes().get(after).copied(),
        None | Some(b'/')
            | Some(b':')
            | Some(b'"')
            | Some(b'\'')
            | Some(b' ')
            | Some(b'\t')
            | Some(b'\n')
    )
}

fn prefix_ends_with_shell_assignment(prefix: &str) -> bool {
    let token_start = prefix
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| {
            (ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '('))
                .then_some(idx + ch.len_utf8())
        })
        .unwrap_or(0);
    let token = &prefix[token_start..];
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let name = name.strip_prefix("-D").unwrap_or(name);
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn source_url_for_input(
    input: &str,
    package: &BookPackage,
    recipe: &PageRecipe,
    manifest: &[ManifestEntry],
) -> Result<String> {
    for url in &recipe.source_urls {
        if filename_from_url(url) == input || url.ends_with(input) {
            return Ok(url.trim_start_matches("git+").to_string());
        }
    }
    if let Some(entry) = manifest.iter().find(|entry| entry.output_name == input) {
        return Ok(entry.url.clone());
    }
    let mut filename_matches = manifest
        .iter()
        .filter(|entry| filename_from_url(&entry.url) == input)
        .collect::<Vec<_>>();
    if filename_matches.len() > 1 {
        filename_matches.retain(|entry| manifest_entry_matches_package(entry, package));
    }
    if let Some(entry) = filename_matches.first() {
        return Ok(entry.url.clone());
    }
    if input.ends_with(".patch") || input.ends_with(".diff") {
        return Ok(Url::parse(&package.page_url)?
            .join(&format!("../../patches/{input}"))?
            .to_string());
    }
    anyhow::bail!(
        "Could not find source URL for input file {} required by {}",
        input,
        package.name
    )
}

fn manifest_entry_matches_package(entry: &ManifestEntry, package: &BookPackage) -> bool {
    let package = normalize_source_match_token(&package.name);
    !package.is_empty()
        && (normalize_source_match_token(&entry.output_name).contains(&package)
            || normalize_source_match_token(&entry.url).contains(&package))
}

fn source_urls_from_prelude(html: &str) -> Vec<String> {
    code_values(html)
        .into_iter()
        .filter(|code| source_value_looks_like_url(code))
        .collect()
}

fn input_files_from_prelude(html: &str) -> Vec<String> {
    let explicit_files = input_files_from_explicit_metadata(html);
    if !explicit_files.is_empty() {
        return explicit_files;
    }

    collect_input_files_from_code_values(code_values(html))
}

fn input_files_from_explicit_metadata(html: &str) -> Vec<String> {
    let mut input_files = Vec::new();
    let mut seen_input_files = BTreeSet::new();

    for paragraph in paragraph_bodies(html) {
        if !paragraph_has_input_file_metadata_label(paragraph) {
            continue;
        }

        for file in collect_input_files_from_code_values(code_values(paragraph)) {
            if seen_input_files.insert(file.clone()) {
                input_files.push(file);
            }
        }
    }

    input_files
}

fn paragraph_bodies(html: &str) -> Vec<&str> {
    let mut paragraphs = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<p") {
        let after_start = &rest[start..];
        let Some(tag_end) = after_start.find('>') else {
            break;
        };
        let body_start = start + tag_end + 1;
        let after_body = &rest[body_start..];
        let Some(body_end) = after_body.find("</p>") else {
            break;
        };

        paragraphs.push(&rest[body_start..body_start + body_end]);
        rest = &after_body[body_end + "</p>".len()..];
    }
    paragraphs
}

fn paragraph_has_input_file_metadata_label(paragraph: &str) -> bool {
    let text = strip_html_tags(paragraph).to_ascii_lowercase();
    let Some((label, _)) = text.split_once(':') else {
        return false;
    };
    matches!(
        label.trim(),
        "input assumption"
            | "input assumptions"
            | "input file"
            | "input files"
            | "source package"
            | "source packages"
            | "source archive"
            | "source archives"
            | "patch"
            | "patches"
    )
}

fn collect_input_files_from_code_values(code_values: Vec<String>) -> Vec<String> {
    let mut input_files = Vec::new();
    let mut seen_input_files = BTreeSet::new();
    for code in code_values {
        for file in words_that_look_like_input_files(&code) {
            if seen_input_files.insert(file.clone()) {
                input_files.push(file);
            }
        }
    }
    input_files
}

fn extract_dir_from_html(html: &str) -> Option<String> {
    for (_, code) in shell_code_blocks_by_heading(html) {
        for line in code.lines() {
            let trimmed = line.trim();
            if trimmed == "cd \"$LBI_SOURCES\""
                || trimmed == "cd /sources"
                || trimmed == "cd \"/sources\""
            {
                continue;
            }
            if let Some(dir) = trimmed.strip_prefix("cd ") {
                let dir = dir.trim_matches('"').trim_matches('\'');
                if !dir.starts_with('$') && !dir.starts_with('/') && !dir.contains(' ') {
                    return dir.split('/').next().map(str::to_string);
                }
            }
        }
    }
    None
}

fn code_values(html: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<code") {
        let after = &rest[start..];
        let Some(close) = after.find('>') else {
            break;
        };
        let value_start = start + close + 1;
        let after_value = &rest[value_start..];
        let Some(end) = after_value.find("</code>") else {
            break;
        };
        values.push(html_decode(&after_value[..end]));
        rest = &after_value[end + "</code>".len()..];
    }
    values
}

fn words_that_look_like_input_files(input: &str) -> Vec<String> {
    input
        .split(|c: char| {
            c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == '"' || c == '\''
        })
        .map(|word| word.trim_matches(|c: char| c == '`' || c == '.' || c == ';'))
        .filter(|word| {
            !source_value_looks_like_url(word)
                && (is_archive_filename(word)
                    || word.ends_with(".patch")
                    || word.ends_with(".diff")
                    || word.ends_with(".pem"))
        })
        .map(str::to_string)
        .collect()
}

fn source_value_looks_like_url(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("git+http://")
        || lower.starts_with("git+https://")
}

fn first_list_item_after_heading(html: &str, heading: &str) -> Option<String> {
    list_items_after_heading(html, heading).into_iter().next()
}

fn list_items_after_heading(html: &str, heading: &str) -> Vec<String> {
    let marker = format!("<h2>{heading}:</h2>");
    let Some(after) = html.split_once(&marker).map(|(_, after)| after) else {
        return Vec::new();
    };
    let list = after
        .split_once("</ul>")
        .map(|(list, _)| list)
        .unwrap_or(after);
    let mut items = Vec::new();
    let mut rest = list;
    while let Some((_, after_li)) = rest.split_once("<li>") {
        let Some((item, after_item)) = after_li.split_once("</li>") else {
            break;
        };
        let item = strip_html_tags(item);
        if !item.is_empty() {
            items.push(item);
        }
        rest = after_item;
    }
    items
}

fn dependency_names_from_items(items: Vec<String>) -> Vec<String> {
    let mut deps = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items {
        for dep in normalize_dependency_item(&item) {
            if seen.insert(dep.clone()) {
                deps.push(dep);
            }
        }
    }
    deps
}

fn normalize_dependency_item(item: &str) -> Vec<String> {
    let item = item.replace('`', "");
    let lower = item.trim().to_ascii_lowercase();
    if lower.is_empty()
        || lower.contains("(host)")
        || lower.starts_with("host ")
        || lower.contains("host c compiler")
    {
        return Vec::new();
    }

    let dep = if lower.contains("musl") || lower == "pthreads" {
        "musl"
    } else if lower.contains("clang/llvm") || lower.contains("llvm toolchain") {
        "llvm"
    } else if lower.starts_with("compiler-rt") {
        "compiler-rt"
    } else if lower.starts_with("libunwind") {
        "libunwind"
    } else if lower.starts_with("libcxxabi") {
        "libcxxabi"
    } else if lower.starts_with("libcxx") {
        "libcxx"
    } else if lower.starts_with("lld") {
        "lld"
    } else if lower.starts_with("llvm") {
        "llvm"
    } else if lower.starts_with("clang") || lower.starts_with("c++ compiler") {
        "clang"
    } else if lower.starts_with("cargo") {
        "cargo"
    } else if lower.starts_with("rustc") {
        "rustc"
    } else if lower.starts_with("libressl") {
        "libressl"
    } else if lower.starts_with("sqlite") {
        "sqlite"
    } else if lower.starts_with("zlib-ng") {
        "zlib-ng"
    } else if lower.starts_with("xz") {
        "xz"
    } else if lower.starts_with("zstd") {
        "zstd"
    } else if lower.starts_with("curl") {
        "curl"
    } else if lower.starts_with("pkgconf") {
        "pkgconf"
    } else if lower.starts_with("ca-certificates") {
        "ca-certificates"
    } else if lower.starts_with("python-flit-core") || lower.starts_with("flit-core") {
        "python-flit-core"
    } else if lower.starts_with("python-packaging") || lower.starts_with("packaging") {
        "python-packaging"
    } else if lower.starts_with("python-wheel") || lower.starts_with("wheel") {
        "python-wheel"
    } else if lower.starts_with("python-setuptools") || lower.starts_with("setuptools") {
        "python-setuptools"
    } else if lower.starts_with("python") {
        "python"
    } else if lower.starts_with("pip") {
        "pip"
    } else if lower.starts_with("cmake") {
        "cmake"
    } else if lower.starts_with("meson") {
        "meson"
    } else if lower.starts_with("samurai") {
        "samurai"
    } else if lower.starts_with("ninja") {
        "ninja"
    } else if lower.starts_with("bmake") {
        "bmake"
    } else if lower.starts_with("make") {
        "make"
    } else if lower.starts_with("bison")
        || lower.starts_with("yacc")
        || lower.contains("yacc-compatible")
    {
        "byacc"
    } else if lower.starts_with("lex") || lower.starts_with("flex") {
        "flex"
    } else if lower == "m4" || lower.starts_with("m4 ") {
        "m4"
    } else if lower.starts_with("om4") {
        "om4"
    } else if lower.starts_with("ncurses") {
        "ncurses"
    } else if lower.starts_with("libedit") {
        "libedit"
    } else if lower.starts_with("libarchive") {
        "libarchive"
    } else if lower.starts_with("bheaded") {
        "bheaded"
    } else if lower.starts_with("awk") {
        "awk"
    } else {
        return Vec::new();
    };

    vec![dep.to_string()]
}

fn lead_text(html: &str) -> Option<String> {
    let after = html.split_once("<p class=\"lead\">")?.1;
    let text = after.split_once("</p>")?.0;
    Some(strip_html_tags(text))
}

fn strip_html_tags(input: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_decode(out.trim())
}

fn html_decode(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

fn clean_code_block(input: &str) -> String {
    let stripped = strip_html_tags(input);
    stripped
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_archive_filename(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
        || lower.ends_with(".tar.bz2")
        || lower.ends_with(".tbz2")
        || lower.ends_with(".zip")
        || lower.ends_with(".tar")
}

fn strip_archive_suffix(input: &str) -> &str {
    for suffix in [
        ".tar.gz", ".tar.xz", ".tar.bz2", ".tgz", ".txz", ".tbz2", ".zip", ".tar",
    ] {
        if let Some(stripped) = input.strip_suffix(suffix) {
            return stripped;
        }
    }
    input
}

fn filename_from_url(url: &str) -> String {
    let clean = url
        .trim_start_matches("git+")
        .split('#')
        .next()
        .unwrap_or(url);
    Url::parse(clean)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_string))
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| clean.rsplit('/').next().unwrap_or(clean).to_string())
}

fn normalize_source_match_token(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn toml_escape(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shell_double_quote_literal(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

fn version_from_title(title: &str) -> String {
    title
        .split_whitespace()
        .rev()
        .find(|word| word.chars().any(|c| c.is_ascii_digit()))
        .map(|word| {
            word.trim_matches(|c: char| c == ',' || c == ';' || c == ':' || c == '.')
                .to_string()
        })
        .unwrap_or_else(|| "0".to_string())
}

fn page_slug_from_title(title: &str, package: &str) -> String {
    let lower = normalize_title(title);

    match lower.as_str() {
        s if s.starts_with("musl libc final pass") => "musl-libc-final-pass".to_string(),
        s if s.starts_with("llvm final") => "llvm-final".to_string(),

        // These chapter 8 pages are stage-2 packages in the title,
        // but the actual LBI page filenames do not include "-stage2".
        s if s.starts_with("zstd") => "zstd".to_string(),
        s if s.starts_with("samurai") => "samurai".to_string(),

        s if s.contains("stage 2") => format!("{package}-stage2"),

        _ => package.to_string(),
    }
}

fn book_base_url(book_url: &str) -> Result<Url> {
    let mut url = Url::parse(book_url).with_context(|| format!("Invalid book URL: {book_url}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Book URL cannot be a base URL: {book_url}"))?;
        segments.pop();
        segments.push("");
    }
    Ok(url)
}

fn parse_book_steps(text: &str, book_url: &str) -> Result<Vec<BookStep>> {
    let book_base = book_base_url(book_url)?;
    let mut sections = Vec::<(String, String)>::new();
    let mut seen_sections = BTreeSet::<String>::new();
    for line in text.lines() {
        if let Some((section, title)) = parse_section_line(line)
            && seen_sections.insert(section.clone())
        {
            sections.push((section, title));
        }
    }

    let mut steps = Vec::new();
    for (section, title) in sections {
        let Some(chapter) = section_chapter(&section) else {
            continue;
        };
        if let Some(kind) = operation_kind_from_title(&title) {
            steps.push(BookStep::Operation(BookOperation {
                section: section.clone(),
                title: title.clone(),
                kind,
                recipe_id: format!("{}-{}", section.replace('.', "-"), operation_slug(kind)),
            }));
            continue;
        }

        let Some(name) = package_name_from_title(&title) else {
            continue;
        };
        let slug = page_slug_from_title(&title, &name);
        let page_url = book_base
            .join(&format!("chapters/chapter{chapter}/{slug}.html"))?
            .to_string();
        let recipe_id = format!("{}-{}", section.replace('.', "-"), name);
        steps.push(BookStep::Package(BookPackage {
            chapter,
            section: section.clone(),
            title: title.clone(),
            version: version_from_title(&title),
            layer: layer_for_package(chapter, &name),
            name,
            page_url,
            recipe_id,
        }));
    }
    Ok(steps)
}

fn packages_from_steps(steps: &[BookStep]) -> Vec<BookPackage> {
    steps
        .iter()
        .filter_map(|step| match step {
            BookStep::Package(package) => Some(package.clone()),
            BookStep::Operation(_) => None,
        })
        .collect()
}

impl BookStep {
    fn section(&self) -> &str {
        match self {
            BookStep::Package(package) => &package.section,
            BookStep::Operation(operation) => &operation.section,
        }
    }
}

fn packages_by_layer(packages: &[BookPackage]) -> BTreeMap<String, Vec<String>> {
    let mut layers = BTreeMap::<String, Vec<String>>::new();
    let mut seen = BTreeMap::<String, BTreeSet<String>>::new();
    for package in packages {
        if seen
            .entry(package.layer.clone())
            .or_default()
            .insert(package.name.clone())
        {
            layers
                .entry(package.layer.clone())
                .or_default()
                .push(package.name.clone());
        }
    }
    layers
}

fn bootstrap_layer_packages_for_state(
    layers: &BTreeMap<String, Vec<String>>,
    layer: &str,
) -> Vec<String> {
    let mut packages = layers.get(layer).cloned().unwrap_or_default();
    if layer == BASE_LAYER && !packages.iter().any(|package| package == FILESYSTEM_PACKAGE) {
        packages.insert(0, FILESYSTEM_PACKAGE.to_string());
    }
    filter_retired_layer_packages(layer, packages)
}

fn filter_retired_layer_packages(layer: &str, packages: Vec<String>) -> Vec<String> {
    if layer != TEMP_LAYER {
        return packages;
    }

    packages
        .into_iter()
        .filter(|package| !CHAPTER7_RETIRED_PACKAGES.contains(&package.as_str()))
        .collect()
}

fn parse_section_line(line: &str) -> Option<(String, String)> {
    let trimmed = line
        .trim_start_matches(|c: char| c == '\u{c}' || c.is_whitespace())
        .trim_start_matches(['▪', '•', '◦', '*', '-'])
        .trim();
    let mut fields = trimmed.split_whitespace();
    let section_raw = fields.next()?.trim_end_matches('.');
    if !section_raw.contains('.') {
        return None;
    }
    if !section_raw
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let chapter = section_chapter(section_raw)?;
    if !(5..=9).contains(&chapter) {
        return None;
    }
    let title = fields.collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return None;
    }
    Some((section_raw.to_string(), title))
}

fn section_chapter(section: &str) -> Option<u8> {
    section.split('.').next()?.parse::<u8>().ok()
}

fn package_name_from_title(title: &str) -> Option<String> {
    let lower = normalize_title(title);
    if should_skip_title(&lower) {
        return None;
    }

    let mapped = match lower.as_str() {
        s if s.starts_with("linux api headers") => "linux-api-headers",
        s if s.starts_with("musl libc headers") => "musl-libc-headers",
        s if s.starts_with("llvm/clang pass 1") => "llvm-clang-pass1",
        s if s.starts_with("musl libc pass 2") => "musl-libc-pass2",
        s if s.starts_with("llvm runtimes") => "llvm-runtimes",
        s if s.starts_with("llvm/clang pass 2") => "llvm-clang-pass2",
        s if s.starts_with("musl libc final pass") => "musl",
        s if s.starts_with("gnu make") => "make",
        s if s.starts_with("bsd-diffutils") => "bsddiffutils",
        s if s.starts_with("patch stage 2") => "bsdpatch",
        s if s.starts_with("llvm final") => "llvm",
        s if s.starts_with("cmake") => "cmake",
        s if s.starts_with("shadow") => "shadow",
        s if s.starts_with("libressl") => "libressl",
        s if s.starts_with("python-flit-core") => "python-flit-core",
        s if s.starts_with("ca-certificates") => "ca-certificates",
        _ => return generic_package_name(&lower),
    };
    Some(mapped.to_string())
}

fn normalize_title(title: &str) -> String {
    let no_parens = title
        .split_once('(')
        .map(|(before, _)| before)
        .unwrap_or(title);
    no_parens
        .trim()
        .trim_end_matches('.')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn should_skip_title(title: &str) -> bool {
    if title.starts_with("copy selected build variables and helper functions into target") {
        return false;
    }

    matches!(
        title,
        "introduction"
            | "sources"
            | "final checks"
            | "reset target tree ownership to root"
            | "create virtual filesystem link targets"
            | "copy selected build variables and helper functions into target profile"
            | "enter chroot environment"
            | "create essential system files"
            | "cleanup"
            | "dinit service setup"
    )
}

fn operation_kind_from_title(title: &str) -> Option<BookOperationKind> {
    let title = normalize_title(title);
    if title.starts_with("copy selected build variables and helper functions into target") {
        return Some(BookOperationKind::CopyBuildProfile);
    }

    match title.as_str() {
        "reset target tree ownership to root" => Some(BookOperationKind::ResetTargetTreeOwnership),
        "create virtual filesystem link targets" => {
            Some(BookOperationKind::CreateVirtualFilesystemLinkTargets)
        }
        "enter chroot environment" => Some(BookOperationKind::EnterChroot),
        "create essential system files" => Some(BookOperationKind::CreateEssentialSystemFiles),
        _ => None,
    }
}

fn operation_slug(kind: BookOperationKind) -> &'static str {
    match kind {
        BookOperationKind::ResetTargetTreeOwnership => "reset-ownership",
        BookOperationKind::CreateVirtualFilesystemLinkTargets => "virtual-filesystems",
        BookOperationKind::CopyBuildProfile => "copy-build-profile",
        BookOperationKind::EnterChroot => "enter-chroot",
        BookOperationKind::CreateEssentialSystemFiles => "essential-files",
    }
}

fn generic_package_name(title: &str) -> Option<String> {
    let mut words = Vec::new();
    for word in title.split_whitespace() {
        let cleaned = word.trim_matches(|c: char| {
            c == ',' || c == ';' || c == ':' || c == '.' || c == '(' || c == ')'
        });
        if cleaned.is_empty() || !cleaned.chars().any(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        if cleaned.eq_ignore_ascii_case("stage")
            || cleaned.eq_ignore_ascii_case("pass")
            || cleaned.eq_ignore_ascii_case("final")
            || cleaned.eq_ignore_ascii_case("git")
            || cleaned.eq_ignore_ascii_case("main")
            || cleaned.eq_ignore_ascii_case("master")
            || cleaned.eq_ignore_ascii_case("snapshot")
            || cleaned.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            break;
        }
        words.push(cleaned);
    }
    if words.is_empty() {
        return None;
    }
    let package = words.join("-").replace('/', "-");
    package
        .chars()
        .any(|c| c.is_ascii_alphanumeric())
        .then_some(package)
}

fn layer_for_package(chapter: u8, package: &str) -> String {
    if package == "linux-api-headers" {
        return DEVEL_LAYER.to_string();
    }

    if chapter == 5 || chapter == 6 {
        return TEMP_LAYER.to_string();
    }

    if is_devel_package(package) {
        DEVEL_LAYER.to_string()
    } else {
        BASE_LAYER.to_string()
    }
}

fn is_devel_package(package: &str) -> bool {
    matches!(
        package,
        "byacc"
            | "bmake"
            | "cmake"
            | "flex"
            | "llvm"
            | "make"
            | "pkgconf"
            | "python"
            | "python-flit-core"
            | "python-packaging"
            | "python-setuptools"
            | "python-wheel"
            | "rustc"
            | "meson"
            | "samurai"
            | "sqlite"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use std::ffi::OsStr;

    const SAMPLE_TOC: &str = r#"
       5.1 Linux API Headers 7.0
       5.3 llvm/clang pass 1 22.1.3
       6.17 GNU Make 4.4.1
       7.1 Introduction
       ▪ 7.2 Reset Target Tree Ownership to root
       ▪ 7.3 Create virtual filesystem link targets
       ▪ 7.4 Copy selected build variables and helper functions into target
         profile
       ▪ 7.5 Enter chroot environment
       ▪ 7.6 Create essential system files
       7.7 gettext-tiny 0.3.3
       7.8 byacc 20260126
       8.2 iana-etc 20260409
       8.4 pigz stage 2 2.8
       8.14.1 ca-certificates 2026-03-19
       ▪ 8.17 libffi 3.5.2
       ▪ 8.18 python 3.14.4
       ▪ 8.20 Python-Packaging 26.2
       ▪ 8.21 Python-Wheel 0.47.0
       ▪ 8.22 Python-Setuptools 82.0.1
       ▪ 8.23 Meson 1.11.1
       8.29 CMake 4.3.2
       8.34 LLVM final 22.1.3
       9.2 Limine 11.4.1
       9.3 dinit service setup
    "#;

    #[test]
    fn parses_book_packages_into_layers() {
        let steps =
            parse_book_steps(SAMPLE_TOC, "https://www.vertexlinux.net/lbi/book.pdf").unwrap();
        let packages = packages_from_steps(&steps);
        let layers = packages_by_layer(&packages);
        assert_eq!(layers[TEMP_LAYER], vec!["llvm-clang-pass1", "make"]);
        assert_eq!(
            layers[BASE_LAYER],
            vec![
                "gettext-tiny",
                "iana-etc",
                "pigz",
                "ca-certificates",
                "libffi",
                "limine"
            ]
        );
        assert_eq!(
            layers[DEVEL_LAYER],
            vec![
                "linux-api-headers",
                "byacc",
                "python",
                "python-packaging",
                "python-wheel",
                "python-setuptools",
                "meson",
                "cmake",
                "llvm"
            ]
        );
        assert_eq!(packages[0].recipe_id, "5-1-linux-api-headers");
        assert_eq!(
            packages[0].page_url,
            "https://www.vertexlinux.net/lbi/chapters/chapter5/linux-api-headers.html"
        );
        assert_eq!(packages.last().unwrap().name, "limine");
        assert!(packages.iter().all(|package| package.chapter <= 9));
    }

    #[test]
    fn bootstrap_state_base_layer_includes_filesystem_package() {
        let steps =
            parse_book_steps(SAMPLE_TOC, "https://www.vertexlinux.net/lbi/book.pdf").unwrap();
        let packages = packages_from_steps(&steps);
        let layers = packages_by_layer(&packages);

        assert_eq!(
            bootstrap_layer_packages_for_state(&layers, BASE_LAYER),
            vec![
                "filesystem",
                "gettext-tiny",
                "iana-etc",
                "pigz",
                "ca-certificates",
                "libffi",
                "limine"
            ]
        );
    }

    #[test]
    fn parses_book_steps_including_operational_sections() {
        let steps =
            parse_book_steps(SAMPLE_TOC, "https://www.vertexlinux.net/lbi/book.pdf").unwrap();
        let operations = steps
            .iter()
            .filter_map(|step| match step {
                BookStep::Operation(operation) => {
                    Some((operation.section.as_str(), operation.kind))
                }
                BookStep::Package(_) => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            operations,
            vec![
                ("7.2", BookOperationKind::ResetTargetTreeOwnership),
                ("7.3", BookOperationKind::CreateVirtualFilesystemLinkTargets),
                ("7.4", BookOperationKind::CopyBuildProfile),
                ("7.5", BookOperationKind::EnterChroot),
                ("7.6", BookOperationKind::CreateEssentialSystemFiles),
            ]
        );
        assert!(matches!(steps[0], BookStep::Package(_)));
        assert!(matches!(steps[3], BookStep::Operation(_)));
    }

    #[test]
    fn parse_book_steps_preserves_book_order_for_reordered_sections() {
        let steps = parse_book_steps(
            r#"
       8.25.1 samurai stage 2 1.3
       8.26 BSD-Diffutils stage 2 0.99.0
       8.25.1 samurai stage 2 1.3
    "#,
            "https://www.vertexlinux.net/lbi/book.pdf",
        )
        .unwrap();
        let packages = packages_from_steps(&steps);

        assert_eq!(
            packages
                .iter()
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            vec!["samurai", "bsddiffutils"]
        );
    }

    #[test]
    fn fresh_book_fetch_url_adds_cache_buster_without_changing_path() {
        let fetch_url =
            fresh_book_fetch_url("https://www.vertexlinux.net/lbi/book.pdf?existing=1").unwrap();
        let parsed = Url::parse(&fetch_url).unwrap();

        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("www.vertexlinux.net"));
        assert_eq!(parsed.path(), "/lbi/book.pdf");
        let pairs = parsed.query_pairs().collect::<Vec<_>>();
        assert!(
            pairs
                .iter()
                .any(|(key, value)| key == "existing" && value == "1")
        );
        assert!(
            pairs
                .iter()
                .any(|(key, value)| key == BOOK_FETCH_CACHE_BUST_PARAM && !value.is_empty())
        );
    }

    #[test]
    fn parses_lbi_b2sum_manifest_by_filename() {
        let sums = parse_source_b2sums(
            r#"
# Linux by Intent starter source checksums (BLAKE2b-512)
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  musl-1.2.6.tar.gz
BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB *linux-by-intent-patches.zip
"#,
        )
        .unwrap();

        assert_eq!(
            sums["musl-1.2.6.tar.gz"],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            sums["linux-by-intent-patches.zip"],
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn registers_filesystem_package_for_lbi_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config::Config::for_rootfs(tmp.path());

        register_filesystem_package(tmp.path(), &config).unwrap();

        let db_path = config.installed_db_path(tmp.path());
        let installed = crate::db::get_installed_packages(&db_path).unwrap();
        assert!(installed.contains(FILESYSTEM_PACKAGE));

        let files = crate::db::get_package_files(&db_path, FILESYSTEM_PACKAGE).unwrap();
        assert!(files.contains(&"etc".to_string()));
        assert!(files.contains(&"system/configuration/passwd".to_string()));

        let groups = crate::db::get_package_groups(&db_path, FILESYSTEM_PACKAGE).unwrap();
        assert_eq!(groups, vec![BASE_LAYER.to_string()]);
    }

    #[test]
    fn page_recipe_preserves_input_order_and_strips_extract_scaffolding() {
        let package = BookPackage {
            chapter: 6,
            section: "6.7".to_string(),
            title: "musl libc pass 2 1.2.5".to_string(),
            name: "musl-libc-pass2".to_string(),
            version: "1.2.5".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://www.vertexlinux.net/lbi/chapters/chapter6/musl-libc-pass2.html"
                .to_string(),
            recipe_id: "6-7-musl-libc-pass2".to_string(),
        };
        let html = r#"
            <p class="lead">Build musl for the temporary toolchain.</p>
            <p><strong>Source package:</strong> <code>musl-1.2.5.tar.gz</code></p>
            <p><strong>Source package:</strong> <code>mimalloc-v3.3.0.tar.gz</code></p>
            <p><strong>Source URLs:</strong> <code>https://musl.libc.org/releases/musl-1.2.5.tar.gz</code> and <code>https://github.com/microsoft/mimalloc/archive/refs/tags/v3.3.0.tar.gz</code></p>
            <p><strong>Source package:</strong> <code>musl-fix.patch</code></p>
            <h2>Extract:</h2>
            <pre><code>cd "$LBI_SOURCES"
tar -xf musl-1.2.5.tar.gz
cd musl-1.2.5</code></pre>
            <h2>Build:</h2>
            <pre><code>patch -Np1 -i ../musl-fix.patch
tar -xf ../mimalloc-v3.3.0.tar.gz
./configure --prefix=/system
make</code></pre>
            <h2>Install:</h2>
            <pre><code>make DESTDIR="$LBI_ROOT" install
rm -rf musl-1.2.5</code></pre>
            <h2>Licenses:</h2><ul><li>MIT</li></ul>
        "#;

        let recipe = parse_page_recipe(html, &package).unwrap();

        assert_eq!(
            recipe.input_files,
            vec![
                "musl-1.2.5.tar.gz",
                "mimalloc-v3.3.0.tar.gz",
                "musl-fix.patch"
            ]
        );
        assert_eq!(
            recipe.source_urls,
            vec![
                "https://musl.libc.org/releases/musl-1.2.5.tar.gz",
                "https://github.com/microsoft/mimalloc/archive/refs/tags/v3.3.0.tar.gz"
            ]
        );
        assert_eq!(recipe.extract_dir.as_deref(), Some("musl-1.2.5"));
        assert!(
            !recipe
                .commands
                .iter()
                .any(|cmd| cmd.contains("tar -xf musl"))
        );
        assert!(
            recipe
                .commands
                .iter()
                .any(|cmd| cmd.contains("../musl-fix.patch"))
        );
        assert!(
            recipe
                .commands
                .iter()
                .any(|cmd| cmd.contains("../mimalloc-v3.3.0.tar.gz"))
        );
        assert_eq!(recipe.license, "MIT");
    }

    #[test]
    fn page_recipe_ignores_source_note_archives_when_input_assumption_exists() {
        let package = BookPackage {
            chapter: 9,
            section: "9.2".to_string(),
            title: "Limine 11.4.1 binary release".to_string(),
            name: "limine".to_string(),
            version: "11.4.1".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://www.vertexlinux.net/lbi/chapters/chapter9/limine.html".to_string(),
            recipe_id: "9-2-limine".to_string(),
        };
        let html = r#"
            <p class="lead">Install Limine's bootloader payloads.</p>
            <div class="pull-note">
            <p><strong>Input assumption:</strong> <code>limine-5be26a73d7b7.tar.gz</code> is already present in <code>/sources</code>.</p>
            <p><strong>Source URL:</strong> <code>https://github.com/Limine-Bootloader/Limine/commit/5be26a73d7b7b4d4477d18be94e1d16e615adf56</code></p>
            <p><strong>Source note:</strong> this is the upstream <code>v11.4.1-binary</code> snapshot. Unlike the full <code>limine-11.4.1.tar.xz</code> source release, it does not need <code>nasm</code>.</p>
            </div>
            <h2>Extract and Enter the Source Tree</h2>
            <pre><code>cd /sources
tar -xf limine-5be26a73d7b7.tar.gz
cd limine-5be26a73d7b7</code></pre>
            <h2>Build the Limine Utility</h2>
            <pre><code>make $LWI_MAKE_FLAGS CC=clang</code></pre>
            <h2>Install Limine</h2>
            <pre><code>install -Dm755 limine /system/binaries/limine</code></pre>
            <h2>Licenses:</h2><ul><li>BSD 2-Clause</li></ul>
        "#;
        let manifest = parse_source_manifest(
            "git+https://github.com/Limine-Bootloader/Limine.git#5be26a73d7b7b4d4477d18be94e1d16e615adf56 limine-5be26a73d7b7.tar.gz",
        );

        let recipe = parse_page_recipe(html, &package).unwrap();
        let source_url =
            source_url_for_input(&recipe.input_files[0], &package, &recipe, &manifest).unwrap();

        assert_eq!(
            recipe.input_files,
            vec!["limine-5be26a73d7b7.tar.gz".to_string()]
        );
        assert_eq!(
            source_url,
            "https://github.com/Limine-Bootloader/Limine.git#5be26a73d7b7b4d4477d18be94e1d16e615adf56"
        );
    }

    #[test]
    fn page_recipe_preserves_book_subdir_after_archive_root() {
        let package = BookPackage {
            chapter: 5,
            section: "5.5".to_string(),
            title: "LLVM runtimes (libunwind, libcxxabi, libcxx) 22.1.3".to_string(),
            name: "llvm-runtimes".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/llvm-runtimes.html".to_string(),
            recipe_id: "5-5-llvm-runtimes".to_string(),
        };
        let html = r#"
            <p class="lead">Build runtimes.</p>
            <p><strong>Source package:</strong> <code>llvm-project-22.1.3.src.tar.xz</code></p>
            <p><strong>Source URLs:</strong> <code>https://example.invalid/llvm-project-22.1.3.src.tar.xz</code></p>
            <h2>Extract:</h2>
            <pre><code>cd "$LBI_SOURCES"
tar -xf llvm-project-22.1.3.src.tar.xz
cd llvm-project-22.1.3.src/runtimes</code></pre>
            <h2>Configure:</h2>
            <pre><code>lbi_cmake build-runtimes \
    -DLIBUNWIND_INSTALL_LIBRARY_DIR=/system/libraries</code></pre>
        "#;

        let recipe = parse_page_recipe(html, &package).unwrap();

        assert_eq!(
            recipe.extract_dir.as_deref(),
            Some("llvm-project-22.1.3.src")
        );
        assert!(
            recipe
                .commands
                .iter()
                .any(|cmd| cmd.trim() == "cd runtimes")
        );
    }

    #[test]
    fn rewrites_absolute_system_paths_for_destdir_installs() {
        let input =
            "PREFIX=/system\ninstall -Dm755 foo /system/binaries/foo\nln -s /system/bin foo";
        let rewritten = rewrite_absolute_system_paths(input);

        assert!(rewritten.contains("PREFIX=/system"));
        assert!(rewritten.contains("install -Dm755 foo $DESTDIR/system/binaries/foo"));
        assert!(rewritten.contains("ln -s $DESTDIR/system/bin foo"));
    }

    #[test]
    fn rewrite_absolute_system_paths_keeps_configure_path_options() {
        let input = "./configure --prefix=/system \\\n    --bindir=/system/binaries \\\n    --mandir=/system/documentation/man-pages";
        let rewritten = rewrite_absolute_system_paths(input);

        assert!(rewritten.contains("--prefix=/system"));
        assert!(rewritten.contains("--bindir=/system/binaries"));
        assert!(rewritten.contains("--mandir=/system/documentation/man-pages"));
        assert!(!rewritten.contains("--prefix=$DESTDIR/system"));
        assert!(!rewritten.contains("--bindir=$DESTDIR/system/binaries"));
    }

    #[test]
    fn rewrite_absolute_system_paths_does_not_double_prefix_quoted_destdir() {
        let input = "\"$DESTDIR/system/systembinaries/pwconv\" -R /";
        let rewritten = rewrite_absolute_system_paths(input);

        assert_eq!(rewritten, input);
    }

    #[test]
    fn rewrite_absolute_system_paths_does_not_rewrite_systembinaries_segment() {
        let input = "/system/systembinaries/pwconv";
        let rewritten = rewrite_absolute_system_paths(input);

        assert_eq!(rewritten, "$DESTDIR/system/systembinaries/pwconv");
    }

    #[test]
    fn generated_oksh_keeps_prefixes_and_uses_resolved_cross_tools() {
        let package = BookPackage {
            chapter: 6,
            section: "6.14".to_string(),
            title: "oksh 7.8".to_string(),
            name: "oksh".to_string(),
            version: "7.8".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/oksh.html".to_string(),
            recipe_id: "6-14-oksh".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["oksh-7.8.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/oksh-7.8.tar.gz".to_string()],
            extract_dir: Some("oksh-7.8".to_string()),
            commands: vec![
                r#"./configure \
    --no-thanks \
    --disable-curses \
    --prefix=/system \
    --bindir=/system/binaries \
    --mandir=/system/documentation/man-pages \
    --cc="$LBI_ROOT/system/tools/bin/$LBI_TARGET-clang" \
    --cflags="--target=$LBI_TARGET --sysroot=$LBI_ROOT $LWI_CFLAGS""#
                    .to_string(),
                r#"make $LWI_MAKE_FLAGS \
    LDFLAGS="--target=$LBI_TARGET --sysroot=$LBI_ROOT $LBI_CUSTOM_LDFLAGS""#
                    .to_string(),
                r#"make install DESTDIR="$LBI_ROOT"
ln -sf oksh "$LBI_ROOT/system/binaries/ksh""#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "Public domain".to_string(),
            description: "oksh shell".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-lbi-linux-musl", "x86_64");

        assert!(build_script.contains("--prefix=/system"));
        assert!(build_script.contains("--bindir=/system/binaries"));
        assert!(build_script.contains("--mandir=/system/documentation/man-pages"));
        assert!(!build_script.contains("--prefix=$DESTDIR/system"));
        assert!(!build_script.contains("--bindir=$DESTDIR/system/binaries"));
        assert!(build_script.contains(r#"--cc="$CC""#));
        assert!(build_script.contains("lbi_find_cross_tool ar llvm-ar"));
        assert!(build_script.contains("export PATH=\"$LBI_SYSROOT/system/tools/bin:$PATH\""));
        assert!(
            !build_script
                .contains("$LBI_SYSROOT/system/tools/bin\" \"$LBI_SYSROOT/system/binaries")
        );
        assert!(build_script.contains("ln -sf oksh \"$LBI_ROOT/system/binaries/ksh\""));
    }

    #[test]
    fn generated_om4_parser_fix_preserves_c_include_headers() {
        let package = BookPackage {
            chapter: 6,
            section: "6.2".to_string(),
            title: "om4 6.7".to_string(),
            name: "om4".to_string(),
            version: "6.7".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/om4.html".to_string(),
            recipe_id: "6-2-om4".to_string(),
        };
        let html = r#"
            <p class="lead">Build om4.</p>
            <p><strong>Source package:</strong> <code>om4-6.7.tar.gz</code></p>
            <p><strong>Source URLs:</strong> <code>https://example.invalid/om4-6.7.tar.gz</code></p>
            <h2>Extract and Enter the Source Tree</h2>
            <pre><code>cd "$LBI_SOURCES"
tar -xf om4-6.7.tar.gz
cd om4-6.7</code></pre>
            <h2>Apply the Parser Compatibility Fix</h2>
            <pre><code>grep -q '^#include &lt;stdlib.h&gt;$' parser.y || \
    sed -i '/^#include &lt;stdint.h&gt;$/a #include &lt;stdlib.h&gt;' parser.y</code></pre>
            <h2>Build and Install om4</h2>
            <pre><code>make -j1 CC="$LBI_ROOT/system/tools/bin/$LBI_TARGET-clang"
make CC="$LBI_ROOT/system/tools/bin/$LBI_TARGET-clang" install DESTDIR="$LBI_ROOT"</code></pre>
            <h2>Licenses:</h2><ul><li>BSD-3-Clause</li></ul>
        "#;

        let recipe = parse_page_recipe(html, &package).unwrap();
        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("grep -q '^#include <stdlib.h>$' parser.y"));
        assert!(
            build_script.contains("sed -i '/^#include <stdint.h>$/a #include <stdlib.h>' parser.y")
        );
        assert!(!build_script.contains("grep -q '^#include $' parser.y"));
    }

    #[test]
    fn rewrite_absolute_system_paths_keeps_cmake_define_values() {
        let input =
            "cmake -DLIBCXX_INSTALL_LIBRARY_DIR=/system/libraries -DCMAKE_INSTALL_PREFIX=/system";
        let rewritten = rewrite_absolute_system_paths(input);

        assert!(rewritten.contains("-DLIBCXX_INSTALL_LIBRARY_DIR=/system/libraries"));
        assert!(rewritten.contains("-DCMAKE_INSTALL_PREFIX=/system"));
        assert!(!rewritten.contains("$DESTDIR/system/libraries"));
    }

    #[test]
    fn chapter7_chroot_compiler_search_paths_stay_runtime_absolute() {
        let package = BookPackage {
            chapter: 7,
            section: "7.7".to_string(),
            title: "gettext-tiny 0.3.3".to_string(),
            name: "gettext-tiny".to_string(),
            version: "0.3.3".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter7/gettext-tiny.html".to_string(),
            recipe_id: "7-7-gettext-tiny".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["gettext-tiny-0.3.3.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/gettext-tiny-0.3.3.tar.xz".to_string()],
            extract_dir: Some("gettext-tiny-0.3.3".to_string()),
            commands: vec![
                r#"make $LWI_MAKE_FLAGS \
    LIBINTL=musl \
    CPPFLAGS="-I/system/headers" \
    CFLAGS="-I/system/headers" \
    LDFLAGS="-L/system/libraries" \
    CC="cc -B/system/libraries -B/system/libraries/clang/22/lib/linux""#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "gettext-tiny".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-lbi-linux-musl", "x86_64");

        assert!(
            build_script
                .contains("CC=\"cc -B/system/libraries -B/system/libraries/clang/22/lib/linux\"")
        );
        assert!(build_script.contains("LIBINTL=MUSL"));
        assert!(!build_script.contains("LIBINTL=musl"));
        assert!(!build_script.contains("-B$DESTDIR/system/libraries"));
    }

    #[test]
    fn rustc_bootstrap_toml_uses_live_chroot_tool_paths() {
        let package = BookPackage {
            chapter: 8,
            section: "8.40".to_string(),
            title: "rustc 1.95.0".to_string(),
            name: "rustc".to_string(),
            version: "1.95.0".to_string(),
            layer: DEVEL_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/rustc.html".to_string(),
            recipe_id: "8-40-rustc".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["rustc-1.95.0-src.tar.xz".to_string()],
            source_urls: vec![
                "https://static.rust-lang.org/dist/rustc-1.95.0-src.tar.xz".to_string(),
            ],
            extract_dir: Some("rustc-1.95.0-src".to_string()),
            commands: vec![
                r#"cat > bootstrap.toml <<EOF
[install]
prefix = "/system"

[rust]
default-linker = "/system/binaries/cc"

[target.x86_64-unknown-linux-musl]
cc = "/system/binaries/cc"
cxx = "/system/binaries/c++"
linker = "/system/binaries/cc"
ar = "/system/binaries/llvm-ar"
ranlib = "/system/binaries/llvm-ranlib"
llvm-config = "/system/binaries/llvm-config"
EOF"#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "Apache-2.0".to_string(),
            description: "Rust compiler".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-lbi-linux-musl", "x86_64");

        assert!(build_script.contains("prefix = \"/system\""));
        assert!(build_script.contains("default-linker = \"/system/binaries/cc\""));
        assert!(build_script.contains("cc = \"/system/binaries/cc\""));
        assert!(build_script.contains("cxx = \"/system/binaries/c++\""));
        assert!(build_script.contains("ar = \"/system/binaries/llvm-ar\""));
        assert!(build_script.contains("ranlib = \"/system/binaries/llvm-ranlib\""));
        assert!(build_script.contains("llvm-config = \"/system/binaries/llvm-config\""));
        assert!(!build_script.contains("/destdir/system/binaries"));
        assert!(!build_script.contains("$DESTDIR/system/binaries"));
    }

    #[test]
    fn shadow_post_install_uses_staged_account_tools() {
        let package = BookPackage {
            chapter: 8,
            section: "8.10".to_string(),
            title: "Shadow 4.19.4".to_string(),
            name: "shadow".to_string(),
            version: "4.19.4".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/shadow.html".to_string(),
            recipe_id: "8-10-shadow".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["shadow-4.19.4.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/shadow-4.19.4.tar.xz".to_string()],
            extract_dir: Some("shadow-4.19.4".to_string()),
            commands: vec![
                "make install\npwconv\ngrpconv\nmkdir -p /etc/default\nuseradd -D --gid 999\npasswd root"
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "BSD-3-Clause".to_string(),
            description: "shadow".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-lbi-linux-musl", "x86_64");

        assert!(!build_script.contains("\npwconv\n"));
        assert!(!build_script.contains("\ngrpconv\n"));
        assert!(build_script.contains("\"$DESTDIR/system/systembinaries/pwconv\" -R /"));
        assert!(build_script.contains("grep -q '^[^:]*:[^:]*:999:' /etc/group"));
        assert!(build_script.contains("printf '%s\\n' 'users:x:999:' >> /etc/group"));
        assert!(build_script.contains("\"$DESTDIR/system/systembinaries/grpconv\" -R /"));
        assert!(
            build_script.contains("\"$DESTDIR/system/systembinaries/useradd\" -D -R / --gid 999")
        );
        assert!(build_script.contains("\"$DESTDIR/system/binaries/passwd\" -R / -d root"));
        assert!(!build_script.contains("passwd\" -R / root"));
    }

    #[test]
    fn generated_cross_runtime_commands_use_sysroot_toolchain() {
        let package = BookPackage {
            chapter: 5,
            section: "5.5".to_string(),
            title: "LLVM runtimes (libunwind, libcxxabi, libcxx) 22.1.3".to_string(),
            name: "llvm-runtimes".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/llvm-runtimes.html".to_string(),
            recipe_id: "5-5-llvm-runtimes".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["llvm-project-22.1.3.src.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/llvm-project-22.1.3.src.tar.xz".to_string()],
            extract_dir: Some("llvm-project-22.1.3.src".to_string()),
            commands: vec![
                r#"lbi_cmake build-runtimes \
    -DCMAKE_C_COMPILER="$LBI_ROOT/system/tools/bin/$LBI_TARGET-clang" \
    -DCMAKE_CXX_COMPILER="$LBI_ROOT/system/tools/bin/$LBI_TARGET-clang++" \
    -DCMAKE_SYSROOT="$LBI_ROOT" \
    -DCMAKE_FIND_ROOT_PATH="$LBI_ROOT;$LBI_ROOT/system" \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DLLVM_ENABLE_RUNTIMES="libunwind;libcxxabi;libcxx""#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "Apache-2.0".to_string(),
            description: "LLVM runtimes".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("-DCMAKE_C_COMPILER=\"$CC\""));
        assert!(build_script.contains("-DCMAKE_CXX_COMPILER=\"$CXX\""));
        assert!(build_script.contains("-DCMAKE_SYSROOT=\"$LBI_SYSROOT\""));
        assert!(
            build_script.contains("-DCMAKE_FIND_ROOT_PATH=\"$LBI_SYSROOT;$LBI_SYSROOT/system\"")
        );
        assert!(build_script.contains("-DCMAKE_SHARED_LINKER_FLAGS=\"-nostartfiles\""));
        assert!(build_script.contains("-DCMAKE_MODULE_LINKER_FLAGS=\"-nostartfiles\""));
        assert!(!build_script.contains("destdir/llvm-runtimes/system/tools"));
    }

    #[test]
    fn llvm_clang_pass2_generated_script_uses_ccache() {
        let package = BookPackage {
            chapter: 5,
            section: "5.6".to_string(),
            title: "llvm/clang pass 2 22.1.3".to_string(),
            name: "llvm-clang-pass2".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/llvm-clang-pass2.html".to_string(),
            recipe_id: "5-6-llvm-clang-pass2".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["llvm-project-22.1.3.src.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/llvm-project-22.1.3.src.tar.xz".to_string()],
            extract_dir: Some("llvm-project-22.1.3.src".to_string()),
            commands: vec![
                r#"cmake -G Ninja "../llvm" \
    -DCMAKE_INSTALL_PREFIX=$LBI_ROOT/system/tools \
    -DLLVM_NATIVE_TOOL_DIR="$LBI_ROOT/system/tools/bin" \
    -DLLVM_ENABLE_RUNTIMES="compiler-rt""#
                    .to_string(),
                "ninja".to_string(),
            ],
            dependencies: Vec::new(),
            license: "Apache-2.0".to_string(),
            description: "LLVM pass2".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("export LBI_CCACHE"));
        assert!(build_script.contains(
            "cmake -G Ninja \"../llvm\" \\\n    -DCMAKE_C_COMPILER_LAUNCHER=\"$LBI_CCACHE\" \\"
        ));
        assert!(build_script.contains("-DCMAKE_CXX_COMPILER_LAUNCHER=\"$LBI_CCACHE\""));
        assert!(build_script.contains("-DCMAKE_ASM_COMPILER_LAUNCHER=\"$LBI_CCACHE\""));
        assert!(!build_script.contains("-DLLVM_CCACHE_BUILD=ON"));
        assert!(build_script.contains("-DLLVM_NATIVE_TOOL_DIR=\"$LBI_SYSROOT/system/tools/bin\""));
        assert!(
            build_script
                .contains("-DLLVM_CONFIG_PATH=\"$LBI_SYSROOT/system/tools/bin/llvm-config\"")
        );
        assert!(build_script.contains("-DLLVM_ENABLE_RUNTIMES=\"compiler-rt\""));
    }

    #[test]
    fn llvm_clang_pass2_generated_script_fixes_clang_resource_layout() {
        let package = BookPackage {
            chapter: 6,
            section: "6.22".to_string(),
            title: "llvm/clang pass 2 22.1.3".to_string(),
            name: "llvm-clang-pass2".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/llvm-clang-pass2.html".to_string(),
            recipe_id: "6-22-llvm-clang-pass2".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["llvm-project-22.1.3.src.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/llvm-project-22.1.3.src.tar.xz".to_string()],
            extract_dir: Some("llvm-project-22.1.3.src".to_string()),
            commands: vec![
                r#"DESTDIR="$LBI_ROOT" cmake --install build-compiler-rt-pass2

mkdir -p "$LBI_ROOT/system/lib/clang/22/lib/$LBI_TARGET"

if [ -f "$LBI_ROOT/system/tools/lib/clang/22/lib/$LBI_TARGET/libclang_rt.builtins.a" ]; then
    ln -sf "/system/tools/lib/clang/22/lib/$LBI_TARGET/libclang_rt.builtins.a" \
        "$LBI_ROOT/system/lib/clang/22/lib/$LBI_TARGET/libclang_rt.builtins.a"
fi

CRTBEGIN_OBJ=$(find "$LBI_ROOT/system/libraries/clang" \
    -type f \( -name 'crtbeginS.o' -o -name 'clang_rt.crtbegin*.o' \) | head -n1)
CRTEND_OBJ=$(find "$LBI_ROOT/system/libraries/clang" \
    -type f \( -name 'crtendS.o' -o -name 'clang_rt.crtend*.o' \) | head -n1)

CRT_DIR=$(dirname "$CRTBEGIN_OBJ")

if [ -n "$CRTBEGIN_OBJ" ] && [ -n "$CRTEND_OBJ" ]; then
    ln -sf "$(basename "$CRTBEGIN_OBJ")" "$CRT_DIR/crtbeginS.o"
    ln -sf "$(basename "$CRTEND_OBJ")" "$CRT_DIR/crtendS.o"

    ln -sf "${CRTBEGIN_OBJ#$LBI_ROOT/system}" "$LBI_ROOT/system/libraries/crtbeginS.o"
    ln -sf "${CRTEND_OBJ#$LBI_ROOT/system}" "$LBI_ROOT/system/libraries/crtendS.o"
fi"#
                .to_string(),
            ],
            dependencies: Vec::new(),
            license: "Apache-2.0".to_string(),
            description: "LLVM pass2".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-lbi-linux-musl", "x86_64");

        assert!(
            build_script.contains("builtins_name=\"libclang_rt.builtins-${compiler_rt_arch}.a\"")
        );
        assert!(build_script.contains("\"$LBI_ROOT/system/libraries/clang/22/lib/$LBI_TARGET\""));
        assert!(build_script.contains("ln -sf \"../linux/$builtins_name\""));
        assert!(build_script.contains("\"$LBI_ROOT/system/lib/clang\""));
        assert!(build_script.contains("rm -rf \"$LBI_ROOT/system/lib/clang/22\""));
        assert!(!build_script.contains("\"$LBI_ROOT/system/lib/clang/22/lib/linux\""));
        assert!(!build_script.contains(
            "cp -R \"$LBI_ROOT/system/libraries/clang/22/.\" \"$LBI_ROOT/system/lib/clang/22/\""
        ));
        assert!(build_script.contains("2>/dev/null || true; } | head -n1"));
        assert!(build_script.contains(
            "install -m644 \"$CRTBEGIN_OBJ\" \"$LBI_ROOT/system/libraries/crtbeginS.o\""
        ));
        assert!(
            !build_script.contains(
                "$DESTDIR/system/tools/lib/clang/22/lib/$LBI_TARGET/libclang_rt.builtins.a"
            )
        );
        assert!(
            !build_script
                .contains("CRT_DIR=$(dirname \"$CRTBEGIN_OBJ\")\n\nif [ -n \"$CRTBEGIN_OBJ\"")
        );
    }

    #[test]
    fn llvm_clang_pass2_driver_configs_keep_runtime_paths() {
        let package = BookPackage {
            chapter: 6,
            section: "6.22".to_string(),
            title: "llvm/clang pass 2 22.1.3".to_string(),
            name: "llvm-clang-pass2".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/llvm-clang-pass2.html".to_string(),
            recipe_id: "6-22-llvm-clang-pass2".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["llvm-project-22.1.3.src.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/llvm-project-22.1.3.src.tar.xz".to_string()],
            extract_dir: Some("llvm-project-22.1.3.src".to_string()),
            commands: vec![
                r#"cat > "$LBI_ROOT/system/configuration/clang/clang++.cfg" <<EOF
--target=$LBI_TARGET
-nostdinc++
-I/system/headers/c++/v1
-isystem /system/libraries/clang/22/include
-B/system/libraries
-B/system/libraries/clang/22/lib/linux
-L/system/libraries
-Wno-unused-command-line-argument
\$-Wl,-rpath,/system/libraries
\$-lc++
\$-lc++abi
\$-lunwind
EOF"#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "Apache-2.0".to_string(),
            description: "LLVM pass2".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-lbi-linux-musl", "x86_64");

        assert!(build_script.contains("-I/system/headers/c++/v1"));
        assert!(build_script.contains("-isystem /system/libraries/clang/22/include"));
        assert!(build_script.contains("-B/system/libraries"));
        assert!(build_script.contains("-B/system/libraries/clang/22/lib/linux"));
        assert!(build_script.contains("-L/system/libraries"));
        assert!(build_script.contains("\\$-Wl,-rpath,/system/libraries"));
        assert!(!build_script.contains("$DESTDIR/system/headers/c++/v1"));
        assert!(!build_script.contains("$DESTDIR/system/libraries"));
    }

    #[test]
    fn llvm_runtimes_generates_compiler_rt_crt_for_chapter6_links() {
        let package = BookPackage {
            chapter: 5,
            section: "5.5".to_string(),
            title: "LLVM runtimes (libunwind, libcxxabi, libcxx) 22.1.3".to_string(),
            name: "llvm-runtimes".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/llvm-runtimes.html".to_string(),
            recipe_id: "5-5-llvm-runtimes".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["llvm-project-22.1.3.src.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/llvm-project-22.1.3.src.tar.xz".to_string()],
            extract_dir: Some("llvm-project-22.1.3.src".to_string()),
            commands: vec![
                "cd runtimes".to_string(),
                r#"DESTDIR="$LBI_ROOT" ninja -C build-runtimes install"#.to_string(),
            ],
            dependencies: Vec::new(),
            license: "Apache-2.0".to_string(),
            description: "LLVM runtimes".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("lbi_cmake build-compiler-rt-crt"));
        assert!(build_script.contains("-DCOMPILER_RT_BUILD_CRT=ON"));
        assert!(build_script.contains("-DCOMPILER_RT_BUILD_BUILTINS=ON"));
        assert!(!build_script.contains("cmake --install build-compiler-rt-crt"));
        assert!(
            build_script
                .contains("install -m644 \"$crtbegin_obj\" \"$crt_resource_dir/crtbeginS.o\"")
        );
        assert!(
            build_script
                .contains("install -m644 \"$crtend_obj\" \"$LBI_ROOT/system/libraries/crtendS.o\"")
        );
    }

    #[test]
    fn chapter6_ncurses_host_tic_uses_build_toolchain() {
        let package = BookPackage {
            chapter: 6,
            section: "6.3".to_string(),
            title: "ncurses 6.6-20260418".to_string(),
            name: "ncurses".to_string(),
            version: "6.6-20260418".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/ncurses.html".to_string(),
            recipe_id: "6-3-ncurses".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["ncurses-6.6-20260418.tgz".to_string()],
            source_urls: vec!["https://example.invalid/ncurses-6.6-20260418.tgz".to_string()],
            extract_dir: Some("ncurses-6.6-20260418".to_string()),
            commands: vec![
                r#"mkdir -pv build
pushd build
  ../configure --prefix="$LBI_ROOT/system/tools" AWK=gawk
  make -C include
  make -C progs tic
  install -vm755 progs/tic "$LBI_ROOT/system/tools/bin/tic"
popd"#
                    .to_string(),
                r#"lbi_configure \
    --host="$LBI_TARGET" \
    --build="$(./config.guess)" \
    --with-shared \
    AWK=gawk

make $LWI_MAKE_FLAGS
make DESTDIR="$LBI_ROOT" TIC_PATH="$PWD/build/progs/tic" install"#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "X11-style".to_string(),
            description: "ncurses".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("export CC=\"${CC:-$(lbi_find_cross_tool clang)}\""));
        assert!(build_script.contains("rm -rf build obj obj_s obj_g obj_x lib"));
        assert!(build_script.contains("CC=\"${BUILD_CC:-cc}\" \\"));
        assert!(
            build_script.contains("../configure --prefix=\"$LBI_SYSROOT/system/tools\" AWK=gawk")
        );
        assert!(build_script.contains("make ${LWI_MAKE_FLAGS:-} -C progs tic"));
        assert!(
            build_script.contains("install -vm755 progs/tic \"$LBI_SYSROOT/system/tools/bin/tic\"")
        );
        assert!(build_script.contains(")\nrm -rf obj obj_s obj_g obj_x lib"));
        assert!(build_script.contains("--host=\"$LBI_TARGET\""));
        assert!(build_script.contains("--enable-root-access"));
        assert!(
            build_script.contains("export CPPFLAGS=\"${CPPFLAGS:+$CPPFLAGS }-DUSE_ROOT_ACCESS\"")
        );
        assert!(build_script.contains("TIC_PATH=\"$PWD/build/progs/tic\" install"));
        assert!(
            !build_script.contains("../configure --prefix=\"$LBI_ROOT/system/tools\" AWK=gawk")
        );
    }

    #[test]
    fn chapter8_ncurses_enables_root_access_for_program_links() {
        let package = BookPackage {
            chapter: 8,
            section: "8.11".to_string(),
            title: "ncurses stage 2 6.6-20260418".to_string(),
            name: "ncurses".to_string(),
            version: "6.6-20260418".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/ncurses-stage2.html".to_string(),
            recipe_id: "8-11-ncurses".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["ncurses-6.6-20260418.tgz".to_string()],
            source_urls: vec!["https://example.invalid/ncurses-6.6-20260418.tgz".to_string()],
            extract_dir: Some("ncurses-6.6-20260418".to_string()),
            commands: vec![
                r#"lbi_configure \
    --with-manpage-format=normal \
    --with-shared \
    --without-normal \
    --without-cxx-binding \
    --without-debug \
    --without-ada \
    --enable-widec \
    --disable-stripping \
    --enable-pc-files \
    --with-pkg-config-libdir=/system/libraries/pkgconfig \
    AWK=awk

make $LWI_MAKE_FLAGS"#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "X11-style".to_string(),
            description: "ncurses".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("--with-shared \\\n    --enable-root-access \\"));
        assert!(
            build_script.contains("export CPPFLAGS=\"${CPPFLAGS:+$CPPFLAGS }-DUSE_ROOT_ACCESS\"")
        );
        assert_eq!(build_script.matches("--enable-root-access").count(), 1);
        assert_eq!(build_script.matches("-DUSE_ROOT_ACCESS").count(), 1);
    }

    #[test]
    fn chapter8_sqlite_uses_only_supported_autosetup_directory_options() {
        let package = BookPackage {
            chapter: 8,
            section: "8.16".to_string(),
            title: "SQLite 3.53.0".to_string(),
            name: "sqlite".to_string(),
            version: "3.53.0".to_string(),
            layer: DEVEL_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/sqlite.html".to_string(),
            recipe_id: "8-16-sqlite".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["sqlite-autoconf-3530000.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/sqlite-autoconf-3530000.tar.gz".to_string()],
            extract_dir: Some("sqlite-autoconf-3530000".to_string()),
            commands: vec![
                r#"CPPFLAGS="-D SQLITE_ENABLE_COLUMN_METADATA=1 \
          -D SQLITE_ENABLE_UNLOCK_NOTIFY=1 \
          -D SQLITE_ENABLE_DBSTAT_VTAB=1 \
          -D SQLITE_SECURE_DELETE=1" \
lbi_configure \
    --disable-static \
    --enable-shared \
    --disable-readline \
    --soname=legacy"#
                    .to_string(),
                "make $LWI_MAKE_FLAGS".to_string(),
                "make install".to_string(),
            ],
            dependencies: Vec::new(),
            license: "blessing".to_string(),
            description: "SQLite".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains(
            "CPPFLAGS=\"-D SQLITE_ENABLE_COLUMN_METADATA=1 \\\n          -D SQLITE_ENABLE_UNLOCK_NOTIFY=1 \\\n          -D SQLITE_ENABLE_DBSTAT_VTAB=1 \\\n          -D SQLITE_SECURE_DELETE=1\" \\\n./configure \\\n    --prefix=/system \\\n    --bindir=/system/binaries \\\n    --libdir=/system/libraries \\\n    --includedir=/system/headers \\\n    --mandir=/system/documentation/man-pages \\"
        ));
        assert!(!build_script.contains("SQLITE_SECURE_DELETE=1\" \\\nlbi_configure \\"));
    }

    #[test]
    fn chapter8_bmake_install_clears_inherited_destdir() {
        let package = BookPackage {
            chapter: 8,
            section: "8.20".to_string(),
            title: "bmake 20260406".to_string(),
            name: "bmake".to_string(),
            version: "20260406".to_string(),
            layer: DEVEL_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/bmake.html".to_string(),
            recipe_id: "8-20-bmake".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["bmake-20260406.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/bmake-20260406.tar.gz".to_string()],
            extract_dir: Some("bmake".to_string()),
            commands: vec![
                r#"lbi_configure \
    --with-default-sys-path=/system/share/mk \
    --with-mksrc=mk \
    --with-filemon=no \
    --without-lua"#
                    .to_string(),
                "make $LWI_MAKE_FLAGS".to_string(),
                r#"MAKESYSPATH=mk \
./bmake -f Makefile install \
    prefix=/system \
    BINDIR.bmake=/system/binaries \
    SHAREDIR.bmake=/system/share \
    MANDIR.bmake=/system/documentation/man-pages \
    STRIP_FLAG="#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "BSD-2-Clause".to_string(),
            description: "bmake".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(
            build_script.contains("DESTDIR= \\\nMAKESYSPATH=mk \\\n./bmake -f Makefile install \\")
        );
        assert!(build_script.contains("--with-default-sys-path=/system/share/mk \\"));
        assert!(!build_script.contains("--with-default-sys-path=$DESTDIR/system/share/mk \\"));
        assert!(build_script.contains("BINDIR.bmake=$DESTDIR/system/binaries \\"));
        assert!(build_script.contains("SHAREDIR.bmake=$DESTDIR/system/share \\"));
        assert!(build_script.contains("MANDIR.bmake=$DESTDIR/system/documentation/man-pages \\"));
        assert_eq!(
            build_script
                .matches("DESTDIR= \\\nMAKESYSPATH=mk \\")
                .count(),
            1
        );
    }

    #[test]
    fn ubase_install_moves_system_bin_payload_to_system_binaries() {
        let package = BookPackage {
            chapter: 8,
            section: "8.21".to_string(),
            title: "ubase 0.1".to_string(),
            name: "ubase".to_string(),
            version: "0.1".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/ubase.html".to_string(),
            recipe_id: "8-21-ubase".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["ubase-0.1.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/ubase-0.1.tar.gz".to_string()],
            extract_dir: Some("ubase-0.1".to_string()),
            commands: vec![
                "make $LWI_MAKE_FLAGS".to_string(),
                "make PREFIX=/system install".to_string(),
            ],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "ubase".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("make ${LWI_MAKE_FLAGS:-} PREFIX=/system install"));
        assert!(build_script.contains("if [ -d \"$DESTDIR/system/bin\" ]; then"));
        assert!(build_script.contains("mkdir -p \"$DESTDIR/system/binaries\""));
        assert!(build_script.contains("mv \"$path\" \"$DESTDIR/system/binaries/\""));
        assert_eq!(
            build_script
                .matches("if [ -d \"$DESTDIR/system/bin\" ]; then")
                .count(),
            1
        );
    }

    #[test]
    fn chapter8_mandoc_postinstall_uses_staged_man_binary() {
        let package = BookPackage {
            chapter: 8,
            section: "8.24".to_string(),
            title: "mandoc 1.14.6".to_string(),
            name: "mandoc".to_string(),
            version: "1.14.6".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/mandoc.html".to_string(),
            recipe_id: "8-24-mandoc".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["mandoc-1.14.6.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/mandoc-1.14.6.tar.gz".to_string()],
            extract_dir: Some("mandoc-1.14.6".to_string()),
            commands: vec![
                "make $LWI_MAKE_FLAGS".to_string(),
                "make ${LWI_MAKE_FLAGS:-} install\n/system/systembinaries/makewhatis /system/documentation/man-pages\nMANPAGER=cat man mandoc >/dev/null".to_string(),
            ],
            dependencies: Vec::new(),
            license: "ISC".to_string(),
            description: "mandoc".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(!build_script.contains("MANPAGER=cat man mandoc >/dev/null"));
        assert!(build_script.contains(
            "MANPATH=\"$DESTDIR/system/documentation/man-pages\" MANPAGER=cat \"$DESTDIR/system/binaries/man\" mandoc >/dev/null"
        ));
        assert!(build_script.contains(
            "$DESTDIR/system/systembinaries/makewhatis $DESTDIR/system/documentation/man-pages"
        ));
    }

    #[test]
    fn chapter6_file_uses_host_magic_compiler() {
        let package = BookPackage {
            chapter: 6,
            section: "6.9".to_string(),
            title: "File 5.47".to_string(),
            name: "file".to_string(),
            version: "5.47".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/file.html".to_string(),
            recipe_id: "6-9-file".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["file-5.47.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/file-5.47.tar.gz".to_string()],
            extract_dir: Some("file-5.47".to_string()),
            commands: vec![
                r#"CC="$LBI_ROOT/system/tools/bin/$LBI_TARGET-clang" \
lbi_configure \
    --host="$LBI_TARGET""#
                    .to_string(),
                "make $LWI_MAKE_FLAGS".to_string(),
                r#"make install DESTDIR="$LBI_ROOT""#.to_string(),
            ],
            dependencies: Vec::new(),
            license: "BSD-2-Clause".to_string(),
            description: "file".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("rm -rf build-host-file"));
        assert!(build_script.contains("CC=\"${BUILD_CC:-cc}\" \\"));
        assert!(build_script.contains(
            "../configure --prefix=\"$PWD/host-tools\" --disable-shared --enable-static --disable-libseccomp"
        ));
        assert!(build_script.contains("make ${LWI_MAKE_FLAGS:-} -C src file"));
        assert!(
            build_script
                .contains("make $LWI_MAKE_FLAGS FILE_COMPILE=\"$PWD/build-host-file/src/file\"")
        );
        assert!(build_script.contains("CC=\"$CC\""));
        assert!(build_script.contains("--host=\"$LBI_TARGET\""));
    }

    #[test]
    fn source_manifest_uses_output_names() {
        let manifest = parse_source_manifest(
            r#"
                https://example.invalid/upstream.tar.gz renamed.tar.gz
                git+https://example.invalid/project.git#deadbeef project-git.tar.gz
            "#,
        );

        assert_eq!(manifest[0].output_name, "renamed.tar.gz");
        assert_eq!(manifest[0].url, "https://example.invalid/upstream.tar.gz");
        assert_eq!(manifest[1].output_name, "project-git.tar.gz");
        assert_eq!(
            manifest[1].url,
            "https://example.invalid/project.git#deadbeef"
        );
    }

    #[test]
    fn source_url_matches_manifest_url_filename_for_renamed_generic_archive() {
        let package = BookPackage {
            chapter: 8,
            section: "8.26".to_string(),
            title: "BSD-Diffutils stage 2 0.99.0".to_string(),
            name: "bsddiffutils".to_string(),
            version: "0.99.0".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://www.vertexlinux.net/lbi/chapters/chapter8/bsddiffutils-stage2.html"
                .to_string(),
            recipe_id: "8-26-bsddiffutils".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["bsddiff-34e64c08674b.tar.gz".to_string()],
            source_urls: Vec::new(),
            extract_dir: None,
            commands: vec!["make".to_string()],
            dependencies: Vec::new(),
            license: "BSD-2-Clause".to_string(),
            description: "BSD diff utilities".to_string(),
        };
        let manifest = parse_source_manifest(
            r#"
                https://github.com/other/project/archive/refs/heads/main.zip other-main.zip
                git+https://github.com/chimera-linux/bsddiff.git#34e64c08674ba6f96da41075a17be60944e61e33 bsddiff-34e64c08674b.tar.gz
            "#,
        );

        let url = source_url_for_input("bsddiff-34e64c08674b.tar.gz", &package, &recipe, &manifest)
            .unwrap();

        assert_eq!(
            url,
            "https://github.com/chimera-linux/bsddiff.git#34e64c08674ba6f96da41075a17be60944e61e33"
        );
    }

    #[test]
    fn page_recipe_strips_book_unzip_scaffolding() {
        let package = BookPackage {
            chapter: 6,
            section: "6.8".to_string(),
            title: "BSD-Diffutils 0.99.0".to_string(),
            name: "bsddiffutils".to_string(),
            version: "0.99.0".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/bsddiffutils.html".to_string(),
            recipe_id: "6-8-bsddiffutils".to_string(),
        };
        let html = r#"
            <p class="lead">BSD diff utilities.</p>
            <p><strong>Input assumption:</strong> <code>bsddiff-34e64c08674b.tar.gz</code></p>
            <h2>Extract and Enter the Source Tree</h2>
            <pre><code>cd "$LBI_SOURCES"
tar -xf bsddiff-34e64c08674b.tar.gz
cd bsddiff-34e64c08674b</code></pre>
            <h2>Build BSD-Diffutils</h2>
            <pre><code>meson compile -C build -j "$(nproc)"</code></pre>
            <h2>Licenses:</h2><ul><li>BSD-2-Clause</li></ul>
        "#;

        let recipe = parse_page_recipe(html, &package).unwrap();

        assert_eq!(recipe.extract_dir.as_deref(), Some("bsddiff-34e64c08674b"));
        assert!(
            !recipe
                .commands
                .iter()
                .any(|cmd| cmd.contains("bsddiff-34e64c08674b.tar.gz"))
        );
        assert!(
            !recipe
                .commands
                .iter()
                .any(|cmd| cmd.trim() == "cd bsddiff-34e64c08674b")
        );
        assert!(
            recipe
                .commands
                .iter()
                .any(|cmd| cmd.trim() == "meson compile -C build -j \"$(nproc)\"")
        );
    }

    #[test]
    fn generated_bsddiffutils_supports_meson_helpers_and_compatibility_edits() {
        let package = BookPackage {
            chapter: 8,
            section: "8.26".to_string(),
            title: "BSD-Diffutils stage 2 0.99.0".to_string(),
            name: "bsddiffutils".to_string(),
            version: "0.99.0".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/bsddiffutils-stage2.html".to_string(),
            recipe_id: "8-26-bsddiffutils".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["bsddiff-34e64c08674b.tar.gz".to_string()],
            source_urls: vec![
                "git+https://github.com/chimera-linux/bsddiff.git#34e64c08674ba6f96da41075a17be60944e61e33"
                    .to_string(),
            ],
            extract_dir: Some("bsddiff-34e64c08674b".to_string()),
            commands: vec![
                r#"sed -i \
    "s|'strtonum.c', 'warnc.c', 'xmalloc.c'|'strtonum.c', 'warnc.c', 'xmalloc.c', 'fgetln.c'|" \
    compat/meson.build

sed -i \
    '/#include <limits.h>/a char *fgetln(FILE *, size_t *);' \
    diff/diff.c

sed -i \
    '/#include <unistd.h>/a char *fgetln(FILE *, size_t *);' \
    diff3/diff3prog.c

sed -i \
    '/include_directories: \[sysdefs\],/a \    link_with: [libcompat],' \
    diff3/meson.build"#
                    .to_string(),
                "lbi_meson build -Dbuildtype=release".to_string(),
                "meson compile -C build -j \"$(nproc)\"".to_string(),
            ],
            dependencies: vec!["meson".to_string()],
            license: "BSD-2-Clause".to_string(),
            description: "BSD diff utilities".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("lbi_meson()"));
        assert!(build_script.contains("--libexecdir=/system/systembinaries"));
        assert!(build_script.contains("'fgetln.c'"));
        assert!(build_script.contains("link_with: [libcompat]"));
        assert!(build_script.contains("diff/diff.c > diff/diff.c.new"));
        assert!(build_script.contains("diff3/diff3prog.c > diff3/diff3prog.c.new"));
        assert!(build_script.contains("diff3/meson.build > diff3/meson.build.new"));
        assert!(!build_script.contains("/a char *fgetln(FILE *, size_t *);"));
        assert!(!build_script.contains("/a \\    link_with: [libcompat],"));
        assert!(build_script.contains("lbi_meson build -Dbuildtype=release"));
        assert!(build_script.contains("meson compile -C build -j \"${LWI_MAKE_JOBS}\""));
        assert!(!build_script.contains("$(nproc)"));
    }

    fn test_recipe(chapter: u8) -> GeneratedRecipe {
        let name = format!("pkg{chapter}");
        GeneratedRecipe {
            package: BookPackage {
                chapter,
                section: format!("{chapter}.1"),
                title: format!("Package {chapter} 1.0"),
                name: name.clone(),
                version: "1.0".to_string(),
                layer: layer_for_package(chapter, &name),
                page_url: format!("https://example.invalid/chapter{chapter}/{name}.html"),
                recipe_id: format!("{chapter}-1-{name}"),
            },
            spec_path: PathBuf::from(format!("/tmp/{name}.toml")),
            progress_path: PathBuf::from(format!("/tmp/{name}.done")),
        }
    }

    #[cfg(unix)]
    #[test]
    fn bootstrap_completion_accepts_recorded_symlink_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        let mut recipe = test_recipe(8);
        recipe.package.name = "shadow".to_string();
        recipe.progress_path = tmp.path().join("shadow.done");

        fs::create_dir_all(recipe.progress_path.parent().unwrap()).unwrap();
        fs::write(&recipe.progress_path, "package = \"shadow\"\n").unwrap();
        fs::create_dir_all(sysroot.join("system/systembinaries")).unwrap();
        std::os::unix::fs::symlink("vipw", sysroot.join("system/systembinaries/vigr")).unwrap();

        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        drop(rusqlite::Connection::open(&db_path).unwrap());
        crate::db::get_all_replaces(&db_path).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO packages (name, version, revision) VALUES ('shadow', '1.0', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (package_id, path) SELECT id, 'system/systembinaries/vigr' FROM packages WHERE name = 'shadow'",
            [],
        )
        .unwrap();

        assert!(bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());
    }

    #[test]
    fn bsddiffutils_prepare_hands_off_sbase_cmp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        fs::create_dir_all(sysroot.join("system/binaries")).unwrap();
        fs::create_dir_all(sysroot.join("system/documentation/man-pages/man1")).unwrap();
        fs::write(sysroot.join("system/binaries/cmp"), "sbase cmp").unwrap();
        fs::write(
            sysroot.join("system/documentation/man-pages/man1/cmp.1"),
            "sbase cmp man",
        )
        .unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE packages (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE files (package_id INTEGER NOT NULL, path TEXT NOT NULL);
            INSERT INTO packages (id, name) VALUES (1, 'sbase'), (2, 'other');
            INSERT INTO files (package_id, path) VALUES
                (1, 'system/binaries/cmp'),
                (1, 'system/documentation/man-pages/man1/cmp.1'),
                (1, 'system/binaries/cat'),
                (2, 'system/binaries/keep');
            "#,
        )
        .unwrap();

        let package = BookPackage {
            chapter: 6,
            section: "6.8".to_string(),
            title: "BSD-Diffutils 0.99.0".to_string(),
            name: "bsddiffutils".to_string(),
            version: "0.99.0".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/bsddiffutils.html".to_string(),
            recipe_id: "6-8-bsddiffutils".to_string(),
        };

        prepare_bootstrap_package_install(sysroot, &config, &package).unwrap();

        assert!(!sysroot.join("system/binaries/cmp").exists());
        assert!(
            !sysroot
                .join("system/documentation/man-pages/man1/cmp.1")
                .exists()
        );
        let remaining_sbase_files: Vec<String> = conn
            .prepare(
                "SELECT f.path FROM files f JOIN packages p ON p.id = f.package_id WHERE p.name = 'sbase' ORDER BY f.path",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(remaining_sbase_files, vec!["system/binaries/cat"]);
    }

    #[test]
    fn bsdgrep_prepare_hands_off_sbase_grep_files() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        fs::create_dir_all(sysroot.join("system/binaries")).unwrap();
        fs::create_dir_all(sysroot.join("system/documentation/man-pages/man1")).unwrap();
        fs::write(sysroot.join("system/binaries/grep"), "sbase grep").unwrap();
        fs::write(
            sysroot.join("system/documentation/man-pages/man1/grep.1"),
            "sbase grep man",
        )
        .unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE packages (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE files (package_id INTEGER NOT NULL, path TEXT NOT NULL);
            INSERT INTO packages (id, name) VALUES (1, 'sbase'), (2, 'other');
            INSERT INTO files (package_id, path) VALUES
                (1, 'system/binaries/grep'),
                (1, 'system/documentation/man-pages/man1/grep.1'),
                (1, 'system/binaries/cat'),
                (2, 'system/binaries/keep');
            "#,
        )
        .unwrap();

        let package = BookPackage {
            chapter: 6,
            section: "6.10".to_string(),
            title: "bsdgrep master snapshot".to_string(),
            name: "bsdgrep".to_string(),
            version: "master".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter6/bsdgrep.html".to_string(),
            recipe_id: "6-10-bsdgrep".to_string(),
        };

        prepare_bootstrap_package_install(sysroot, &config, &package).unwrap();

        assert!(!sysroot.join("system/binaries/grep").exists());
        assert!(
            !sysroot
                .join("system/documentation/man-pages/man1/grep.1")
                .exists()
        );
        let remaining_sbase_files: Vec<String> = conn
            .prepare(
                "SELECT f.path FROM files f JOIN packages p ON p.id = f.package_id WHERE p.name = 'sbase' ORDER BY f.path",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(remaining_sbase_files, vec!["system/binaries/cat"]);
    }

    #[test]
    fn bootstrap_package_is_complete_detects_missing_payload_files() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let recipe = GeneratedRecipe {
            package: BookPackage {
                chapter: 6,
                section: "6.17".to_string(),
                title: "GNU Make 4.4.1".to_string(),
                name: "make".to_string(),
                version: "4.4.1".to_string(),
                layer: TEMP_LAYER.to_string(),
                page_url: "https://example.invalid/chapter6/make.html".to_string(),
                recipe_id: "6-17-make".to_string(),
            },
            spec_path: tmp.path().join("make.toml"),
            progress_path: tmp.path().join("make.done"),
        };
        fs::write(&recipe.progress_path, "done").unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE packages (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE files (package_id INTEGER NOT NULL, path TEXT NOT NULL);
            INSERT INTO packages (id, name) VALUES (1, 'make');
            INSERT INTO files (package_id, path) VALUES
                (1, 'system/binaries/make'),
                (1, 'system/documentation/info/dir');
            "#,
        )
        .unwrap();

        assert!(!bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());

        fs::create_dir_all(sysroot.join("system/binaries")).unwrap();
        fs::write(sysroot.join("system/binaries/make"), "binary").unwrap();
        assert!(bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());
    }

    #[test]
    fn bootstrap_package_is_complete_requires_bmake_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let recipe = GeneratedRecipe {
            package: BookPackage {
                chapter: 8,
                section: "8.20".to_string(),
                title: "bmake 20260406".to_string(),
                name: "bmake".to_string(),
                version: "20260406".to_string(),
                layer: DEVEL_LAYER.to_string(),
                page_url: "https://example.invalid/chapter8/bmake.html".to_string(),
                recipe_id: "8-20-bmake".to_string(),
            },
            spec_path: tmp.path().join("bmake.toml"),
            progress_path: tmp.path().join("bmake.done"),
        };
        fs::write(&recipe.progress_path, "recipe_revision = 2\n").unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE packages (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE files (package_id INTEGER NOT NULL, path TEXT NOT NULL);
            INSERT INTO packages (id, name) VALUES (1, 'bmake');
            INSERT INTO files (package_id, path) VALUES
                (1, 'system/share/licenses/bmake/COPYING');
            "#,
        )
        .unwrap();
        fs::create_dir_all(sysroot.join("system/share/licenses/bmake")).unwrap();
        fs::write(
            sysroot.join("system/share/licenses/bmake/COPYING"),
            "license",
        )
        .unwrap();

        assert!(!bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());

        fs::create_dir_all(sysroot.join("system/binaries")).unwrap();
        fs::create_dir_all(sysroot.join("system/share/mk")).unwrap();
        fs::write(sysroot.join("system/binaries/bmake"), "binary").unwrap();
        fs::write(sysroot.join("system/share/mk/sys.mk"), "mk").unwrap();

        assert!(bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());
    }

    #[test]
    fn bootstrap_package_is_complete_requires_llvm_pass1_native_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let recipe = GeneratedRecipe {
            package: BookPackage {
                chapter: 5,
                section: "5.3".to_string(),
                title: "llvm/clang pass 1 22.1.3".to_string(),
                name: "llvm-clang-pass1".to_string(),
                version: "22.1.3".to_string(),
                layer: TEMP_LAYER.to_string(),
                page_url: "https://example.invalid/chapter5/llvm-clang-pass1.html".to_string(),
                recipe_id: "5-3-llvm-clang-pass1".to_string(),
            },
            spec_path: tmp.path().join("llvm-clang-pass1.toml"),
            progress_path: tmp.path().join("llvm-clang-pass1.done"),
        };
        fs::write(&recipe.progress_path, "done\n").unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE packages (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE files (package_id INTEGER NOT NULL, path TEXT NOT NULL);
            INSERT INTO packages (id, name) VALUES (1, 'llvm-clang-pass1');
            INSERT INTO files (package_id, path) VALUES
                (1, 'system/tools/bin/clang');
            "#,
        )
        .unwrap();
        fs::create_dir_all(sysroot.join("system/tools/bin")).unwrap();
        fs::write(sysroot.join("system/tools/bin/clang"), "binary").unwrap();

        assert!(!bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());

        for tool in ["llvm-config", "llvm-tblgen", "clang-tblgen"] {
            fs::write(sysroot.join("system/tools/bin").join(tool), "binary").unwrap();
        }

        assert!(bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());
    }

    #[test]
    fn bootstrap_package_is_complete_reinstalls_stale_bmake_recipe_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let recipe = GeneratedRecipe {
            package: BookPackage {
                chapter: 8,
                section: "8.20".to_string(),
                title: "bmake 20260406".to_string(),
                name: "bmake".to_string(),
                version: "20260406".to_string(),
                layer: DEVEL_LAYER.to_string(),
                page_url: "https://example.invalid/chapter8/bmake.html".to_string(),
                recipe_id: "8-20-bmake".to_string(),
            },
            spec_path: tmp.path().join("bmake.toml"),
            progress_path: tmp.path().join("bmake.done"),
        };

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE packages (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE files (package_id INTEGER NOT NULL, path TEXT NOT NULL);
            INSERT INTO packages (id, name) VALUES (1, 'bmake');
            INSERT INTO files (package_id, path) VALUES
                (1, 'system/binaries/bmake'),
                (1, 'system/share/mk/sys.mk');
            "#,
        )
        .unwrap();
        fs::create_dir_all(sysroot.join("system/binaries")).unwrap();
        fs::create_dir_all(sysroot.join("system/share/mk")).unwrap();
        fs::write(sysroot.join("system/binaries/bmake"), "binary").unwrap();
        fs::write(sysroot.join("system/share/mk/sys.mk"), "mk").unwrap();

        fs::write(&recipe.progress_path, "done\n").unwrap();
        assert!(!bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());

        fs::write(&recipe.progress_path, "recipe_revision = 2\n").unwrap();
        assert!(bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());
    }

    #[test]
    fn bootstrap_package_is_complete_treats_replaced_package_as_done() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let db_path = config.installed_db_path(sysroot);
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let recipe = GeneratedRecipe {
            package: BookPackage {
                chapter: 5,
                section: "5.2".to_string(),
                title: "musl libc headers 1.2.6".to_string(),
                name: "musl-libc-headers".to_string(),
                version: "1.2.6".to_string(),
                layer: TEMP_LAYER.to_string(),
                page_url: "https://example.invalid/chapter5/musl-libc-headers.html".to_string(),
                recipe_id: "5-2-musl-libc-headers".to_string(),
            },
            spec_path: tmp.path().join("musl-libc-headers.toml"),
            progress_path: tmp.path().join("musl-libc-headers.done"),
        };
        fs::write(&recipe.progress_path, "done").unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE packages (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE files (package_id INTEGER NOT NULL, path TEXT NOT NULL);
            CREATE TABLE replaces (
                id INTEGER PRIMARY KEY,
                package_id INTEGER NOT NULL,
                replaces_name TEXT NOT NULL,
                UNIQUE(package_id, replaces_name)
            );
            INSERT INTO packages (id, name) VALUES (1, 'musl-libc-pass2');
            INSERT INTO replaces (package_id, replaces_name) VALUES
                (1, 'musl-libc-headers');
            "#,
        )
        .unwrap();

        assert!(bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());
    }

    #[test]
    fn bootstrap_package_is_complete_treats_retired_package_as_done() {
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path();
        let config = config::Config::for_rootfs(sysroot);
        let recipe = GeneratedRecipe {
            package: BookPackage {
                chapter: 5,
                section: "5.3".to_string(),
                title: "llvm/clang pass 1 22.1.3".to_string(),
                name: "llvm-clang-pass1".to_string(),
                version: "22.1.3".to_string(),
                layer: TEMP_LAYER.to_string(),
                page_url: "https://example.invalid/chapter5/llvm-clang-pass1.html".to_string(),
                recipe_id: "5-3-llvm-clang-pass1".to_string(),
            },
            spec_path: tmp.path().join("llvm-clang-pass1.toml"),
            progress_path: tmp.path().join("llvm-clang-pass1.done"),
        };
        fs::write(&recipe.progress_path, "done").unwrap();
        let retired_path = retired_package_progress_path(&config, &recipe.package.name);
        fs::create_dir_all(retired_path.parent().unwrap()).unwrap();
        fs::write(retired_path, "retired = true\n").unwrap();

        assert!(bootstrap_package_is_complete(sysroot, &config, &recipe).unwrap());
    }

    #[test]
    fn chapter7_boundary_is_detected_before_chapter8() {
        let chapter7 = test_recipe(7);
        let next_chapter7 = test_recipe(7);
        let chapter8 = test_recipe(8);

        assert!(!step_completes_chapter(
            &BookStep::Package(chapter7.package.clone()),
            Some(&BookStep::Package(next_chapter7.package)),
            7,
        ));
        assert!(step_completes_chapter(
            &BookStep::Package(chapter7.package),
            Some(&BookStep::Package(chapter8.package)),
            7,
        ));
    }

    #[test]
    fn temp_layer_filter_removes_chapter7_retired_packages() {
        let filtered = filter_retired_layer_packages(
            TEMP_LAYER,
            vec![
                "llvm-clang-pass1".to_string(),
                "make".to_string(),
                "musl-libc-headers".to_string(),
            ],
        );

        assert_eq!(
            filtered,
            vec!["make".to_string(), "musl-libc-headers".to_string()]
        );
        assert_eq!(
            filter_retired_layer_packages(BASE_LAYER, vec!["llvm-clang-pass1".to_string()]),
            vec!["llvm-clang-pass1".to_string()]
        );
    }

    #[test]
    fn chapter6_install_invocation_uses_cross_prefix_and_bootstrap_path() {
        let recipe = test_recipe(6);
        let invocation = bootstrap_install_invocation(
            Path::new("/target"),
            &recipe,
            BootstrapBuildMode::Cross,
            "x86_64-unknown-linux-musl",
            Path::new("/bin/depot"),
        )
        .unwrap();

        assert!(invocation.args.windows(2).any(|pair| {
            pair[0] == OsStr::new("--cross-prefix")
                && pair[1] == OsStr::new("x86_64-unknown-linux-musl")
        }));
        let path = invocation
            .env
            .iter()
            .find(|(key, _)| key == OsStr::new("PATH"))
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .unwrap();
        assert!(path.starts_with("/target/system/tools/bin:"));
        assert!(
            !path
                .split(':')
                .any(|entry| entry == "/target/system/binaries")
        );
        assert!(
            invocation
                .env
                .iter()
                .any(|(key, _)| key == OsStr::new("LWI_MAKE_FLAGS"))
        );
        assert!(
            invocation
                .env
                .iter()
                .any(|(key, _)| key == OsStr::new("LWI_MAKE_JOBS"))
        );
        assert!(invocation.env.iter().any(|(key, value)| {
            key == OsStr::new(DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS) && value == OsStr::new("1")
        }));
        assert!(invocation.env.iter().any(|(key, value)| {
            key == OsStr::new("DEPOT_LBI_SYSROOT") && value == OsStr::new("/target")
        }));
    }

    #[test]
    fn chapter5_post_pass1_install_invocation_uses_cross_mode() {
        let mut recipe = test_recipe(5);
        recipe.package.section = "5.4".to_string();
        recipe.package.name = "musl-libc-pass2".to_string();
        recipe.package.recipe_id = "5-4-musl-libc-pass2".to_string();

        let mode = build_mode_for_package(&recipe.package);
        let invocation = bootstrap_install_invocation(
            Path::new("/target"),
            &recipe,
            mode,
            "x86_64-unknown-linux-musl",
            Path::new("/bin/depot"),
        )
        .unwrap();

        assert_eq!(mode, BootstrapBuildMode::Cross);
        assert!(invocation.args.windows(2).any(|pair| {
            pair[0] == OsStr::new("--cross-prefix")
                && pair[1] == OsStr::new("x86_64-unknown-linux-musl")
        }));
        let path = invocation
            .env
            .iter()
            .find(|(key, _)| key == OsStr::new("PATH"))
            .map(|(_, value)| value.to_string_lossy().into_owned())
            .unwrap();
        assert!(path.starts_with("/target/system/tools/bin:"));
        assert!(
            !path
                .split(':')
                .any(|entry| entry == "/target/system/binaries")
        );
    }

    #[test]
    fn chapter7_and_8_install_invocations_use_chroot_mode() {
        let recipe = test_recipe(8);
        let invocation = bootstrap_install_invocation(
            Path::new("/target"),
            &recipe,
            BootstrapBuildMode::Chroot,
            "x86_64-unknown-linux-musl",
            Path::new("/bin/depot"),
        )
        .unwrap();

        assert!(
            !invocation
                .args
                .iter()
                .any(|arg| arg == OsStr::new("--cross-prefix"))
        );
        assert!(invocation.env.iter().any(|(key, value)| {
            key == OsStr::new("DEPOT_LBI_CHROOT") && value == OsStr::new("1")
        }));
        assert!(invocation.env.iter().any(|(key, value)| {
            key == OsStr::new("DEPOT_LBI_CHROOT_ROOT") && value == OsStr::new("/target")
        }));
    }

    #[test]
    fn bootstrap_chroot_tool_env_prefers_target_tools() {
        let env = bootstrap_chroot_tool_env();
        assert!(
            env.iter()
                .any(|(key, value)| *key == "AR" && *value == "ar")
        );
        assert!(
            env.iter()
                .any(|(key, value)| *key == "RANLIB" && *value == "ranlib")
        );
        assert!(env.iter().any(|(key, value)| {
            *key == "PATH" && value.split(':').any(|entry| entry == "/system/binaries")
        }));
        assert!(env.iter().any(|(key, value)| {
            *key == "PATH"
                && value
                    .split(':')
                    .any(|entry| entry == BOOTSTRAP_CHROOT_SHIM_DIR)
        }));
    }

    #[test]
    fn bootstrap_chroot_mount_guard_cleans_created_file_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("etc/resolv.conf");

        {
            let mut guard = BootstrapChrootMountGuard::default();
            guard.prepare_file_mount_target(&target).unwrap();
            assert!(target.is_file());
        }

        assert!(!target.exists());
    }

    #[test]
    fn bootstrap_makeflags_prefers_explicit_env_override() {
        let mut env = TestEnv::new();
        env.set_var("LWI_MAKE_FLAGS", "-j37 --output-sync=target");
        assert_eq!(
            bootstrap_parallel_makeflags(),
            "-j37 --output-sync=target".to_string()
        );
    }

    #[test]
    fn bootstrap_make_jobs_prefers_explicit_env_override() {
        let mut env = TestEnv::new();
        env.set_var("LWI_MAKE_JOBS", "37");
        assert_eq!(bootstrap_parallel_make_jobs(), "37");
    }

    #[test]
    fn generated_recipe_passthrough_exports_lwi_parallel_env() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_path = tmp.path().join("pkg.toml");
        let build_path = tmp.path().join("build.sh");
        let package = test_recipe(7).package;
        let recipe = PageRecipe {
            input_files: vec!["pkg-1.0.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/pkg-1.0.tar.gz".to_string()],
            extract_dir: None,
            commands: vec!["make".to_string()],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "Example package".to_string(),
        };

        write_generated_recipe(
            &spec_path,
            &build_path,
            &package,
            &recipe,
            &SourceManifest::default(),
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();

        let spec = fs::read_to_string(spec_path).unwrap();
        assert!(spec.contains("\"LWI_MAKE_FLAGS\""));
        assert!(spec.contains("\"LWI_MAKE_JOBS\""));
        assert!(spec.contains("\"DEPOT_LBI_SYSROOT\""));
    }

    #[test]
    fn generated_recipe_preserves_static_archives_only_for_toolchain_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe = PageRecipe {
            input_files: vec!["pkg-1.0.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/pkg-1.0.tar.gz".to_string()],
            extract_dir: None,
            commands: vec!["make".to_string()],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "Example package".to_string(),
        };

        let names = [
            ("llvm-clang-pass2", true),
            ("llvm", true),
            ("musl-libc-pass2", true),
            ("musl", true),
            ("rustc", true),
            ("bmake", false),
        ];

        for (idx, (name, should_preserve)) in names.into_iter().enumerate() {
            let mut package = test_recipe(8).package;
            package.name = name.to_string();
            package.title = format!("{name} 1.0");
            package.recipe_id = format!("8-{idx}-{name}");
            let spec_path = tmp.path().join(format!("{name}.toml"));
            write_generated_recipe(
                &spec_path,
                &tmp.path().join(format!("{name}-build.sh")),
                &package,
                &recipe,
                &SourceManifest::default(),
                "x86_64-unknown-linux-musl",
                "x86_64",
            )
            .unwrap();

            let spec = fs::read_to_string(spec_path).unwrap();
            assert_eq!(
                spec.contains("no_delete_static = true"),
                should_preserve,
                "{name} static archive cleanup policy should match"
            );
        }
    }

    #[test]
    fn generated_recipe_uses_lbi_blake2_source_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_path = tmp.path().join("pkg.toml");
        let build_path = tmp.path().join("build.sh");
        let package = test_recipe(7).package;
        let recipe = PageRecipe {
            input_files: vec!["pkg-1.0.tar.gz".to_string(), "pkg-fix.patch".to_string()],
            source_urls: vec![
                "https://example.invalid/pkg-1.0.tar.gz".to_string(),
                "https://example.invalid/patches/pkg-fix.patch".to_string(),
            ],
            extract_dir: None,
            commands: vec!["make".to_string()],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "Example package".to_string(),
        };
        let manifest = SourceManifest {
            entries: Vec::new(),
            blake2b_512: BTreeMap::from([
                (
                    "pkg-1.0.tar.gz".to_string(),
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                ),
                (
                    "pkg-fix.patch".to_string(),
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .to_string(),
                ),
            ]),
        };

        write_generated_recipe(
            &spec_path,
            &build_path,
            &package,
            &recipe,
            &manifest,
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();

        let spec = fs::read_to_string(spec_path).unwrap();
        assert!(spec.contains("sha256 = \"b2sum:aaaaaaaa"));
        assert!(spec.contains("sha256 = \"b2sum:bbbbbbbb"));
        assert!(!spec.contains("sha256 = \"skip\""));
    }

    #[test]
    fn generated_specs_record_bootstrap_stage_replacements() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe = PageRecipe {
            input_files: vec!["pkg-1.0.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/pkg-1.0.tar.gz".to_string()],
            extract_dir: None,
            commands: vec!["make".to_string()],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "Example package".to_string(),
        };

        let mut musl_pass2 = test_recipe(5).package;
        musl_pass2.name = "musl-libc-pass2".to_string();
        musl_pass2.title = "musl libc pass 2 1.2.6".to_string();
        let musl_pass2_spec = tmp.path().join("musl-pass2.toml");
        write_generated_recipe(
            &musl_pass2_spec,
            &tmp.path().join("musl-pass2-build.sh"),
            &musl_pass2,
            &recipe,
            &SourceManifest::default(),
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();
        let musl_pass2_spec = fs::read_to_string(musl_pass2_spec).unwrap();
        assert!(musl_pass2_spec.contains("replaces = [\"musl-libc-headers\"]"));

        let mut musl_final = test_recipe(8).package;
        musl_final.name = "musl".to_string();
        musl_final.title = "musl libc final pass 1.2.6".to_string();
        let musl_final_spec = tmp.path().join("musl-final.toml");
        write_generated_recipe(
            &musl_final_spec,
            &tmp.path().join("musl-final-build.sh"),
            &musl_final,
            &recipe,
            &SourceManifest::default(),
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();
        let musl_final_spec = fs::read_to_string(musl_final_spec).unwrap();
        assert!(
            musl_final_spec
                .contains("replaces = [\"musl-libc-pass2\", \"musl-libc-headers\", \"musl-libc\"]")
        );

        let mut llvm_pass2 = test_recipe(6).package;
        llvm_pass2.name = "llvm-clang-pass2".to_string();
        llvm_pass2.title = "llvm/clang pass 2 22.1.3".to_string();
        let llvm_pass2_spec = tmp.path().join("llvm-pass2.toml");
        write_generated_recipe(
            &llvm_pass2_spec,
            &tmp.path().join("llvm-pass2-build.sh"),
            &llvm_pass2,
            &recipe,
            &SourceManifest::default(),
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();
        let llvm_pass2_spec = fs::read_to_string(llvm_pass2_spec).unwrap();
        assert!(llvm_pass2_spec.contains("replaces = [\"llvm-runtimes\"]"));
        assert!(!llvm_pass2_spec.contains("llvm-clang-pass1"));

        let mut llvm_final = test_recipe(8).package;
        llvm_final.name = "llvm".to_string();
        llvm_final.title = "llvm final 22.1.3".to_string();
        let llvm_final_spec = tmp.path().join("llvm-final.toml");
        write_generated_recipe(
            &llvm_final_spec,
            &tmp.path().join("llvm-final-build.sh"),
            &llvm_final,
            &recipe,
            &SourceManifest::default(),
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();
        let llvm_final_spec = fs::read_to_string(llvm_final_spec).unwrap();
        assert!(llvm_final_spec.contains(
            "replaces = [\"llvm-clang-pass2\", \"llvm-clang-pass1\", \"llvm-runtimes\"]"
        ));
    }

    #[test]
    fn byacc_generated_script_exposes_unprefixed_yacc() {
        let mut package = test_recipe(8).package;
        package.name = "byacc".to_string();
        package.title = "byacc stage 2 20260126".to_string();
        package.version = "20260126".to_string();
        package.recipe_id = "8-12-byacc".to_string();
        let recipe = PageRecipe {
            input_files: vec!["byacc-20260126.tgz".to_string()],
            source_urls: vec!["https://example.invalid/byacc-20260126.tgz".to_string()],
            extract_dir: Some("byacc-20260126".to_string()),
            commands: vec![
                "lbi_configure --with-manpage-format=normal".to_string(),
                "make $LWI_MAKE_FLAGS".to_string(),
                "make ${LWI_MAKE_FLAGS:-} install".to_string(),
            ],
            dependencies: Vec::new(),
            license: "BSD-3-Clause".to_string(),
            description: "byacc".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-lbi-linux-musl", "x86_64");

        assert!(
            build_script.contains(r#"ln -sf "$LBI_TARGET-yacc" "$DESTDIR/system/binaries/yacc""#)
        );
        assert!(build_script.contains(
            r#"ln -sf "$LBI_TARGET-yacc.1" "$DESTDIR/system/documentation/man-pages/man1/yacc.1""#
        ));
        assert_eq!(
            bootstrap_required_payload_paths("byacc"),
            &["system/binaries/yacc"]
        );
        assert_eq!(bootstrap_recipe_revision("byacc"), Some(1));
    }

    #[test]
    fn generated_python_bootstrap_specs_use_custom_chroot_script() {
        let tmp = tempfile::tempdir().unwrap();
        let recipe = PageRecipe {
            input_files: vec!["flit_core-3.12.0.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/flit_core-3.12.0.tar.gz".to_string()],
            extract_dir: Some("flit_core-3.12.0".to_string()),
            commands: vec![
                "pip3 wheel -w dist --no-cache-dir --no-build-isolation --no-deps $PWD"
                    .to_string(),
                "pip3 install --root=\"${DESTDIR:-/}\" --prefix=/system --no-index --find-links dist flit_core"
                    .to_string(),
            ],
            dependencies: vec!["python".to_string(), "pip".to_string()],
            license: "BSD-3-Clause".to_string(),
            description: "Build and install flit_core.".to_string(),
        };

        let mut package = test_recipe(8).package;
        package.name = "python-flit-core".to_string();
        package.title = "Python-Flit-Core 3.12.0".to_string();
        package.version = "3.12.0".to_string();
        package.recipe_id = "8-19-python-flit-core".to_string();
        let spec_path = tmp.path().join("python-flit-core.toml");
        let build_path = tmp.path().join("build.sh");
        write_generated_recipe(
            &spec_path,
            &build_path,
            &package,
            &recipe,
            &SourceManifest::default(),
            "x86_64-lbi-linux-musl",
            "x86_64",
        )
        .unwrap();

        let spec = fs::read_to_string(spec_path).unwrap();
        let build_script = fs::read_to_string(build_path).unwrap();
        assert!(spec.contains("type = \"custom\""));
        assert!(!spec.contains("type = \"python\""));
        assert!(spec.contains("DEPOT_LBI_CHROOT"));
        assert!(build_script.contains("internal bootstrap-chroot"));
        assert!(build_script.contains("pip3 wheel -w dist"));
    }

    #[test]
    fn llvm_clang_pass1_generated_script_preserves_compiler_rt_builtins() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_path = tmp.path().join("llvm-clang-pass1.toml");
        let build_path = tmp.path().join("build.sh");
        let package = BookPackage {
            chapter: 5,
            section: "5.3".to_string(),
            title: "llvm/clang pass 1 22.1.3".to_string(),
            name: "llvm-clang-pass1".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/llvm-clang-pass1.html".to_string(),
            recipe_id: "5-3-llvm-clang-pass1".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["llvm-project-22.1.3.src.tar.xz".to_string()],
            source_urls: vec!["https://example.invalid/llvm-project-22.1.3.src.tar.xz".to_string()],
            extract_dir: Some("llvm-project-22.1.3.src".to_string()),
            commands: vec![
                "mkdir -p build-llvm\ncd build-llvm".to_string(),
                r#"cmake -G Ninja "../llvm" \
    -DCMAKE_INSTALL_PREFIX=$LBI_ROOT/system/tools \
    -DLLVM_ENABLE_RUNTIMES="compiler-rt" \
    -DCOMPILER_RT_BUILD_BUILTINS=ON \
    -DCOMPILER_RT_BUILD_SANITIZERS=OFF \
    -DCOMPILER_RT_BUILD_XRAY=OFF \
    -DCOMPILER_RT_BUILD_LIBFUZZER=OFF \
    -DCOMPILER_RT_BUILD_PROFILE=OFF \
    -DCLANG_DEFAULT_RTLIB=compiler-rt \
    -DDEFAULT_SYSROOT=$LBI_ROOT"#
                    .to_string(),
            ],
            dependencies: Vec::new(),
            license: "Apache-2.0".to_string(),
            description: "LLVM pass1".to_string(),
        };

        write_generated_recipe(
            &spec_path,
            &build_path,
            &package,
            &recipe,
            &SourceManifest::default(),
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();

        let build_script = fs::read_to_string(build_path).unwrap();
        assert!(build_script.contains("export LBI_SYSROOT=\"${DEPOT_LBI_SYSROOT:-$LBI_ROOT}\""));
        assert!(build_script.contains("export LWI_CXXFLAGS=\"${LWI_CXXFLAGS:-$LWI_CFLAGS}\""));
        assert!(build_script.contains("rm -rf build-llvm"));
        assert!(build_script.contains("LBI_CCACHE=\"$(command -v ccache)\""));
        assert!(build_script.contains(
            "cmake -G Ninja \"../llvm\" \\\n    -DCMAKE_C_COMPILER_LAUNCHER=\"$LBI_CCACHE\" \\"
        ));
        assert!(build_script.contains("-DCMAKE_CXX_COMPILER_LAUNCHER=\"$LBI_CCACHE\""));
        assert!(build_script.contains("-DCMAKE_ASM_COMPILER_LAUNCHER=\"$LBI_CCACHE\""));
        assert!(!build_script.contains("-DLLVM_CCACHE_BUILD=ON"));
        assert!(build_script.contains("LLVM_ENABLE_RUNTIMES=\"compiler-rt\""));
        assert!(build_script.contains("COMPILER_RT_BUILD_BUILTINS=ON"));
        assert!(build_script.contains("COMPILER_RT_DEFAULT_TARGET_ONLY=ON"));
        assert!(
            build_script.contains(
                "BUILTINS_CMAKE_ARGS=\"-DCMAKE_C_FLAGS=--sysroot=$LBI_SYSROOT;-DCMAKE_ASM_FLAGS=--sysroot=$LBI_SYSROOT\""
            )
        );
        assert!(build_script.contains("COMPILER_RT_BUILD_SANITIZERS=OFF"));
        assert!(build_script.contains("COMPILER_RT_BUILD_XRAY=OFF"));
        assert!(build_script.contains("COMPILER_RT_BUILD_LIBFUZZER=OFF"));
        assert!(build_script.contains("COMPILER_RT_BUILD_PROFILE=OFF"));
        assert!(build_script.contains("COMPILER_RT_BUILD_CRT=OFF"));
        assert!(build_script.contains("COMPILER_RT_BUILD_MEMPROF=OFF"));
        assert!(build_script.contains("COMPILER_RT_BUILD_ORC=OFF"));
        assert!(build_script.contains("COMPILER_RT_BUILD_CTX_PROFILE=OFF"));
        assert!(build_script.contains("COMPILER_RT_INCLUDE_TESTS=OFF"));
        assert!(build_script.contains("CLANG_DEFAULT_RTLIB=compiler-rt"));
        assert!(build_script.contains("-DDEFAULT_SYSROOT=$LBI_SYSROOT"));
        assert!(build_script.contains("unset DESTDIR"));
        assert!(build_script.contains("-DCMAKE_INSTALL_PREFIX=$LBI_ROOT/system/tools"));
        assert!(
            !build_script
                .contains("export CC=\"${CC:-$LBI_SYSROOT/system/tools/bin/$LBI_TARGET-clang}\"")
        );
    }

    #[test]
    fn chapter5_post_pass1_scripts_default_to_cross_toolchain() {
        let package = BookPackage {
            chapter: 5,
            section: "5.4".to_string(),
            title: "musl libc pass 2 1.2.6".to_string(),
            name: "musl-libc-pass2".to_string(),
            version: "1.2.6".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/musl-libc-pass2.html".to_string(),
            recipe_id: "5-4-musl-libc-pass2".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["musl-1.2.6.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/musl-1.2.6.tar.gz".to_string()],
            extract_dir: Some("musl-1.2.6".to_string()),
            commands: vec![
                "lbi_configure --target=\"$LBI_TARGET\" --with-malloc=mimalloc\nmake $LWI_MAKE_FLAGS".to_string(),
                "ln -snf ./libc.so \\\n    \"$LBI_ROOT/system/libraries/ld-musl-${LBI_ARCH}.so.1\"\n\nls -lh \"$LBI_ROOT/usr/lib/ld-musl-${LBI_ARCH}.so.1\"".to_string(),
            ],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "musl pass2".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert_eq!(build_mode_for_package(&package), BootstrapBuildMode::Cross);
        assert!(build_script.contains("export CC=\"${CC:-$(lbi_find_cross_tool clang)}\""));
        assert!(build_script.contains("export CXX=\"${CXX:-$(lbi_find_cross_tool clang++)}\""));
        assert!(build_script.contains("libclang_rt.builtins-${compiler_rt_arch}.a"));
        assert!(build_script.contains("export LIBCC"));
        assert!(build_script.contains("make $LWI_MAKE_FLAGS"));
        assert!(!build_script.contains("make ${LWI_MAKE_FLAGS:-} $LWI_MAKE_FLAGS"));
        assert!(
            build_script.contains("ls -lh \"$LBI_ROOT/system/libraries/ld-musl-${LBI_ARCH}.so.1\"")
        );
        assert!(!build_script.contains("$LBI_ROOT/usr/lib/ld-musl-${LBI_ARCH}.so.1"));
        assert!(!build_script.contains("ln -snf"));
        assert!(
            build_script.contains("rm -f \"$LBI_ROOT/system/libraries/ld-musl-${LBI_ARCH}.so.1\"")
        );
        assert!(build_script.contains("ln -sf ./libc.so"));
        assert!(build_script.contains("rm -f \"$LBI_ROOT/lib/ld-musl-${LBI_ARCH}.so.1\""));
        assert!(build_script.contains("rmdir \"$LBI_ROOT/lib\" 2>/dev/null || true"));
        assert!(build_script.contains("export LWI_MAKE_JOBS=\"$jobs\""));
        assert!(build_script.contains("export LWI_MAKE_FLAGS=\"-j${LWI_MAKE_JOBS}\""));

        let final_package = BookPackage {
            chapter: 8,
            section: "8.6".to_string(),
            title: "musl libc final pass 1.2.6".to_string(),
            name: "musl".to_string(),
            version: "1.2.6".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/musl-libc-final-pass.html".to_string(),
            recipe_id: "8-6-musl".to_string(),
        };
        let final_script = generated_build_script(
            &final_package,
            &recipe,
            "x86_64-unknown-linux-musl",
            "x86_64",
        );
        assert!(!final_script.contains("ln -snf"));
        assert!(final_script.contains("ln -sf ./libc.so"));
    }

    #[test]
    fn lbi_helpers_default_to_target_tuple() {
        let package = BookPackage {
            chapter: 8,
            section: "8.99".to_string(),
            title: "Example 1.0".to_string(),
            name: "example".to_string(),
            version: "1.0".to_string(),
            layer: BASE_LAYER.to_string(),
            page_url: "https://example.invalid/chapter8/example.html".to_string(),
            recipe_id: "8-99-example".to_string(),
        };
        let recipe = PageRecipe {
            input_files: vec!["example-1.0.tar.gz".to_string()],
            source_urls: vec!["https://example.invalid/example-1.0.tar.gz".to_string()],
            extract_dir: Some("example-1.0".to_string()),
            commands: vec!["lbi_configure\nmake $LWI_MAKE_FLAGS".to_string()],
            dependencies: Vec::new(),
            license: "MIT".to_string(),
            description: "Example".to_string(),
        };

        let build_script =
            generated_build_script(&package, &recipe, "x86_64-unknown-linux-musl", "x86_64");

        assert!(build_script.contains("        --target=\"$LBI_TARGET\" \\\n"));
        assert!(build_script.contains("        --host=\"$LBI_TARGET\" \\\n"));
        assert!(!build_script.contains("        --build=\"$LBI_TARGET\" \\\n"));
        assert!(build_script.contains("-DCMAKE_C_COMPILER_TARGET=\"$LBI_TARGET\""));
        assert!(build_script.contains("-DCMAKE_CXX_COMPILER_TARGET=\"$LBI_TARGET\""));
        assert!(
            build_script
                .contains("meson setup \"$build_dir\" --cross-file \"$lbi_meson_cross_file\"")
        );
        assert!(
            build_script.contains("c_args = ['--target=$LBI_TARGET', '--sysroot=$LBI_SYSROOT']")
        );
        assert!(
            build_script.contains("cpp_args = ['--target=$LBI_TARGET', '--sysroot=$LBI_SYSROOT']")
        );
    }

    #[test]
    fn copied_build_profile_does_not_default_build_tuple() {
        let tmp = tempfile::tempdir().unwrap();

        copy_build_profile(tmp.path(), "x86_64-unknown-linux-musl", "x86_64").unwrap();

        let profile = fs::read_to_string(tmp.path().join("etc/profile.d/lbi-build.sh")).unwrap();
        assert!(profile.contains("        --target=\"$LBI_TARGET\" \\\n"));
        assert!(profile.contains("        --host=\"$LBI_TARGET\" \\\n"));
        assert!(!profile.contains("        --build=\"$LBI_TARGET\" \\\n"));
    }

    #[test]
    fn rewrite_make_flags_does_not_duplicate_existing_parallel_flags() {
        assert_eq!(
            rewrite_make_flags("make $LWI_MAKE_FLAGS\nmake install"),
            "make $LWI_MAKE_FLAGS\nmake ${LWI_MAKE_FLAGS:-} install"
        );
        assert_eq!(
            rewrite_make_flags("make ${MAKEFLAGS:-} all"),
            "make ${MAKEFLAGS:-} all"
        );
    }

    #[test]
    fn rewrite_parallel_job_counts_uses_bootstrap_job_variable() {
        assert_eq!(
            rewrite_parallel_job_counts("meson compile -C build -j \"$(nproc)\""),
            "meson compile -C build -j \"${LWI_MAKE_JOBS}\""
        );
        assert_eq!(
            rewrite_parallel_job_counts("ninja -j$(nproc)"),
            "ninja -j${LWI_MAKE_JOBS}"
        );
    }

    #[test]
    fn rewrite_lbi_command_removes_chroot_nproc_dependency() {
        assert_eq!(
            rewrite_lbi_command("meson compile -C build -j \"$(nproc)\"", None),
            "meson compile -C build -j \"${LWI_MAKE_JOBS}\""
        );
    }

    #[test]
    fn cross_toolchain_defaults_start_after_pass1() {
        let pre_pass1 = BookPackage {
            chapter: 5,
            section: "5.2".to_string(),
            title: "musl libc headers 1.2.6".to_string(),
            name: "musl-libc-headers".to_string(),
            version: "1.2.6".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/musl-libc-headers.html".to_string(),
            recipe_id: "5-2-musl-libc-headers".to_string(),
        };
        let pass1 = BookPackage {
            chapter: 5,
            section: "5.3".to_string(),
            title: "llvm/clang pass 1 22.1.3".to_string(),
            name: "llvm-clang-pass1".to_string(),
            version: "22.1.3".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/llvm-clang-pass1.html".to_string(),
            recipe_id: "5-3-llvm-clang-pass1".to_string(),
        };
        let post_pass1 = BookPackage {
            chapter: 5,
            section: "5.4".to_string(),
            title: "example package".to_string(),
            name: "example".to_string(),
            version: "1.0".to_string(),
            layer: TEMP_LAYER.to_string(),
            page_url: "https://example.invalid/chapter5/example.html".to_string(),
            recipe_id: "5-4-example".to_string(),
        };

        assert!(!use_cross_toolchain_by_default(&pre_pass1));
        assert!(!use_cross_toolchain_by_default(&pass1));
        assert!(use_cross_toolchain_by_default(&post_pass1));
    }

    #[cfg(unix)]
    #[test]
    fn root_transition_skips_reexec_when_already_root() {
        assert_eq!(
            root_transition(true, Some(OsStr::new(""))).unwrap(),
            RootTransition::AlreadyRoot
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_transition_prefers_sudo_then_doas() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["sudo", "doas"] {
            let path = bin.join(name);
            fs::write(&path, "#!/bin/sh\n").unwrap();
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
        let action = root_transition(false, Some(bin.as_os_str())).unwrap();

        assert_eq!(action, RootTransition::Reexec(bin.join("sudo")));
    }

    #[test]
    fn root_transition_errors_without_helper() {
        let err = root_transition(false, Some(OsStr::new(""))).unwrap_err();
        assert!(err.to_string().contains("neither sudo nor doas"));
    }

    #[test]
    fn parses_body_style_section_lines() {
        assert_eq!(
            parse_section_line("5.5. LLVM runtimes (libunwind, libcxxabi, libcxx) 22.1.3"),
            Some((
                "5.5".to_string(),
                "LLVM runtimes (libunwind, libcxxabi, libcxx) 22.1.3".to_string()
            ))
        );
    }

    #[test]
    fn parses_bulleted_toc_section_lines() {
        assert_eq!(
            parse_section_line("\u{c}   ▪ 8.17 libffi 3.5.2"),
            Some(("8.17".to_string(), "libffi 3.5.2".to_string()))
        );
        assert_eq!(
            parse_section_line("   • 8.23 Meson 1.11.1"),
            Some(("8.23".to_string(), "Meson 1.11.1".to_string()))
        );
    }

    #[test]
    fn skips_chapter_heading_section_lines() {
        assert_eq!(parse_section_line("5. Cross-Compilation Setup"), None);
        assert_eq!(
            parse_section_line("8. Compiling the Remaining utilities for the system"),
            None
        );
    }

    #[test]
    fn normalizes_known_titles() {
        assert_eq!(
            package_name_from_title("BSD-Diffutils stage 2 0.99.0").as_deref(),
            Some("bsddiffutils")
        );
        assert_eq!(
            package_name_from_title("patch stage 2 0.99.1").as_deref(),
            Some("bsdpatch")
        );
        assert_eq!(
            package_name_from_title("uutils-coreutils 0.8.0").as_deref(),
            Some("uutils-coreutils")
        );
        assert_eq!(
            package_name_from_title("musl libc final pass 1.2.6").as_deref(),
            Some("musl")
        );
        assert_eq!(
            package_name_from_title("LLVM final 22.1.3").as_deref(),
            Some("llvm")
        );
        assert_eq!(
            package_name_from_title("bzip2 1.0.8").as_deref(),
            Some("bzip2")
        );
        assert_eq!(package_name_from_title("m4 1.4.20").as_deref(), Some("m4"));
        assert_eq!(package_name_from_title("."), None);
    }

    #[cfg(unix)]
    #[test]
    fn initializes_lbi_layout_for_fresh_bootstrap() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config::Config::for_rootfs(tmp.path());

        ensure_lbi_layout_for_fresh_bootstrap(
            tmp.path(),
            &config,
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();

        let state = system_state::load(&config).unwrap();
        assert_eq!(state.stage.as_deref(), Some("layout"));
        assert!(tmp.path().join("etc/depot.d/build.toml").exists());
        assert!(tmp.path().join("system/binaries").is_dir());
    }

    #[test]
    fn essential_system_files_include_lwi_account_defaults() {
        let tmp = tempfile::tempdir().unwrap();

        create_essential_system_files(tmp.path()).unwrap();

        let passwd = fs::read_to_string(tmp.path().join("etc/passwd")).unwrap();
        let group = fs::read_to_string(tmp.path().join("etc/group")).unwrap();
        assert!(passwd.contains("root:x:0:0:root:/system/charlie:/bin/oksh"));
        assert!(passwd.contains("messagebus:x:18:18:D-Bus Message Daemon User"));
        assert!(group.contains("users:x:999:"));
        assert!(group.contains("wheel:x:97:"));
    }

    #[test]
    fn skips_lbi_layout_initialization_when_resuming() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config::Config::for_rootfs(tmp.path());
        system_state::set_stage(&config, "bootstrap-layers".to_string()).unwrap();

        ensure_lbi_layout_for_fresh_bootstrap(
            tmp.path(),
            &config,
            "x86_64-unknown-linux-musl",
            "x86_64",
        )
        .unwrap();

        assert!(!tmp.path().join("etc/depot.d/build.toml").exists());
        assert_eq!(
            fs::read_link(tmp.path().join("usr/include")).unwrap(),
            PathBuf::from("../system/headers")
        );
        assert_eq!(
            fs::read_link(tmp.path().join("dev")).unwrap(),
            PathBuf::from("system/devices")
        );
        let state = system_state::load(&config).unwrap();
        assert_eq!(state.stage.as_deref(), Some("bootstrap-layers"));
    }
}
