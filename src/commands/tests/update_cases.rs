use super::*;

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
fn transaction_orders_relinquishing_update_before_new_file_owner() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let payloads = tempfile::tempdir().context("Failed to create payload dir")?;
    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.build_dir = rootfs.path().join("var/cache/depot/build");
    cfg.db_dir = rootfs.path().join("var/lib/depot");
    let db_path = cfg.installed_db_path(rootfs.path());

    let old_alpha = file_ownership_test_spec("alpha", "1.0");
    let old_alpha_dest = payloads.path().join("old-alpha");
    stage_file(&old_alpha_dest, "usr/bin/shared", "alpha-old")?;
    stage_file(rootfs.path(), "usr/bin/shared", "alpha-old")?;
    db::register_package(&db_path, &old_alpha, &old_alpha_dest)?;

    let new_alpha = file_ownership_test_spec("alpha", "2.0");
    let new_alpha_dest = payloads.path().join("new-alpha");
    stage_file(&new_alpha_dest, "usr/bin/alpha", "alpha-new")?;
    let beta = file_ownership_test_spec("beta", "1.0");
    let beta_dest = payloads.path().join("beta");
    stage_file(&beta_dest, "usr/bin/shared", "beta")?;

    let mut plans = plan_package_outputs_for_install(&beta, &beta_dest, rootfs.path(), &cfg)?;
    plans.extend(plan_package_outputs_for_install(
        &new_alpha,
        &new_alpha_dest,
        rootfs.path(),
        &cfg,
    )?);

    let ordered = preflight_file_ownership_and_order(&plans, &HashSet::new(), rootfs.path(), &cfg)?;
    assert_eq!(ordered[0].spec.package.name, "alpha");
    assert_eq!(ordered[1].spec.package.name, "beta");

    install_direct_transaction(&plans, rootfs.path(), &cfg)?;

    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/bin/shared"))?,
        "beta"
    );
    assert_eq!(
        db::owns_path(&db_path, Path::new("usr/bin/shared"))?,
        Some("beta".into())
    );
    assert_eq!(
        db::get_package_version(&db_path, "alpha")?,
        Some("2.0".into())
    );
    Ok(())
}

#[test]
fn transaction_rejects_file_still_owned_after_planned_update_before_mutation() -> Result<()> {
    let rootfs = tempfile::tempdir().context("Failed to create temp rootfs")?;
    let payloads = tempfile::tempdir().context("Failed to create payload dir")?;
    let mut cfg = config::Config::for_rootfs(rootfs.path());
    cfg.build_dir = rootfs.path().join("var/cache/depot/build");
    cfg.db_dir = rootfs.path().join("var/lib/depot");
    let db_path = cfg.installed_db_path(rootfs.path());

    let old_alpha = file_ownership_test_spec("alpha", "1.0");
    let old_alpha_dest = payloads.path().join("old-alpha");
    stage_file(&old_alpha_dest, "usr/bin/shared", "alpha-old")?;
    stage_file(rootfs.path(), "usr/bin/shared", "alpha-old")?;
    db::register_package(&db_path, &old_alpha, &old_alpha_dest)?;

    let new_alpha = file_ownership_test_spec("alpha", "2.0");
    let new_alpha_dest = payloads.path().join("new-alpha");
    stage_file(&new_alpha_dest, "usr/bin/shared", "alpha-new")?;
    let beta = file_ownership_test_spec("beta", "1.0");
    let beta_dest = payloads.path().join("beta");
    stage_file(&beta_dest, "usr/bin/shared", "beta")?;

    let mut plans = plan_package_outputs_for_install(&beta, &beta_dest, rootfs.path(), &cfg)?;
    plans.extend(plan_package_outputs_for_install(
        &new_alpha,
        &new_alpha_dest,
        rootfs.path(),
        &cfg,
    )?);

    let err = install_direct_transaction(&plans, rootfs.path(), &cfg)
        .expect_err("retained ownership conflict should fail preflight");
    assert!(
        err.to_string()
            .contains("still provided by its transaction update")
    );
    assert_eq!(
        fs::read_to_string(rootfs.path().join("usr/bin/shared"))?,
        "alpha-old"
    );
    assert_eq!(
        db::get_package_version(&db_path, "alpha")?,
        Some("1.0".into())
    );
    assert_eq!(db::get_package_version(&db_path, "beta")?, None);
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
