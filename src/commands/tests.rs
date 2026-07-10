use super::*;
use crate::cli::{
    BuildArgs, BuildExecArgs, Cli, InstallArgs, Lib32Args, PromptArgs, RootfsArgs, SearchArgs,
    UpdateArgs,
};
use crate::test_support::TestEnv;
use git2::{Oid, Repository};
use std::path::Path;
use std::sync::{
    Mutex, MutexGuard,
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

#[test]
fn build_env_rootfs_uses_selected_non_live_rootfs() {
    let tmp = tempfile::tempdir().unwrap();
    let expected = tmp.path().canonicalize().unwrap();

    assert_eq!(
        build_cmd::build_env_rootfs(tmp.path()),
        expected.to_string_lossy()
    );
    assert_eq!(build_cmd::build_env_rootfs(Path::new("/")), "/");
}

#[test]
fn parallel_verification_processes_every_item() -> Result<()> {
    let items = vec![0_u8; 32];
    let completed = AtomicUsize::new(0);
    let progress = ProgressBar::hidden();

    run_parallel_verification(&items, &progress, |_| {
        completed.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(())
    })?;

    assert_eq!(completed.load(AtomicOrdering::Relaxed), items.len());
    Ok(())
}

#[test]
fn install_post_extract_env_uses_selected_non_live_rootfs() -> Result<()> {
    let _guard = assume_yes_test_lock();
    let temp = tempfile::tempdir().context("Failed to create temp dir")?;
    let rootfs = temp.path().join("rootfs");
    let spec_dir = temp.path().join("packages").join("demo");
    let source_dir = temp.path().join("source").join("demo-1.0.0");
    let observed_env = temp.path().join("post-extract-rootfs.txt");
    fs::create_dir_all(&rootfs)?;
    fs::create_dir_all(&spec_dir)?;
    fs::create_dir_all(&source_dir)?;
    fs::write(source_dir.join("README"), "demo source")?;
    fs::write(
        spec_dir.join("build.sh"),
        "mkdir -p \"$DESTDIR/usr/bin\"\nprintf demo > \"$DESTDIR/usr/bin/demo\"\n",
    )?;

    let spec_path = spec_dir.join("demo.toml");
    fs::write(
        &spec_path,
        format!(
            r#"[package]
name = "demo"
version = "1.0.0"
revision = 1
description = "demo"
homepage = "https://example.test/demo"
license = "MIT"

[[source]]
url = "file://{}"
sha256 = "skip"
extract_dir = "demo-1.0.0"
post_extract = ["printf '%s' \"$DEPOT_ROOTFS\" > '{}'"]

[build]
type = "custom"

[dependencies]
build = []
runtime = []
optional = []
"#,
            source_dir.display(),
            observed_env.display()
        ),
    )?;

    let config = config::Config::for_rootfs(&rootfs);
    register_required_development_package_if_configured(&config, &rootfs)?;

    run(Cli {
        command: Commands::Install(InstallArgs {
            rootfs_args: rootfs_args(rootfs.clone()),
            prompt_args: prompt_args(true),
            build_exec_args: BuildExecArgs {
                no_deps: true,
                ..build_exec_args()
            },
            lib32_args: lib32_args(),
            spec_or_archive: vec![spec_path],
            spec: None,
        }),
    })?;

    assert_eq!(
        fs::read_to_string(&observed_env)?,
        build_cmd::build_env_rootfs(&rootfs)
    );
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

#[test]
fn run_internal_clone_checks_out_git_revision() {
    let (_tmp, remote_url, tagged) = make_remote_git_repo();
    let clone_root = tempfile::tempdir().unwrap();
    let dest = clone_root.path().join("cloned-src");

    run_internal_command(InternalCommands::Clone {
        repo: format!("{remote_url}#v1.0.0"),
        dest: Some(dest.clone()),
    })
    .unwrap();

    let repo = Repository::open(&dest).unwrap();
    assert_eq!(repo.head().unwrap().target().unwrap(), tagged);
    assert_eq!(
        std::fs::read_to_string(dest.join("README")).unwrap(),
        "tagged\n"
    );
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
fn clean_build_source_dirs_removes_build_dir_only() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.build_dir = rootfs.path().join("tmp/build");
    cfg.cache_dir = rootfs.path().join("tmp/sources");

    fs::create_dir_all(&cfg.build_dir)
        .with_context(|| format!("Failed to create {}", cfg.build_dir.display()))?;
    fs::create_dir_all(&cfg.cache_dir)
        .with_context(|| format!("Failed to create {}", cfg.cache_dir.display()))?;

    clean_build_source_dirs(&cfg)?;

    assert!(!cfg.build_dir.exists());
    assert!(cfg.cache_dir.exists());
    Ok(())
}

#[test]
fn clean_build_source_dirs_noops_when_build_dir_missing() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.build_dir = rootfs.path().join("tmp/build");
    cfg.cache_dir = rootfs.path().join("tmp/sources");

    fs::create_dir_all(&cfg.cache_dir)
        .with_context(|| format!("Failed to create {}", cfg.cache_dir.display()))?;

    clean_build_source_dirs(&cfg)?;

    assert!(!cfg.build_dir.exists());
    assert!(cfg.cache_dir.exists());
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
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: None,
        filename: archive_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default()
            .to_string(),
        size: payload.len() as u64,
        sha512: String::new(),
        description: Some("test package".into()),
        homepage: Some("https://example.test".into()),
        license: Some("MIT".into()),
        provides: vec!["pkg-virtual".into()],
        conflicts: Vec::new(),
        replaces: Vec::new(),
        runtime_dependencies: vec!["glibc".into()],
        optional_dependencies: vec!["manpages".into()],
        groups: vec!["base".into()],
    };
    let spec = package_spec_from_repo_record(&record);
    let installed = install_package_outputs_to_rootfs(&spec, staged.path(), rootfs.path(), &cfg)?;

    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].package.name, "pkg");
    assert!(rootfs.path().join("usr/bin/hello").exists());

    let db_path = cfg.installed_db_path(rootfs.path());
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
        let encoder =
            zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
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
    fn write_archive(archive_path: &Path, package_name: &str, conflicts: &[&str]) -> Result<()> {
        let file = fs::File::create(archive_path)
            .with_context(|| format!("Failed to create {}", archive_path.display()))?;
        let encoder =
            zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
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
fn update_transaction_runs_matching_transaction_hook_once_for_batch() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
    let old_alpha = pkg_dir.path().join("alpha-1.0-1-x86_64.depot.pkg.tar.zst");
    let old_beta = pkg_dir.path().join("beta-1.0-1-x86_64.depot.pkg.tar.zst");
    let new_alpha = pkg_dir.path().join("alpha-2.0-1-x86_64.depot.pkg.tar.zst");
    let new_beta = pkg_dir.path().join("beta-2.0-1-x86_64.depot.pkg.tar.zst");
    write_basic_binary_archive(&old_alpha, "alpha", "1.0", 1, "usr/bin/alpha", b"alpha-old")?;
    write_basic_binary_archive(&old_beta, "beta", "1.0", 1, "usr/bin/beta", b"beta-old")?;
    write_basic_binary_archive(&new_alpha, "alpha", "2.0", 1, "usr/bin/alpha", b"alpha-new")?;
    write_basic_binary_archive(&new_beta, "beta", "2.0", 1, "usr/bin/beta", b"beta-new")?;

    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.build_dir = rootfs.path().join("var/cache/depot/build");
    cfg.db_dir = rootfs.path().join("var/lib/depot");

    run_direct_archive_install_requests(
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
        &[old_alpha, old_beta],
        false,
    )?;

    let hooks_dir = install::hooks::transaction_hooks_dir(rootfs.path());
    fs::create_dir_all(&hooks_dir)?;
    fs::write(
        hooks_dir.join("90-update-batch.toml"),
        r#"
[hook]
name = "update batch recorder"

[when]
phase = "post"
operation = ["update"]
paths = ["usr/bin/*"]

[exec]
command = "printf '%s:%s\n' \"$DEPOT_ACTION\" \"$DEPOT_PACKAGE\" >> \"$DEPOT_ROOTFS/hook-runs\"; cat >> \"$DEPOT_ROOTFS/hook-targets\""
needs_paths = true
"#,
    )?;

    let updated = run_update_transaction_install_requests(
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
        &[new_alpha, new_beta],
    )?;

    assert!(updated);
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/bin/alpha"))?,
        "alpha-new"
    );
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/bin/beta"))?,
        "beta-new"
    );
    let hook_runs = fs::read_to_string(rootfs.path().join("hook-runs"))?;
    assert_eq!(hook_runs.lines().collect::<Vec<_>>(), vec!["update:alpha"]);
    let hook_targets: BTreeSet<_> = fs::read_to_string(rootfs.path().join("hook-targets"))?
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        hook_targets,
        BTreeSet::from(["usr/bin/alpha".to_string(), "usr/bin/beta".to_string()])
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
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: None,
        filename: archive_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default()
            .to_string(),
        size: payload.len() as u64,
        sha512: String::new(),
        description: Some("sudo".into()),
        homepage: Some("https://example.test".into()),
        license: Some("ISC".into()),
        provides: Vec::new(),
        conflicts: Vec::new(),
        replaces: Vec::new(),
        runtime_dependencies: Vec::new(),
        optional_dependencies: Vec::new(),
        groups: Vec::new(),
    };
    let spec = package_spec_from_repo_record(&record);
    let installed = install_package_outputs_to_rootfs(&spec, staged.path(), rootfs.path(), &cfg)?;

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

    let installed = install_package_outputs_to_rootfs(&spec, staged.path(), rootfs.path(), &cfg)?;
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
fn binary_archive_install_honors_replaces_from_metadata() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let pkg_dir = tempfile::tempdir().context("Failed to create temp package dir")?;
    let archive_path = pkg_dir.path().join("vx-0.1.0-1-x86_64.depot.pkg.tar.zst");

    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.build_dir = rootfs.path().join("var/cache/depot/build");
    cfg.db_dir = rootfs.path().join("var/lib/depot");

    let old_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "diffutils".into(),
            real_name: None,
            version: "3.12".into(),
            revision: 1,
            description: "diffutils".into(),
            homepage: "https://example.test/diffutils".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["GPL-3.0-or-later".into()],
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
    let old_dest = rootfs.path().join("old-dest");
    fs::create_dir_all(old_dest.join("usr/bin"))?;
    fs::write(old_dest.join("usr/bin/diff"), "old-diff")?;
    install_package_outputs_to_rootfs(&old_spec, &old_dest, rootfs.path(), &cfg)?;

    let file = fs::File::create(&archive_path)
        .with_context(|| format!("Failed to create {}", archive_path.display()))?;
    let encoder =
        zstd::stream::write::Encoder::new(file, 3).context("Failed to create zstd encoder")?;
    let mut tar = tar::Builder::new(encoder);

    let payload = b"vx-diff";
    let mut payload_header = tar::Header::new_gnu();
    payload_header.set_path("usr/bin/diff")?;
    payload_header.set_size(payload.len() as u64);
    payload_header.set_mode(0o755);
    payload_header.set_cksum();
    tar.append(&payload_header, &payload[..])?;

    let metadata = br#"name = "vx"
