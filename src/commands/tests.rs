use super::*;
use crate::cli::{
    BuildArgs, BuildExecArgs, Cli, InstallArgs, Lib32Args, PromptArgs, RootfsArgs, SearchArgs,
    UpdateArgs,
};
use crate::test_support::TestEnv;
use git2::{Oid, Repository};
use std::path::Path;
use std::sync::{
    Barrier, Mutex, MutexGuard,
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
};

static ASSUME_YES_TEST_LOCK: Mutex<()> = Mutex::new(());

fn assume_yes_test_lock() -> MutexGuard<'static, ()> {
    ASSUME_YES_TEST_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

fn rootfs_args(rootfs: impl Into<PathBuf>) -> RootfsArgs {
    RootfsArgs {
        rootfs: rootfs.into(),
    }
}

fn prompt_args(yes: bool) -> PromptArgs {
    PromptArgs { yes }
}

fn build_exec_args() -> BuildExecArgs {
    BuildExecArgs {
        no_deps: false,
        no_flags: false,
        cross_prefix: None,
        clean: false,
        dry_run: false,
        test_deps: false,
    }
}

fn lib32_args() -> Lib32Args {
    Lib32Args { lib32_only: false }
}

fn write_basic_binary_archive(
    archive_path: &Path,
    package_name: &str,
    version: &str,
    revision: u32,
    payload_path: &str,
    payload: &[u8],
) -> Result<()> {
    let file = fs::File::create(archive_path)
        .with_context(|| format!("Failed to create {}", archive_path.display()))?;
    let encoder =
        zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
    let mut tar = tar::Builder::new(encoder);

    let mut payload_header = tar::Header::new_gnu();
    payload_header.set_path(payload_path)?;
    payload_header.set_size(payload.len() as u64);
    payload_header.set_mode(0o755);
    payload_header.set_cksum();
    tar.append(&payload_header, payload)?;

    let metadata = format!(
        "name = \"{package_name}\"\nversion = \"{version}\"\nrevision = {revision}\ndescription = \"test\"\nhomepage = \"https://example.test\"\nlicense = \"MIT\"\n[dependencies]\nruntime = []\noptional = []\n"
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

fn test_binary_repo_record(name: &str, filename: &str) -> db::repo::BinaryRepoPackageRecord {
    db::repo::BinaryRepoPackageRecord {
        repo_name: "core".into(),
        name: name.into(),
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: None,
        filename: filename.into(),
        size: 1,
        sha512: "sha512".into(),
        description: None,
        homepage: None,
        license: None,
        provides: Vec::new(),
        conflicts: Vec::new(),
        replaces: Vec::new(),
        runtime_dependencies: Vec::new(),
        optional_dependencies: Vec::new(),
        groups: Vec::new(),
    }
}

fn test_package_spec(
    build_type: package::BuildType,
    make_test_target: Option<&str>,
    make_test_targets: &[&str],
) -> package::PackageSpec {
    let mut flags = package::BuildFlags::default();
    if let Some(target) = make_test_target {
        flags.make_test_target = target.to_string();
    }
    flags.make_test_targets = make_test_targets
        .iter()
        .map(|target| (*target).to_string())
        .collect();

    package::PackageSpec {
        package: package::PackageInfo {
            name: "pkg".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Default::default(),
        manual_sources: Vec::new(),
        source: vec![package::Source {
            url: "https://example.test/pkg.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "pkg".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: package::Build { build_type, flags },
        dependencies: package::Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    }
}

fn commit_git_file(repo: &Repository, workdir: &Path, rel: &str, data: &str) -> Oid {
    let full_path = workdir.join(rel);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full_path, data).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new(rel)).unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("depot-test", "depot@example.test").unwrap();
    let mut parents = Vec::new();
    if let Ok(head) = repo.head()
        && let Some(oid) = head.target()
    {
        parents.push(repo.find_commit(oid).unwrap());
    }
    let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();

    repo.commit(Some("HEAD"), &sig, &sig, "test", &tree, &parent_refs)
        .unwrap()
}

fn make_remote_git_repo() -> (tempfile::TempDir, String, Oid) {
    let tmp = tempfile::tempdir().unwrap();
    let remote_dir = tmp.path().join("origin.git");
    let workdir = tmp.path().join("work");

    Repository::init_bare(&remote_dir).unwrap();
    let repo = Repository::init(&workdir).unwrap();
    let tagged = commit_git_file(&repo, &workdir, "README", "tagged\n");
    let tag_target = repo.find_object(tagged, None).unwrap();
    repo.tag_lightweight("v1.0.0", &tag_target, false).unwrap();

    let branch_ref = repo.head().unwrap().name().unwrap().to_string();
    let mut remote = repo.remote("origin", remote_dir.to_str().unwrap()).unwrap();
    let push_specs = [
        format!("{branch_ref}:{branch_ref}"),
        "refs/tags/v1.0.0:refs/tags/v1.0.0".to_string(),
    ];
    let push_spec_refs: Vec<&String> = push_specs.iter().collect();
    remote.push(&push_spec_refs, None).unwrap();

    let remote_url = url::Url::from_file_path(&remote_dir).unwrap().to_string();
    (tmp, remote_url, tagged)
}

fn register_installed_test_package(
    config: &config::Config,
    rootfs: &Path,
    name: &str,
    version: &str,
) -> Result<()> {
    let mut spec = test_package_spec(package::BuildType::Bin, None, &[]);
    spec.package.name = name.to_string();
    spec.package.version = version.to_string();

    let dest = rootfs.join("dest").join(name);
    fs::create_dir_all(dest.join("usr/bin"))?;
    fs::write(dest.join("usr/bin").join(name), name)?;
    db::register_package(&config.installed_db_path(rootfs), &spec, &dest)?;
    Ok(())
}

fn set_installed_test_package_completed_at(
    config: &config::Config,
    rootfs: &Path,
    name: &str,
    completed_at: i64,
) -> Result<()> {
    let db_path = config.installed_db_path(rootfs);
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("Failed to open {}", db_path.display()))?;
    conn.execute(
        "UPDATE packages SET completed_at = ?1 WHERE name = ?2",
        rusqlite::params![completed_at, name],
    )
    .with_context(|| format!("Failed to update completed_at for package '{}'", name))?;
    Ok(())
}

fn register_required_development_package_if_configured(
    config: &config::Config,
    rootfs: &Path,
) -> Result<()> {
    if let Some(package_name) = builder::requested_development_package() {
        register_installed_test_package(config, rootfs, &package_name, "1.0.0")?;
    }
    Ok(())
}

fn write_test_repo_spec(path: &Path, name: &str, version: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!(
            r#"[package]
name = "{name}"
version = "{version}"
revision = 1
description = "{name}"
homepage = "https://example.test/{name}"
license = "MIT"

[[source]]
url = "https://example.test/{name}-{version}.tar.gz"
sha256 = "skip"
extract_dir = "{name}-{version}"

[build]
type = "custom"

[dependencies]
build = []
runtime = []
optional = []
"#
        ),
    )
    .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}
use anyhow::Context;
use std::io::Write;

fn file_ownership_test_spec(name: &str, version: &str) -> package::PackageSpec {
    let mut spec = test_package_spec(package::BuildType::Bin, None, &[]);
    spec.package.name = name.to_string();
    spec.package.version = version.to_string();
    spec.source.clear();
    spec
}

fn stage_file(destdir: &Path, path: &str, contents: &str) -> Result<()> {
    let file = destdir.join(path);
    fs::create_dir_all(
        file.parent()
            .context("Staged file path must have a parent")?,
    )?;
    fs::write(file, contents)?;
    Ok(())
}

mod build_cases;
mod cli_cases;
mod install_cases;
mod update_cases;
mod version_cases;
