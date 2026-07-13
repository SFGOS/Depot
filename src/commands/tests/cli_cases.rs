use super::*;

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
fn rootfs_is_system_root_detects_live_rootfs() {
    assert!(rootfs_is_system_root(Path::new("/")));
    assert!(!rootfs_is_system_root(Path::new("/tmp/depot-test-rootfs")));
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