version = "0.1.0"
revision = 1
description = "vertex utils"
homepage = "https://example.test/vx"
license = "MIT"
replaces = ["diffutils"]

[dependencies]
runtime = []
optional = []
"#;
    let mut meta_header = tar::Header::new_gnu();
    meta_header.set_path(".metadata.toml")?;
    meta_header.set_size(metadata.len() as u64);
    meta_header.set_mode(0o644);
    meta_header.set_cksum();
    tar.append(&meta_header, &metadata[..])?;

    let encoder = tar.into_inner()?;
    encoder.finish()?;

    let (spec, staged) = load_package_archive_into_staging(&cfg, &archive_path)?;
    assert_eq!(spec.alternatives.replaces, vec!["diffutils".to_string()]);

    let installed = install_package_outputs_to_rootfs(&spec, staged.path(), rootfs.path(), &cfg)?;

    assert_eq!(installed.len(), 1);
    assert!(installed[0].is_update);
    assert_eq!(installed[0].package.name, "vx");
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/bin/diff"))?,
        "vx-diff"
    );

    let db_path = cfg.installed_db_path(rootfs.path());
    assert_eq!(db::get_package_version(&db_path, "diffutils")?, None);
    assert_eq!(
        db::get_package_version(&db_path, "vx")?,
        Some("0.1.0".into())
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
            real_name: None,
            version: "1.0.1".into(),
            revision: 3,
            description: "Base filesystem".into(),
            homepage: "https://example.test".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
fn renamed_abi_updates_keep_versioned_shared_libraries() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.db_dir = rootfs.path().join("var/lib/depot");
    cfg.build_dir = rootfs.path().join("var/cache/depot/build");

    let old_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "libxml214".into(),
            real_name: Some("libxml2".into()),
            version: "2.14.9".into(),
            revision: 1,
            description: "libxml2 2.14".into(),
            homepage: "https://example.test/libxml2".into(),
            abi_breaking: true,
            built_against: Vec::new(),
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
    let old_dest = rootfs.path().join("old-dest");
    fs::create_dir_all(old_dest.join("usr/lib/pkgconfig"))?;
    fs::write(old_dest.join("usr/lib/libxml2.so.14.9.0"), "old-real")?;
    std::os::unix::fs::symlink("libxml2.so.14.9.0", old_dest.join("usr/lib/libxml2.so.14"))?;
    std::os::unix::fs::symlink("libxml2.so.14", old_dest.join("usr/lib/libxml2.so"))?;
    fs::write(
        old_dest.join("usr/lib/pkgconfig/libxml-2.0.pc"),
        "old-pkgconfig",
    )?;
    install_package_outputs_to_rootfs(&old_spec, &old_dest, rootfs.path(), &cfg)?;

    let new_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "libxml215".into(),
            real_name: Some("libxml2".into()),
            version: "2.15.1".into(),
            revision: 1,
            description: "libxml2 2.15".into(),
            homepage: "https://example.test/libxml2".into(),
            abi_breaking: true,
            built_against: Vec::new(),
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
    let new_dest = rootfs.path().join("new-dest");
    fs::create_dir_all(new_dest.join("usr/lib/pkgconfig"))?;
    fs::write(new_dest.join("usr/lib/libxml2.so.15.1.0"), "new-real")?;
    std::os::unix::fs::symlink("libxml2.so.15.1.0", new_dest.join("usr/lib/libxml2.so.15"))?;
    std::os::unix::fs::symlink("libxml2.so.15", new_dest.join("usr/lib/libxml2.so"))?;
    fs::write(
        new_dest.join("usr/lib/pkgconfig/libxml-2.0.pc"),
        "new-pkgconfig",
    )?;

    let installed = install_package_outputs_to_rootfs(&new_spec, &new_dest, rootfs.path(), &cfg)?;
    assert_eq!(installed.len(), 1);
    assert!(installed[0].is_update);
    assert_eq!(installed[0].package.name, "libxml215");

    assert!(rootfs.path().join("usr/lib/libxml2.so.14.9.0").exists());
    assert!(rootfs.path().join("usr/lib/libxml2.so.14").exists());
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/lib/libxml2.so.15.1.0"))?,
        "new-real"
    );
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/lib/pkgconfig/libxml-2.0.pc"))?,
        "new-pkgconfig"
    );

    let db_path = cfg.installed_db_path(rootfs.path());
    let old_files = db::get_package_files(&db_path, "libxml214")?;
    assert_eq!(
        old_files,
        vec![
            "usr/lib/libxml2.so.14".to_string(),
            "usr/lib/libxml2.so.14.9.0".to_string(),
        ]
    );

    let new_files = db::get_package_files(&db_path, "libxml215")?;
    assert!(new_files.contains(&"usr/lib/libxml2.so".to_string()));
    assert!(new_files.contains(&"usr/lib/libxml2.so.15".to_string()));
    assert!(new_files.contains(&"usr/lib/libxml2.so.15.1.0".to_string()));
    assert!(new_files.contains(&"usr/lib/pkgconfig/libxml-2.0.pc".to_string()));
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
fn collect_update_candidates_matches_renamed_packages_by_real_name() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let rootfs = temp.path().join("rootfs");
    let repo_clones = temp.path().join("repos");
    let build_dir = temp.path().join("build");
    let db_dir = rootfs.join("var/lib/depot");
    fs::create_dir_all(&rootfs)?;
    fs::create_dir_all(&repo_clones)?;
    fs::create_dir_all(&build_dir)?;
    fs::create_dir_all(&db_dir)?;

    let mut config = config::Config::for_rootfs(&rootfs);
    config.repo_clone_dir = repo_clones.clone();
    config.build_dir = build_dir;
    config.db_dir = db_dir.clone();
    config.repo_settings.prefer_binary = false;
    config.binary_repos.clear();
    config.source_repos.clear();
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
            name: "icu78".into(),
            real_name: Some("icu".into()),
            version: "78.2".into(),
            revision: 1,
            description: "icu78".into(),
            homepage: "https://example.test/icu".into(),
            abi_breaking: true,
            built_against: Vec::new(),
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
    fs::write(dest.join("usr/lib/libicuuc.so.78.2"), "icu78")?;
    db::register_package(&config.installed_db_path(&rootfs), &installed_spec, &dest)?;

    let repo_root = repo_clones.join("private");
    fs::create_dir_all(&repo_root)?;
    fs::write(
        repo_root.join("icu79.toml"),
        r#"[package]
name = "icu79"
real_name = "icu"
version = "79.1"
revision = 1
description = "icu79"
homepage = "https://example.test/icu"
abi_breaking = true
license = "MIT"

[build]
type = "meta"

[dependencies]
runtime = []
optional = []
"#,
    )?;

    let installed_records = db::list_installed_package_records(&config.installed_db_path(&rootfs))?;
    assert_eq!(installed_records.len(), 1);
    assert_eq!(installed_records[0].real_name.as_deref(), Some("icu"));

    let source_candidates =
        collect_best_source_update_candidates(&config, &HashSet::from([String::from("icu")]))?;
    assert!(source_candidates.contains_key("icu"));
    let selected = select_update_candidate(
        &installed_records[0],
        installed_records[0].completed_at,
        &HashMap::new(),
        &HashMap::new(),
        &source_candidates,
        &HashMap::new(),
        false,
    );
    assert!(selected.is_some());

    let updates = collect_update_candidates(&config, &rootfs, &["icu78".into()])?;
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].installed_package, "icu78");
    assert_eq!(updates[0].candidate_package, "icu79");
    assert_eq!(updates[0].candidate_version, "79.1");
    Ok(())
}

