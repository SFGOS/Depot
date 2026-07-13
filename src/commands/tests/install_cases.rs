use super::*;

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
fn parallel_tasks_run_concurrently_and_preserve_input_order() -> Result<()> {
    let items = vec![3_u8, 1, 4, 2];
    let barrier = Barrier::new(items.len());

    let results = run_parallel_tasks(&items, items.len(), |_, item| {
        barrier.wait();
        Ok(item * 2)
    })?;

    assert_eq!(results, vec![6, 2, 8, 4]);
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
