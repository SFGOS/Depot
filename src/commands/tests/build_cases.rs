use super::*;

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