#[test]
fn update_candidate_prefers_binary_when_versions_match_and_config_does() {
    let installed = db::InstalledPackageRecord {
        name: "pkg".into(),
        real_name: None,
        version: "1.0.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: None,
    };
    let source_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "pkg".into(),
            real_name: None,
            version: "1.1.0".into(),
            revision: 1,
            description: "test".into(),
            homepage: "https://example.test".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
                real_name: None,
                version: "1.1.0".into(),
                revision: 1,
                abi_breaking: false,
                built_against: Vec::new(),
                completed_at: None,
                filename: "pkg-1.1.0-1-x86_64.depot.pkg.tar.zst".into(),
                size: 1,
                sha512: String::new(),
                description: None,
                homepage: None,
                license: None,
                provides: Vec::new(),
                conflicts: Vec::new(),
                replaces: Vec::new(),
                runtime_dependencies: Vec::new(),
                optional_dependencies: Vec::new(),
                groups: Vec::new(),
            },
        ),
    )]);

    let selected = select_update_candidate(
        &installed,
        None,
        &HashMap::new(),
        &HashMap::new(),
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
        real_name: None,
        version: "1.0.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: Some(100),
    };
    let source_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "pkg".into(),
            real_name: None,
            version: "1.0.0".into(),
            revision: 1,
            description: "test".into(),
            homepage: "https://example.test".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
        &HashMap::new(),
        &HashMap::new(),
        &source_candidates,
        &HashMap::new(),
        true,
    )
    .expect("expected update candidate");
    assert_eq!(selected.candidate_version, "1.0.0");
    assert_eq!(selected.candidate_completed_at, Some(200));
}

#[test]
fn select_update_candidate_prefers_replacement_candidate() {
    let installed = db::InstalledPackageRecord {
        name: "findutils".into(),
        real_name: None,
        version: "4.9.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: Some(100),
    };

    let source_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "findutils".into(),
            real_name: None,
            version: "5.0.0".into(),
            revision: 1,
            description: "findutils".into(),
            homepage: "https://example.test/findutils".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
    let replacement_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "busybox".into(),
            real_name: None,
            version: "1.36.1".into(),
            revision: 1,
            description: "busybox".into(),
            homepage: "https://example.test/busybox".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["GPL-2.0-only".into()],
        },
        packages: Vec::new(),
        alternatives: package::Alternatives {
            provides: Vec::new(),
            conflicts: Vec::new(),
            replaces: vec!["findutils".into()],
            lib32: None,
        },
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
        "findutils".to_string(),
        SourceUpdateCandidate {
            repo_name: "source".into(),
            repo_priority: 5,
            path: PathBuf::from("/tmp/findutils.toml"),
            completed_at: Some(200),
            spec: source_spec,
        },
    )]);
    let source_replacement_candidates = HashMap::from([(
        "findutils".to_string(),
        SourceUpdateCandidate {
            repo_name: "source".into(),
            repo_priority: 0,
            path: PathBuf::from("/tmp/busybox.toml"),
            completed_at: Some(150),
            spec: replacement_spec,
        },
    )]);

    let selected = select_update_candidate(
        &installed,
        installed.completed_at,
        &source_replacement_candidates,
        &HashMap::new(),
        &source_candidates,
        &HashMap::new(),
        false,
    )
    .expect("expected replacement update candidate");

    assert!(selected.replaces_installed);
    assert_eq!(selected.installed_package, "findutils");
    assert_eq!(selected.candidate_package, "busybox");
}

#[test]
fn install_planned_packages_to_rootfs_runs_post_hooks_after_batch_install() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.db_dir = rootfs.path().join("var/lib/depot");
    cfg.build_dir = rootfs.path().join("var/cache/depot/build");

    let old_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "findutils".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "findutils".into(),
            homepage: "https://example.test/findutils".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
    let old_dest = rootfs.path().join("old-dest");
    fs::create_dir_all(old_dest.join("usr/bin"))?;
    fs::write(old_dest.join("usr/bin/find"), "old-find")?;
    install_package_outputs_to_rootfs(&old_spec, &old_dest, rootfs.path(), &cfg)?;

    let alpha_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "alpha".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "alpha".into(),
            homepage: "https://example.test/alpha".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
    let alpha_dest = rootfs.path().join("alpha-dest");
    fs::create_dir_all(alpha_dest.join("usr/bin"))?;
    fs::create_dir_all(alpha_dest.join("scripts"))?;
    fs::write(alpha_dest.join("usr/bin/alpha"), "alpha")?;
    fs::write(
        alpha_dest.join("scripts/post_install"),
        "cat \"$DEPOT_ROOTFS/usr/bin/find\" > \"$DEPOT_ROOTFS/alpha-marker\"\n",
    )?;

    let replacement_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "busybox".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "busybox".into(),
            homepage: "https://example.test/busybox".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["GPL-2.0-only".into()],
        },
        packages: Vec::new(),
        alternatives: package::Alternatives {
            provides: Vec::new(),
            conflicts: Vec::new(),
            replaces: vec!["findutils".into()],
            lib32: None,
        },
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
    let replacement_dest = rootfs.path().join("replacement-dest");
    fs::create_dir_all(replacement_dest.join("usr/bin"))?;
    fs::write(replacement_dest.join("usr/bin/find"), "new-find")?;

    let mut plans = Vec::new();
    plans.extend(plan_package_outputs_for_install(
        &alpha_spec,
        &alpha_dest,
        rootfs.path(),
        &cfg,
    )?);
    plans.extend(plan_package_outputs_for_install(
        &replacement_spec,
        &replacement_dest,
        rootfs.path(),
        &cfg,
    )?);

    install_planned_packages_to_rootfs(&plans, rootfs.path(), &cfg)?;

    assert_eq!(
        fs::read_to_string(rootfs.path().join("alpha-marker"))?,
        "new-find"
    );
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/bin/find"))?,
        "new-find"
    );
    assert!(db::get_package_version(&cfg.installed_db_path(rootfs.path()), "findutils")?.is_none());
    assert_eq!(
        db::get_package_version(&cfg.installed_db_path(rootfs.path()), "busybox")?,
        Some("1.0".into())
    );
    Ok(())
}

#[test]
fn install_planned_packages_sets_sole_tool_provider_before_post_hooks() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let mut config = config::Config::for_rootfs(rootfs.path());
    config.db_dir = rootfs.path().join("var/lib/depot");
    config.build_dir = rootfs.path().join("var/cache/depot/build");

    let mut dash_spec = test_package_spec(package::BuildType::Bin, None, &[]);
    dash_spec.package.name = "dash".into();
    let dash_dest = rootfs.path().join("dash-dest");
    fs::create_dir_all(dash_dest.join("usr/bin"))?;
    fs::create_dir_all(dash_dest.join("scripts"))?;
    fs::write(dash_dest.join("usr/bin/dash"), "dash")?;
    fs::write(
        dash_dest.join("scripts/post_install"),
        "[ -L \"$DEPOT_ROOTFS/usr/bin/sh\" ] && [ \"$(readlink \"$DEPOT_ROOTFS/usr/bin/sh\")\" = dash ]\n",
    )?;

    let plans = plan_package_outputs_for_install(&dash_spec, &dash_dest, rootfs.path(), &config)?;
    install_planned_packages_to_rootfs(&plans, rootfs.path(), &config)?;

    assert_eq!(
        fs::read_link(rootfs.path().join("usr/bin/sh"))?,
        PathBuf::from("dash")
    );
    Ok(())
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
    config.source_repos.clear();
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
            real_name: None,
            version: "1.0.0".into(),
            revision: 1,
            description: "pkg".into(),
            homepage: "https://example.test".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
fn explicit_depot_self_update_request_requires_only_depot() {
    assert!(is_explicit_depot_self_update_request(&[
        DEPOT_PACKAGE_NAME.to_string()
    ]));
    assert!(!is_explicit_depot_self_update_request(&[]));
    assert!(!is_explicit_depot_self_update_request(&["pkg".to_string()]));
    assert!(!is_explicit_depot_self_update_request(&[
        DEPOT_PACKAGE_NAME.to_string(),
        "pkg".to_string()
    ]));
}

#[test]
fn depot_self_update_check_blocks_when_update_is_available() -> Result<()> {
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
    config.db_dir = db_dir;
    config.repo_settings.prefer_binary = false;
    config.binary_repos.clear();
    config.source_repos.clear();
    config.source_repos.insert(
        "core".into(),
        config::SourceRepo {
            url: "https://example.test/core.git".into(),
            enabled: true,
            priority: 0,
            subdirs: Vec::new(),
        },
    );

    register_installed_test_package(&config, &rootfs, DEPOT_PACKAGE_NAME, "1.0.0")?;
    write_test_repo_spec(
        &repo_clones.join("core").join("depot.toml"),
        DEPOT_PACKAGE_NAME,
        "1.1.0",
    )?;

    let err = ensure_depot_self_update_not_required(&config, &rootfs)
        .expect_err("outdated depot should block command execution");
    assert!(err.to_string().contains("update depot"));
    Ok(())
}

#[test]
fn depot_self_update_check_allows_when_depot_is_current() -> Result<()> {
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
    config.db_dir = db_dir;
    config.repo_settings.prefer_binary = false;
    config.binary_repos.clear();
    config.source_repos.clear();
    config.source_repos.insert(
        "core".into(),
        config::SourceRepo {
            url: "https://example.test/core.git".into(),
            enabled: true,
            priority: 0,
            subdirs: Vec::new(),
        },
    );

    let repo_spec = repo_clones.join("core").join("depot.toml");
    register_installed_test_package(&config, &rootfs, DEPOT_PACKAGE_NAME, "1.1.0")?;
    write_test_repo_spec(&repo_spec, DEPOT_PACKAGE_NAME, "1.1.0")?;
    let repo_completed_at =
        crate::metadata_time::system_time_to_unix(fs::metadata(&repo_spec)?.modified()?)?;
    set_installed_test_package_completed_at(
        &config,
        &rootfs,
        DEPOT_PACKAGE_NAME,
        repo_completed_at + 1,
    )?;

    ensure_depot_self_update_not_required(&config, &rootfs)?;
    Ok(())
}

#[test]
fn depot_self_update_check_is_skipped_for_nested_update_install_context() -> Result<()> {
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
    config.db_dir = db_dir;
    config.repo_settings.prefer_binary = false;
    config.binary_repos.clear();
    config.source_repos.clear();
    config.source_repos.insert(
        "core".into(),
        config::SourceRepo {
            url: "https://example.test/core.git".into(),
            enabled: true,
            priority: 0,
            subdirs: Vec::new(),
        },
    );

    register_installed_test_package(&config, &rootfs, DEPOT_PACKAGE_NAME, "1.0.0")?;
    write_test_repo_spec(
        &repo_clones.join("core").join("depot.toml"),
        DEPOT_PACKAGE_NAME,
        "1.1.0",
    )?;

    let mut env = TestEnv::new();
    env.set_var(DEPOT_INSTALL_CONTEXT_ENV, INSTALL_CONTEXT_UPDATE);

    ensure_depot_self_update_not_required(&config, &rootfs)?;
    Ok(())
}

#[test]
fn collect_missing_update_dependencies_skips_planned_provides_and_installed_deps() -> Result<()> {
    let temp = tempfile::tempdir().context("Failed to create temp dir")?;
    let db_path = temp.path().join("packages.db");

    let libc_spec = package::PackageSpec {
        package: package::PackageInfo {
            name: "glibc".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "glibc".into(),
            homepage: "https://example.test".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
                installed_package: "pkg".into(),
                candidate_package: "pkg".into(),
                replaces_installed: false,
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
                installed_package: "helper".into(),
                candidate_package: "helper".into(),
                replaces_installed: false,
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
                installed_package: "tool".into(),
                candidate_package: "tool".into(),
                replaces_installed: false,
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
    assert_eq!(
        compare_versions_for_updates("v1.0.0", "1.0.0"),
        Ordering::Equal
    );
    assert_eq!(
        compare_versions_for_updates("lts_2027_01_01", "20260107.1"),
        Ordering::Greater
    );
}

#[test]
fn compare_versions_for_updates_is_transitive_for_mixed_formats() {
    let versions = [
        "01",
        "1a",
        "1.0.0",
        "1.2.0",
        "1.2.0rc2",
        "v1.0.0",
        "1.0.0+meta",
        "20260107.1",
        "lts_2026_01_07",
    ];

    for left in versions {
        for middle in versions {
            for right in versions {
                let left_middle = compare_versions_for_updates(left, middle);
                let middle_right = compare_versions_for_updates(middle, right);
                let left_right = compare_versions_for_updates(left, right);

                if left_middle == Ordering::Less && middle_right == Ordering::Less {
                    assert_eq!(
                        left_right,
                        Ordering::Less,
                        "expected transitive ordering for {left} < {middle} < {right}"
                    );
                }

                if left_middle == Ordering::Greater && middle_right == Ordering::Greater {
                    assert_eq!(
                        left_right,
                        Ordering::Greater,
                        "expected transitive ordering for {left} > {middle} > {right}"
                    );
                }

                if left_middle == Ordering::Equal && middle_right == Ordering::Equal {
                    assert_eq!(
                        left_right,
                        Ordering::Equal,
                        "expected transitive equality for {left} == {middle} == {right}"
                    );
                }
            }
        }
    }
}

#[test]
fn extract_version_patterns_handles_git_and_release_urls() {
    let git_patterns = extract_version_patterns("https://codeberg.org/Limine/limine.git#v$version");
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
fn best_newer_version_skips_branches_and_prereleases() {
    let candidates = ["2", "1.10.0rc1", "1.10.0", "release-0.13"];
    assert_eq!(
        best_newer_version("1.9.5", candidates.into_iter()),
        Some("1.10.0".to_string())
    );
}

#[test]
fn best_newer_version_normalizes_date_style_tags() {
    let candidates = ["lts_2026_01_07", "lts_2027_02_03"];
    assert_eq!(
        best_newer_version("20260107.1", candidates.into_iter()),
        Some("20270203".to_string())
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
fn remote_git_repository_from_gitlab_archive_url_maps_to_repo_git_url() {
    let repo_url = remote_git_repository_from_source_url(
        "https://gitlab.com/graphviz/graphviz/-/archive/14.1.4/graphviz-14.1.4.tar.gz",
    );
    assert_eq!(
        repo_url,
        Some("https://gitlab.com/graphviz/graphviz.git".to_string())
    );
}

#[test]
fn archive_listing_probe_uses_parent_of_first_version_segment() {
    let probe = archive_listing_probe(
        "https://downloads.example.test/dav1d/$version/dav1d-$version.tar.xz",
        "https://downloads.example.test/dav1d/1.5.3/dav1d-1.5.3.tar.xz",
    )
    .expect("archive probe");
    assert_eq!(probe.listing_url, "https://downloads.example.test/dav1d/");
    assert_eq!(
        probe.patterns,
        vec![VersionPattern {
            prefix: String::new(),
            suffix: String::new(),
        }]
    );
}

#[test]
fn candidate_versions_from_listing_matches_archive_entries() {
    let patterns = vec![VersionPattern {
        prefix: "alsa-lib-".into(),
        suffix: ".tar.bz2".into(),
    }];
    let html = r#"
            <a href="alsa-lib-1.2.15.3.tar.bz2">alsa-lib-1.2.15.3.tar.bz2</a>
            <a href="alsa-lib-1.2.16.tar.bz2">alsa-lib-1.2.16.tar.bz2</a>
        "#;
    assert_eq!(
        candidate_versions_from_listing(html, &patterns),
        vec!["1.2.15.3".to_string(), "1.2.16".to_string()]
    );
}

#[test]
fn list_archive_versions_reads_simple_http_index() -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    let listener = TcpListener::bind("127.0.0.1:0").context("bind test listener")?;
    let addr = listener.local_addr().context("listener addr")?;
    let server = thread::spawn(move || -> Result<()> {
        let (mut stream, _) = listener.accept().context("accept request")?;
        let mut reader = BufReader::new(stream.try_clone().context("clone stream")?);
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .context("read request line")?;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).context("read header line")?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        assert!(request_line.starts_with("GET /pub/lib/ HTTP/1.1"));
        let body = r#"
                <html>
                    <a href="alsa-lib-1.2.15.3.tar.bz2">alsa-lib-1.2.15.3.tar.bz2</a>
                    <a href="alsa-lib-1.2.16.tar.bz2">alsa-lib-1.2.16.tar.bz2</a>
                </html>
            "#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .context("write response")?;
        stream.flush().context("flush response")?;
        Ok(())
    });

    let probe = ArchiveListingProbe {
        listing_url: format!("http://{addr}/pub/lib/"),
        patterns: vec![VersionPattern {
            prefix: "alsa-lib-".into(),
            suffix: ".tar.bz2".into(),
        }],
    };
    let versions = list_archive_versions(&probe)?;
    server.join().expect("join server")?;
    assert_eq!(versions, vec!["1.2.15.3".to_string(), "1.2.16".to_string()]);
    Ok(())
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
            lib32_only: false,
            install_test_deps: true,
            install_context: None,
            dep_chain: Some("parent"),
        },
    )?;

    let captured_args = fs::read_to_string(&args_path)
        .with_context(|| format!("Failed to read {}", args_path.display()))?;
    assert_eq!(
        captured_args.lines().collect::<Vec<_>>(),
        vec![
            "install",
            "-r",
            "/",
            "--no-flags",
            "--cross-prefix",
            "x86_64-linux-musl",
            "--clean",
            "--test-deps",
            "/tmp/pkg-a.toml",
            "/tmp/pkg-b.toml",
        ]
    );
    assert_eq!(fs::read_to_string(&env_path)?, "parent");
    Ok(())
}

#[test]
#[cfg(unix)]
fn child_install_command_includes_lib32_only_flag_when_requested() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().context("Failed to create temp dir")?;
    let script_path = temp.path().join("capture-lib32-child-install.sh");
    let args_path = temp.path().join("args.txt");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
        args_path.display()
    );
    fs::write(&script_path, script)
        .with_context(|| format!("Failed to write {}", script_path.display()))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Failed to chmod {}", script_path.display()))?;

    run_install_command_with_program(
        &script_path,
        &[PathBuf::from("/tmp/pkg.toml")],
        Path::new("/"),
        ChildInstallCommandOptions {
            no_deps: true,
            assume_yes: true,
            no_flags: false,
            cross_prefix: None,
            clean: false,
            lib32_only: true,
            install_test_deps: false,
            install_context: None,
            dep_chain: None,
        },
    )?;

    let captured_args = fs::read_to_string(&args_path)
        .with_context(|| format!("Failed to read {}", args_path.display()))?;
    assert!(captured_args.lines().any(|line| line == "--lib32-only"));
    Ok(())
}

#[test]
#[cfg(unix)]
fn child_install_command_propagates_install_context_env() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().context("Failed to create temp dir")?;
    let script_path = temp.path().join("capture-child-install-context.sh");
    let env_path = temp.path().join("context.txt");
    let script = format!(
        "#!/bin/sh\nprintf '%s' \"${{{}:-}}\" > \"{}\"\n",
        DEPOT_INSTALL_CONTEXT_ENV,
        env_path.display()
    );
    fs::write(&script_path, script)
        .with_context(|| format!("Failed to write {}", script_path.display()))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Failed to chmod {}", script_path.display()))?;

    run_install_command_with_program(
        &script_path,
        &[PathBuf::from("/tmp/pkg.toml")],
        Path::new("/"),
        ChildInstallCommandOptions {
            no_deps: true,
            assume_yes: true,
            no_flags: false,
            cross_prefix: None,
            clean: false,
            lib32_only: false,
            install_test_deps: false,
            install_context: Some(INSTALL_CONTEXT_UPDATE),
            dep_chain: None,
        },
    )?;

    assert_eq!(fs::read_to_string(&env_path)?, INSTALL_CONTEXT_UPDATE);
    Ok(())
}

#[test]
fn direct_install_checks_manual_sources_before_dependency_resolution() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let spec_dir = tempfile::tempdir().context("Failed to create temp spec dir")?;
    let spec_path = spec_dir.path().join("demo.toml");
    fs::write(
        &spec_path,
        r#"[package]
name = "demo"
version = "1.0.0"
revision = 1
description = "demo"
homepage = "https://example.test/demo"
license = "MIT"

[build]
type = "custom"

[dependencies]
runtime = ["definitely-missing-dep"]
optional = []

[[manual_sources]]
file = "missing.patch"
"#,
    )?;

    let mut config = config::Config::for_rootfs(rootfs.path());
    config.build_dir = rootfs.path().join("var/cache/depot/build");
    config.cache_dir = rootfs.path().join("var/cache/depot/sources");
    config.db_dir = rootfs.path().join("var/lib/depot");

    ui::set_assume_yes(true);
    let result = run_direct_install_request(
        DirectInstallOptions {
            rootfs: rootfs.path(),
            no_deps: false,
            no_flags: false,
            cross_prefix: None,
            clean: false,
            dry_run: false,
            lib32_only: false,
            install_test_deps: false,
        },
        &config,
        spec_path,
    );
    ui::set_assume_yes(false);

    let err = result.expect_err("missing manual source should fail before dependency install");
    assert!(
        err.to_string()
            .contains("Manual source not found: missing.patch")
    );
    assert!(
        !err.to_string()
            .contains("Could not find package spec for dependency")
    );
    Ok(())
}

#[test]
fn build_command_checks_manual_sources_before_dependency_resolution() -> Result<()> {
    let _guard = assume_yes_test_lock();
    let temp = tempfile::tempdir().context("Failed to create temp dir")?;
    let rootfs = temp.path().join("rootfs");
    let spec_dir = temp.path().join("packages").join("demo");
    fs::create_dir_all(&rootfs)?;
    fs::create_dir_all(&spec_dir)?;

    let spec_path = spec_dir.join("demo.toml");
    fs::write(
        &spec_path,
        r#"[package]
name = "demo"
version = "1.0.0"
revision = 1
description = "demo"
homepage = "https://example.test/demo"
license = "MIT"

[[source]]
url = "https://example.test/demo-1.0.0.tar.gz"
sha256 = "skip"
extract_dir = "demo-1.0.0"

[build]
type = "custom"

[dependencies]
build = ["definitely-missing-dep"]
runtime = []
optional = []

[[manual_sources]]
file = "missing.patch"
"#,
    )?;

    let result = run(Cli {
        command: Commands::Build(BuildArgs {
            rootfs_args: rootfs_args(rootfs),
            prompt_args: prompt_args(true),
            build_exec_args: build_exec_args(),
            lib32_args: lib32_args(),
            spec_pos: Some(spec_path),
            spec: None,
            install: false,
            install_deps: true,
            cleanup_deps: false,
        }),
    });

    let err = result.expect_err("missing manual source should fail before dependency install");
    assert!(
        err.to_string()
            .contains("Manual source not found: missing.patch")
    );
    assert!(
        !err.to_string()
            .contains("Failed to resolve required build tool package")
    );
    Ok(())
}

#[test]
fn source_build_warning_messages_include_dependency_context() {
    let plan = planner::ExecutionPlan {
        steps: vec![
            planner::PlannedStep {
                package: "dep-src".into(),
                action: planner::PlanAction::BuildAndInstall,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("/tmp/dep-src.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["dependency dep-src".into(), "app needs dep-src".into()],
            },
            planner::PlannedStep {
                package: "dep-bin".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Binary {
                    repo_name: "core".into(),
                    record: Box::new(test_binary_repo_record(
                        "dep-bin",
                        "dep-bin-1.0-1-x86_64.tar.zst",
                    )),
                },
                requested_by: vec!["app needs dep-bin".into()],
            },
        ],
    };

    assert_eq!(
        source_build_warning_messages(&plan),
        vec!["dep-src (requested dependency 'dep-src', needed by 'app')".to_string()]
    );
}

#[test]
fn planned_source_build_prereqs_check_manual_sources_before_confirmation() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let spec_dir = tempfile::tempdir().context("Failed to create temp spec dir")?;
    let spec_path = spec_dir.path().join("demo.toml");
    fs::write(
        &spec_path,
        r#"[package]
name = "demo"
version = "1.0.0"
revision = 1
description = "demo"
homepage = "https://example.test/demo"
license = "MIT"

[[source]]
url = "https://example.test/demo-1.0.0.tar.gz"
sha256 = "skip"
extract_dir = "demo-1.0.0"

[build]
type = "custom"

[dependencies]
build = []
runtime = []
optional = []

[[manual_sources]]
file = "missing.patch"
"#,
    )?;

    let config = config::Config::for_rootfs(rootfs.path());
    let plan = planner::ExecutionPlan {
        steps: vec![planner::PlannedStep {
            package: "demo".into(),
            action: planner::PlanAction::BuildAndInstall,
            origin: planner::PlanOrigin::Source {
                path: spec_path,
                local_sibling: true,
            },
            requested_by: vec!["requested spec".into()],
        }],
    };

    let err = validate_source_build_prereqs_for_plan(&plan, rootfs.path(), &config)
        .expect_err("missing local manual source should fail before confirmation");
    assert!(
        err.to_string()
            .contains("Manual source not found: missing.patch")
    );
    Ok(())
}

#[test]
fn suppress_nested_install_output_for_planned_context() {
    let mut env = TestEnv::new();
    env.set_var(DEPOT_INSTALL_CONTEXT_ENV, INSTALL_CONTEXT_PLANNED);

    assert!(suppress_nested_install_output());
    assert_eq!(
        current_install_invocation_context(),
        InstallInvocationContext::Planned
    );
}

#[test]
fn sudo_preserve_env_arg_only_includes_present_depot_env_vars() {
    let mut env = TestEnv::new();
    assert_eq!(sudo_preserve_env_arg(), None);

    env.set_var(DEPOT_INSTALL_CONTEXT_ENV, INSTALL_CONTEXT_PLANNED);
    assert_eq!(
        sudo_preserve_env_arg(),
        Some(format!("--preserve-env={}", DEPOT_INSTALL_CONTEXT_ENV))
    );

    env.set_var("DEPOT_DEPCHAIN", "parent");
    assert_eq!(
        sudo_preserve_env_arg(),
        Some(format!(
            "--preserve-env={},DEPOT_DEPCHAIN",
            DEPOT_INSTALL_CONTEXT_ENV
        ))
    );
}

#[test]
fn plan_dependency_closure_tracks_requested_dependency_roots() {
    let plan = planner::ExecutionPlan {
        steps: vec![
            planner::PlannedStep {
                package: "zlib".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("packages/core/zlib/zlib.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["cmake needs zlib".into()],
            },
            planner::PlannedStep {
                package: "cmake".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("packages/core/cmake/cmake.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["dependency cmake".into()],
            },
            planner::PlannedStep {
                package: "libffi".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("packages/core/libffi/libffi.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["python needs libffi".into()],
            },
            planner::PlannedStep {
                package: "python".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("packages/core/python/python.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["dependency python".into()],
            },
        ],
    };

    let cmake_closure = plan_dependency_closure_for_requested_deps(&plan, &["cmake".into()]);
    assert_eq!(
        cmake_closure,
        HashSet::from(["cmake".to_string(), "zlib".to_string()])
    );

    let python_closure = plan_dependency_closure_for_requested_deps(&plan, &["python".into()]);
    assert_eq!(
        python_closure,
        HashSet::from(["python".to_string(), "libffi".to_string()])
    );
}

#[test]
fn cleanup_targets_keep_runtime_dependencies_for_build_install() {
    let plan = planner::ExecutionPlan {
        steps: vec![
            planner::PlannedStep {
                package: "zlib".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("packages/core/zlib/zlib.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["cmake needs zlib".into(), "llvm-runtime needs zlib".into()],
            },
            planner::PlannedStep {
                package: "cmake".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("packages/core/cmake/cmake.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["dependency cmake".into()],
            },
            planner::PlannedStep {
                package: "llvm-runtime".into(),
                action: planner::PlanAction::InstallBinary,
                origin: planner::PlanOrigin::Source {
                    path: PathBuf::from("packages/core/llvm-runtime/llvm-runtime.toml"),
                    local_sibling: false,
                },
                requested_by: vec!["dependency llvm-runtime".into()],
            },
        ],
    };
    let mut tracker = AutoInstalledDependencyTracker::default();
    tracker.record_plan(&plan, &["cmake".into()], AutoInstalledDependencyKind::Build);
    tracker.record_plan(
        &plan,
        &["llvm-runtime".into()],
        AutoInstalledDependencyKind::Runtime,
    );

    assert_eq!(tracker.cleanup_targets(false), vec!["cmake".to_string()]);
    assert_eq!(
        tracker.cleanup_targets(true),
        vec![
            "llvm-runtime".to_string(),
            "cmake".to_string(),
            "zlib".to_string()
        ]
    );
}

#[test]
fn build_type_runs_automatic_tests_matches_builder_behavior() {
    assert!(build_type_runs_automatic_tests(&test_package_spec(
        package::BuildType::Autotools,
        None,
        &[]
    )));
    assert!(build_type_runs_automatic_tests(&test_package_spec(
        package::BuildType::Perl,
        None,
        &[]
    )));
    assert!(build_type_runs_automatic_tests(&test_package_spec(
        package::BuildType::Meson,
        None,
        &[]
    )));
    assert!(build_type_runs_automatic_tests(&test_package_spec(
        package::BuildType::CMake,
        None,
        &[]
    )));
}

#[test]
fn requested_test_deps_prompt_can_disable_tests() -> Result<()> {
    let _guard = assume_yes_test_lock();
    let mut spec = test_package_spec(package::BuildType::Meson, None, &[]);
    spec.dependencies.test = vec!["pytest".into()];

    ui::set_assume_yes(true);
    let prompted = maybe_prompt_to_skip_tests_for_missing_requested_deps(
        &mut spec,
        &["pytest".into()],
        "Requested test dependencies are missing",
    )?;
    ui::set_assume_yes(false);

    assert!(prompted);
    assert!(spec.build.flags.skip_tests);
    Ok(())
}

#[test]
fn requested_test_deps_prompt_is_ignored_for_non_automatic_test_builders() -> Result<()> {
    let _guard = assume_yes_test_lock();
    let mut spec = test_package_spec(package::BuildType::Custom, None, &[]);
    spec.dependencies.test = vec!["pytest".into()];

    ui::set_assume_yes(true);
    let prompted = maybe_prompt_to_skip_tests_for_missing_requested_deps(
        &mut spec,
        &["pytest".into()],
        "Requested test dependencies are missing",
    )?;
    ui::set_assume_yes(false);

    assert!(!prompted);
    assert!(!spec.build.flags.skip_tests);
    Ok(())
}

#[test]
fn requested_test_deps_prompt_is_ignored_for_multilib_builds() -> Result<()> {
    let _guard = assume_yes_test_lock();
    let mut spec = test_package_spec(package::BuildType::Meson, None, &[]);
    spec.build.flags.build_32 = true;
    spec.dependencies.test = vec!["pytest".into()];

    ui::set_assume_yes(true);
    let prompted = maybe_prompt_to_skip_tests_for_missing_requested_deps(
        &mut spec,
        &["pytest".into()],
        "Requested test dependencies are missing",
    )?;
    ui::set_assume_yes(false);

    assert!(!prompted);
    assert!(!spec.build.flags.skip_tests);
    Ok(())
}

#[test]
fn should_not_install_test_deps_for_cli_lib32_only_builds() {
    let mut spec = test_package_spec(package::BuildType::Meson, None, &[]);
    spec.dependencies.lib32 = Some(package::DependencyGroup {
        build: Vec::new(),
        runtime: Vec::new(),
        test: vec!["lib32-pytest".into()],
        optional: Vec::new(),
        groups: Vec::new(),
    });

    assert!(!should_install_test_deps(
        &spec,
        true,
        deps::RequestedOutputs::Lib32Only
    ));
}

#[test]
fn rootfs_is_system_root_detects_live_rootfs() {
    assert!(rootfs_is_system_root(Path::new("/")));
    assert!(!rootfs_is_system_root(Path::new("/tmp/depot-test-rootfs")));
}

#[test]
fn command_requires_live_root_for_install_remove_and_update() {
    assert!(command_requires_live_root(&Commands::Install(
        InstallArgs {
            rootfs_args: rootfs_args("/"),
            prompt_args: prompt_args(false),
            build_exec_args: build_exec_args(),
            lib32_args: lib32_args(),
            spec_or_archive: vec![PathBuf::from("foo")],
            spec: None,
        }
    )));
    assert!(command_requires_live_root(&Commands::Remove(RemoveArgs {
        rootfs_args: rootfs_args("/"),
        prompt_args: prompt_args(false),
        package: "foo".to_string(),
    })));
    assert!(command_requires_live_root(&Commands::Update(UpdateArgs {
        rootfs_args: rootfs_args("/"),
        prompt_args: prompt_args(false),
        build_exec_args: build_exec_args(),
        packages: vec!["foo".to_string()],
    })));
    assert!(!command_requires_live_root(&Commands::Build(BuildArgs {
        rootfs_args: rootfs_args("/"),
        prompt_args: prompt_args(false),
        build_exec_args: build_exec_args(),
        lib32_args: lib32_args(),
        spec_pos: Some(PathBuf::from("foo.toml")),
        spec: None,
        install: false,
        install_deps: false,
        cleanup_deps: false,
    })));
    assert!(!command_requires_live_root(&Commands::Search(SearchArgs {
        rootfs_args: rootfs_args("/"),
        query: "foo".to_string(),
        files: false,
    })));
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

#[test]
fn live_rootfs_child_install_batches_group_consecutive_binary_steps() -> Result<()> {
    let source_path = PathBuf::from("/tmp/requested.toml");
    let expat_archive = PathBuf::from("/tmp/expat.pkg.tar.zst");
    let python_archive = PathBuf::from("/tmp/python.pkg.tar.zst");
    let compiler_rt_archive = PathBuf::from("/tmp/lib32-compiler-rt.pkg.tar.zst");

    let expat_record = test_binary_repo_record("expat", "expat-1.0-1-x86_64.depot.pkg.tar.zst");
    let python_record = test_binary_repo_record("python", "python-1.0-1-x86_64.depot.pkg.tar.zst");
    let compiler_rt_record = test_binary_repo_record(
        "lib32-compiler-rt",
        "lib32-compiler-rt-1.0-1-x86_64.depot.pkg.tar.zst",
    );

    let steps = [
        planner::PlannedStep {
            package: "expat".into(),
            action: planner::PlanAction::InstallBinary,
            origin: planner::PlanOrigin::Binary {
                repo_name: "core".into(),
                record: Box::new(expat_record.clone()),
            },
            requested_by: vec!["pkg needs expat".into()],
        },
        planner::PlannedStep {
            package: "python".into(),
            action: planner::PlanAction::InstallBinary,
            origin: planner::PlanOrigin::Binary {
                repo_name: "core".into(),
                record: Box::new(python_record.clone()),
            },
            requested_by: vec!["pkg needs python".into()],
        },
        planner::PlannedStep {
            package: "pkg".into(),
            action: planner::PlanAction::BuildAndInstall,
            origin: planner::PlanOrigin::Source {
                path: source_path.clone(),
                local_sibling: false,
            },
            requested_by: vec!["requested spec".into()],
        },
        planner::PlannedStep {
            package: "lib32-compiler-rt".into(),
            action: planner::PlanAction::InstallBinary,
            origin: planner::PlanOrigin::Binary {
                repo_name: "core".into(),
                record: Box::new(compiler_rt_record.clone()),
            },
            requested_by: vec!["pkg needs lib32-compiler-rt".into()],
        },
    ];
    let step_refs = steps.iter().collect::<Vec<_>>();

    let mut binary_archives = HashMap::new();
    binary_archives.insert(
        ("core".to_string(), expat_record.filename.clone()),
        db::repo::BinaryRepoCachedArchive {
            package_path: expat_archive.clone(),
            signature_path: PathBuf::from("/tmp/expat.sig"),
        },
    );
    binary_archives.insert(
        ("core".to_string(), python_record.filename.clone()),
        db::repo::BinaryRepoCachedArchive {
            package_path: python_archive.clone(),
            signature_path: PathBuf::from("/tmp/python.sig"),
        },
    );
    binary_archives.insert(
        ("core".to_string(), compiler_rt_record.filename.clone()),
        db::repo::BinaryRepoCachedArchive {
            package_path: compiler_rt_archive.clone(),
            signature_path: PathBuf::from("/tmp/lib32-compiler-rt.sig"),
        },
    );

    let options = InstallPlanExecutionOptions {
        no_flags: false,
        cross_prefix: None,
        clean: false,
        dry_run: false,
        confirm_installation: false,
        lib32_only_requested_specs: true,
        install_test_deps: false,
    };

    let batches = build_live_rootfs_child_install_batches(&step_refs, &options, &binary_archives)?;

    assert_eq!(
        batches,
        vec![
            ChildInstallBatch {
                requests: vec![expat_archive, python_archive],
                lib32_only: false,
            },
            ChildInstallBatch {
                requests: vec![source_path],
                lib32_only: true,
            },
            ChildInstallBatch {
                requests: vec![compiler_rt_archive],
                lib32_only: false,
            },
        ]
    );
    Ok(())
}

#[test]
fn build_command_requires_install_deps_flag_for_missing_dependencies() -> Result<()> {
    let _guard = assume_yes_test_lock();
    let temp = tempfile::tempdir().context("Failed to create temp dir")?;
    let rootfs = temp.path().join("rootfs");
    let repo_root = temp.path().join("packages");
    let app_dir = repo_root.join("app");
    let dep_dir = repo_root.join("dep");
    fs::create_dir_all(&rootfs)?;
    fs::create_dir_all(&app_dir)?;
    fs::create_dir_all(&dep_dir)?;

    let app_spec = app_dir.join("app.toml");
    fs::write(
        &app_spec,
        r#"[package]
name = "app"
version = "1.0.0"
revision = 1
description = "app"
homepage = "https://example.test/app"
license = "MIT"

[[source]]
url = "https://example.test/app-1.0.0.tar.gz"
sha256 = "skip"
extract_dir = "app-1.0.0"

[build]
type = "custom"

[dependencies]
build = ["dep"]
runtime = []
optional = []
"#,
    )
    .with_context(|| format!("Failed to write {}", app_spec.display()))?;

    let dep_spec = dep_dir.join("dep.toml");
    fs::write(
        &dep_spec,
        r#"[package]
name = "dep"
version = "1.0.0"
revision = 1
description = "dep"
homepage = "https://example.test/dep"
license = "MIT"

[[source]]
url = "https://example.test/dep-1.0.0.tar.gz"
sha256 = "skip"
extract_dir = "dep-1.0.0"

[build]
type = "custom"

[dependencies]
build = []
runtime = []
optional = []
"#,
    )
    .with_context(|| format!("Failed to write {}", dep_spec.display()))?;

    let config = config::Config::for_rootfs(&rootfs);
    register_required_development_package_if_configured(&config, &rootfs)?;

    let result = run(Cli {
        command: Commands::Build(BuildArgs {
            rootfs_args: rootfs_args(rootfs.clone()),
            prompt_args: prompt_args(true),
            build_exec_args: build_exec_args(),
            lib32_args: lib32_args(),
            spec_pos: Some(app_spec),
            spec: None,
            install: false,
            install_deps: false,
            cleanup_deps: false,
        }),
    });
    ui::set_assume_yes(false);

    let err = result.expect_err("build should require --install-deps when deps are missing");
    assert!(err.to_string().contains("Re-run with --install-deps"));
    Ok(())
}

#[test]
fn make_lib32_build_spec_uses_only_lib32_flag_rules() {
    let mut base = test_package_spec(package::BuildType::Custom, None, &[]);
    base.build.flags.cflags = vec!["-O2".into()];
    base.build.flags.replace_cflags = vec!["-O2=>-O3".into()];
    base.build.flags.cflags_lib32 = vec!["-m32".into()];
    base.build.flags.replace_cflags_lib32 = vec!["-m32=>-mstackrealign".into()];
    base.build.flags.cxxflags = vec!["-O2".into()];
    base.build.flags.replace_cxxflags = vec!["-O2=>-O3".into()];
    base.build.flags.cxxflags_lib32 = vec!["-fno-rtti".into()];
    base.build.flags.replace_cxxflags_lib32 = vec!["-fno-rtti=>-fno-exceptions".into()];

    let lib32 = make_lib32_build_spec(&base);

    assert!(lib32.build.flags.lib32_variant);
    assert_eq!(lib32.build.flags.cflags, vec!["-m32"]);
    assert_eq!(
        lib32.build.flags.replace_cflags,
        vec!["-m32=>-mstackrealign"]
    );
    assert_eq!(lib32.build.flags.cxxflags, vec!["-fno-rtti"]);
    assert_eq!(
        lib32.build.flags.replace_cxxflags,
        vec!["-fno-rtti=>-fno-exceptions"]
    );
}

#[test]
fn make_lib32_package_spec_uses_lib32_dependency_override() {
    let mut base = test_package_spec(package::BuildType::Custom, None, &[]);
    base.dependencies.runtime = vec!["zlib".into()];
    base.dependencies.lib32 = Some(package::DependencyGroup {
        build: vec!["gcc-multilib".into()],
        runtime: vec!["lib32-zlib".into()],
        test: Vec::new(),
        optional: vec!["lib32-gtk-doc".into()],
        groups: Vec::new(),
    });

    let lib32 = make_lib32_package_spec(&base);

    assert_eq!(lib32.package.name, "lib32-pkg");
    assert_eq!(lib32.dependencies.build, vec!["gcc-multilib"]);
    assert_eq!(lib32.dependencies.runtime, vec!["lib32-zlib", "pkg"]);
    assert_eq!(lib32.dependencies.optional, vec!["lib32-gtk-doc"]);
}

#[test]
fn make_lib32_package_spec_does_not_inherit_primary_alternatives() {
    let mut base = test_package_spec(package::BuildType::Custom, None, &[]);
    base.alternatives.provides = vec!["editor".into()];
    base.alternatives.conflicts = vec!["nano".into()];
    base.alternatives.replaces = vec!["vi".into()];

    let lib32 = make_lib32_package_spec(&base);

    assert_eq!(lib32.package.name, "lib32-pkg");
    assert!(lib32.alternatives.provides.is_empty());
    assert!(lib32.alternatives.conflicts.is_empty());
    assert!(lib32.alternatives.replaces.is_empty());
}

#[test]
fn requested_outputs_prefers_lib32_only_spec_flag() {
    let mut spec = test_package_spec(package::BuildType::Custom, None, &[]);
    spec.build.flags.lib32_only = true;

    assert_eq!(
        requested_outputs(&spec, false),
        deps::RequestedOutputs::Lib32Only
    );
}

#[test]
fn expand_install_requests_for_groups_uses_source_specs() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let rootfs = temp.path().join("rootfs");
    let repo_root = temp.path().join("repos");
    let core = repo_root.join("core").join("foo");
    let desktop = repo_root.join("desktop").join("bar");
    fs::create_dir_all(&rootfs)?;
    fs::create_dir_all(&core)?;
    fs::create_dir_all(&desktop)?;

    let foo_spec = core.join("foo.toml");
    fs::write(
        &foo_spec,
        r#"[package]
name = "foo"
version = "1.0.0"
revision = 1
description = "foo"
homepage = "https://example.test/foo"
license = "MIT"

[[source]]
url = "https://example.test/foo-1.0.0.tar.gz"
sha256 = "skip"
extract_dir = "foo-1.0.0"

[build]
type = "custom"

[dependencies]
groups = ["base"]
runtime = []
optional = []
"#,
    )?;

    let bar_spec = desktop.join("bar.toml");
    fs::write(
        &bar_spec,
        r#"[package]
name = "bar"
version = "1.0.0"
revision = 1
description = "bar"
homepage = "https://example.test/bar"
license = "MIT"

[[source]]
url = "https://example.test/bar-1.0.0.tar.gz"
sha256 = "skip"
extract_dir = "bar-1.0.0"

[build]
type = "custom"

[dependencies]
groups = ["base", "desktop"]
runtime = []
optional = []
"#,
    )?;

    let mut config = config::Config::for_rootfs(&rootfs);
    config.repo_clone_dir = repo_root;
    config.binary_repos.clear();

    let (expanded, groups) =
        expand_install_requests_for_groups(&config, &rootfs, &[PathBuf::from("base")])?;

    assert_eq!(groups, vec!["base".to_string()]);
    assert_eq!(expanded, vec![PathBuf::from("bar"), PathBuf::from("foo")]);
    Ok(())
}

#[test]
fn expand_installed_group_targets_uses_installed_group_membership() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let rootfs = temp.path().join("rootfs");
    fs::create_dir_all(&rootfs)?;
    let config = config::Config::for_rootfs(&rootfs);
    let db_path = config.installed_db_path(&rootfs);

    let dest = temp.path().join("dest");
    fs::create_dir_all(dest.join("usr/bin"))?;
    fs::write(dest.join("usr/bin/foo"), "foo")?;

    let mut spec = test_package_spec(package::BuildType::Custom, None, &[]);
    spec.package.name = "foo".into();
    spec.dependencies.groups = vec!["base".into()];
    db::register_package(&db_path, &spec, &dest)?;
    db::record_installed_groups(&db_path, &[String::from("base")])?;

    let (expanded, groups) = expand_installed_group_targets(&db_path, &[String::from("base")])?;
    assert_eq!(groups, vec!["base".to_string()]);
    assert_eq!(expanded, vec!["foo".to_string()]);
    Ok(())
}
