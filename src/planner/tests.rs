use super::{
    Candidate, CandidateKind, MatchKind, PlannerOptions, build_dependency_install_plan,
    dedupe_candidate_packages, prune_replacement_fallback_candidates, sort_candidates,
    source_deps_for_install,
};
use crate::config::{BinaryRepo, Config};
use crate::db;
use crate::package::{
    Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn mk_spec() -> PackageSpec {
    PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: vec![PackageInfo {
            name: "foo-libs".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        }],
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.test/foo.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies {
            build: vec!["make".into()],
            runtime: vec!["foo-libs".into(), "zlib".into()],
            test: vec!["bats".into()],
            optional: vec!["docs-viewer".into()],
            groups: Vec::new(),
            lib32: None,
        },
        package_alternatives: BTreeMap::from([(
            "foo-libs".into(),
            Alternatives {
                provides: vec!["libfoo".into()],
                conflicts: Vec::new(),
                replaces: Vec::new(),
                lib32: None,
            },
        )]),
        package_dependencies: BTreeMap::from([(
            "foo".into(),
            Dependencies {
                build: Vec::new(),
                runtime: vec!["foo-libs".into(), "libfoo".into(), "openssl".into()],
                test: Vec::new(),
                optional: Vec::new(),
                groups: Vec::new(),
                lib32: None,
            },
        )]),
        spec_dir: PathBuf::from("."),
    }
}

fn mk_installed_spec(name: &str, version: &str) -> PackageSpec {
    let mut spec = mk_spec();
    spec.package.name = name.to_string();
    spec.package.version = version.to_string();
    spec
}

fn mk_binary_candidate(name: &str, repo_name: &str, priority: i32) -> Candidate {
    Candidate {
        package: name.to_string(),
        kind: CandidateKind::Binary {
            repo_name: repo_name.to_string(),
            record: Box::new(db::repo::BinaryRepoPackageRecord {
                repo_name: repo_name.to_string(),
                name: name.to_string(),
                real_name: None,
                version: "1.0.0".to_string(),
                revision: 1,
                abi_breaking: false,
                built_against: Vec::new(),
                completed_at: None,
                filename: format!("{name}-1.0.0-1-x86_64.depot.pkg.tar.zst"),
                size: 1024,
                sha512: "sha512".to_string(),
                description: None,
                homepage: None,
                license: None,
                provides: Vec::new(),
                conflicts: Vec::new(),
                replaces: Vec::new(),
                runtime_dependencies: Vec::new(),
                optional_dependencies: Vec::new(),
                groups: Vec::new(),
            }),
        },
        match_kind: MatchKind::Exact,
        sort_repo_priority: priority,
        sort_label: format!("binary:{repo_name}"),
    }
}

fn mk_source_candidate(name: &str, path: &Path, local_sibling: bool) -> Candidate {
    Candidate {
        package: name.to_string(),
        kind: CandidateKind::Source {
            path: path.to_path_buf(),
            local_sibling,
        },
        match_kind: MatchKind::Exact,
        sort_repo_priority: if local_sibling { -10 } else { 0 },
        sort_label: if local_sibling {
            "source:local-sibling".to_string()
        } else {
            "source:local".to_string()
        },
    }
}

#[test]
fn source_deps_for_install_excludes_local_runtime_outputs_and_provides() {
    let spec = mk_spec();
    let deps = source_deps_for_install(&spec, false, false);
    assert!(deps.contains(&"make".to_string()));
    assert!(deps.contains(&"zlib".to_string()));
    assert!(deps.contains(&"openssl".to_string()));
    assert!(!deps.contains(&"foo-libs".to_string()));
    assert!(!deps.contains(&"libfoo".to_string()));
}

#[test]
fn source_deps_for_install_does_not_include_test_deps() {
    let spec = mk_spec();
    let deps = source_deps_for_install(&spec, false, false);
    assert!(!deps.contains(&"bats".to_string()));
}

#[test]
fn source_deps_for_install_includes_test_deps_when_enabled() {
    let spec = mk_spec();
    let deps = source_deps_for_install(&spec, true, false);
    assert!(deps.contains(&"bats".to_string()));
}

#[test]
fn source_deps_for_install_uses_lib32_only_dependencies_when_requested() {
    let mut spec = mk_spec();
    spec.dependencies.lib32 = Some(crate::package::DependencyGroup {
        build: vec!["gcc-multilib".into()],
        runtime: vec!["lib32-zlib".into()],
        test: vec!["lib32-bats".into()],
        optional: Vec::new(),
        groups: Vec::new(),
    });

    let deps = source_deps_for_install(&spec, true, true);
    assert!(deps.contains(&"gcc-multilib".to_string()));
    assert!(deps.contains(&"lib32-zlib".to_string()));
    assert!(!deps.contains(&"lib32-bats".to_string()));
    assert!(!deps.contains(&"make".to_string()));
    assert!(!deps.contains(&"zlib".to_string()));
    assert!(!deps.contains(&"bats".to_string()));
}

#[test]
fn source_deps_for_install_uses_lib32_only_dependencies_from_spec_flag() {
    let mut spec = mk_spec();
    spec.build.flags.lib32_only = true;
    spec.dependencies.lib32 = Some(crate::package::DependencyGroup {
        build: vec!["gcc-multilib".into()],
        runtime: vec!["lib32-zlib".into()],
        test: vec!["lib32-bats".into()],
        optional: Vec::new(),
        groups: Vec::new(),
    });

    let deps = source_deps_for_install(&spec, true, false);
    assert!(deps.contains(&"gcc-multilib".to_string()));
    assert!(deps.contains(&"lib32-zlib".to_string()));
    assert!(!deps.contains(&"lib32-bats".to_string()));
    assert!(!deps.contains(&"make".to_string()));
    assert!(!deps.contains(&"zlib".to_string()));
    assert!(!deps.contains(&"bats".to_string()));
}

#[test]
fn candidate_dedup_keeps_highest_priority_origin_for_same_package() {
    let candidates = vec![
        mk_source_candidate("meson", Path::new("packages/core/meson/meson.toml"), false),
        mk_binary_candidate("meson", "core", 0),
        mk_source_candidate(
            "meson",
            Path::new("../packages/core/meson/meson.toml"),
            true,
        ),
    ];

    let deduped = dedupe_candidate_packages(sort_candidates(&candidates, true));
    assert_eq!(deduped.len(), 1);
    assert!(matches!(deduped[0].kind, CandidateKind::Binary { .. }));
}

#[test]
fn candidate_dedup_uses_local_origin_when_binaries_are_not_preferred() {
    let candidates = vec![
        mk_binary_candidate("ninja", "core", 0),
        mk_source_candidate("ninja", Path::new("packages/core/ninja/ninja.toml"), false),
    ];

    let deduped = dedupe_candidate_packages(sort_candidates(&candidates, false));
    assert_eq!(deduped.len(), 1);
    assert!(matches!(deduped[0].kind, CandidateKind::Source { .. }));
}

#[test]
fn replacement_candidates_are_pruned_when_direct_matches_exist() {
    let mut replacement = mk_binary_candidate("vx", "core", 0);
    replacement.match_kind = MatchKind::Replaces;

    let mut exact = mk_binary_candidate("patch", "core", 0);
    exact.match_kind = MatchKind::Exact;

    let mut provides = mk_binary_candidate("busybox", "core", 0);
    provides.match_kind = MatchKind::Provides;

    let pruned = prune_replacement_fallback_candidates(vec![replacement, exact, provides]);
    assert_eq!(pruned.len(), 2);
    assert!(
        pruned
            .iter()
            .all(|candidate| candidate.match_kind != MatchKind::Replaces)
    );
}

#[test]
fn replacement_candidates_remain_when_they_are_the_only_matches() {
    let mut replacement = mk_binary_candidate("vx", "core", 0);
    replacement.match_kind = MatchKind::Replaces;

    let pruned = prune_replacement_fallback_candidates(vec![replacement]);
    assert_eq!(pruned.len(), 1);
    assert!(matches!(pruned[0].match_kind, MatchKind::Replaces));
    assert_eq!(pruned[0].package, "vx");
}

#[test]
fn build_dependency_install_plan_skips_installed_dependency() {
    let rootfs = tempfile::tempdir().unwrap();
    let config = Config::for_rootfs(rootfs.path());
    let db_path = config.db_dir.join("packages.db");
    let destdir = rootfs.path().join("dest");
    std::fs::create_dir_all(&destdir).unwrap();
    db::register_package(&db_path, &mk_installed_spec("meson", "1.0.0"), &destdir).unwrap();

    let plan = build_dependency_install_plan(
        &config,
        rootfs.path(),
        &["meson".to_string()],
        PlannerOptions {
            assume_yes: false,
            prefer_binary: true,
            local_sibling_root: None,
            include_test_deps: false,
            lib32_only_requested_specs: false,
        },
    )
    .unwrap();

    assert!(plan.steps.is_empty());
    assert!(plan.actionable_steps().next().is_none());
}

#[test]
fn build_dependency_install_plan_reports_source_cycle_chain() {
    let rootfs = tempfile::tempdir().unwrap();
    let repo_root = tempfile::tempdir().unwrap();
    let config = Config::for_rootfs(rootfs.path());

    let alpha_dir = repo_root.path().join("alpha");
    let beta_dir = repo_root.path().join("beta");
    fs::create_dir_all(&alpha_dir).unwrap();
    fs::create_dir_all(&beta_dir).unwrap();

    fs::write(
        alpha_dir.join("alpha.toml"),
        r#"
[build]
type = "meta"

[dependencies]
runtime = ["beta"]

[package]
description = "alpha"
homepage = "https://example.test/alpha"
license = "MIT"
name = "alpha"
version = "1.0.0"
"#,
    )
    .unwrap();

    fs::write(
        beta_dir.join("beta.toml"),
        r#"
[build]
type = "meta"

[dependencies]
runtime = ["alpha"]

[package]
description = "beta"
homepage = "https://example.test/beta"
license = "MIT"
name = "beta"
version = "1.0.0"
"#,
    )
    .unwrap();

    let err = build_dependency_install_plan(
        &config,
        rootfs.path(),
        &["alpha".to_string()],
        PlannerOptions {
            assume_yes: false,
            prefer_binary: false,
            local_sibling_root: Some(repo_root.path().to_path_buf()),
            include_test_deps: false,
            lib32_only_requested_specs: false,
        },
    )
    .unwrap_err();

    assert_eq!(
        err.to_string(),
        "Dependency cycle detected: alpha -> beta -> alpha"
    );
}

#[test]
fn build_dependency_install_plan_matches_local_sibling_real_name() {
    let rootfs = tempfile::tempdir().unwrap();
    let repo_root = tempfile::tempdir().unwrap();
    let config = Config::for_rootfs(rootfs.path());

    let libressl_dir = repo_root.path().join("libressl43");
    fs::create_dir_all(&libressl_dir).unwrap();
    fs::write(
        libressl_dir.join("libressl43.toml"),
        r#"
[build]
type = "meta"

[package]
description = "LibreSSL"
homepage = "https://www.libressl.org/"
license = "ISC"
name = "libressl43"
real_name = "libressl"
version = "4.3.2"
"#,
    )
    .unwrap();

    let plan = build_dependency_install_plan(
        &config,
        rootfs.path(),
        &["libressl".to_string()],
        PlannerOptions {
            assume_yes: false,
            prefer_binary: false,
            local_sibling_root: Some(repo_root.path().to_path_buf()),
            include_test_deps: false,
            lib32_only_requested_specs: false,
        },
    )
    .unwrap();

    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].package, "libressl43");
    assert!(matches!(
        plan.steps[0].origin,
        super::PlanOrigin::Source {
            local_sibling: true,
            ..
        }
    ));
}

fn write_compressed_repo_db(db_path: &Path, zst_path: &Path) {
    let mut input = fs::File::open(db_path).unwrap();
    let output = fs::File::create(zst_path).unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(output, 3).unwrap();
    std::io::copy(&mut input, &mut encoder).unwrap();
    encoder.finish().unwrap();
}

#[test]
fn binary_plan_uses_built_against_concrete_dependency() {
    let rootfs = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let mut config = Config::for_rootfs(rootfs.path());
    config.package_cache_dir = cache_dir.path().to_path_buf();
    config.repo_settings.prefer_binary = true;
    config.binary_repos.insert(
        "core".into(),
        BinaryRepo {
            url: url::Url::from_directory_path(repo_dir.path())
                .expect("file URL")
                .to_string(),
            allow_unsigned: true,
            ..BinaryRepo::default()
        },
    );

    let installed_dest = rootfs.path().join("installed");
    fs::create_dir_all(&installed_dest).unwrap();
    let mut installed = mk_installed_spec("icu79", "79.1");
    installed.package.real_name = Some("icu".into());
    db::register_package(
        &config.installed_db_path(rootfs.path()),
        &installed,
        &installed_dest,
    )
    .unwrap();

    let db_path = repo_dir.path().join("repo.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "
            CREATE TABLE packages (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                real_name TEXT,
                version TEXT NOT NULL,
                revision INTEGER NOT NULL,
                abi_breaking INTEGER NOT NULL DEFAULT 0,
                built_against TEXT NOT NULL DEFAULT '',
                completed_at INTEGER,
                description TEXT,
                homepage TEXT,
                license TEXT,
                filename TEXT NOT NULL,
                size INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                sha512 TEXT NOT NULL
            );
            CREATE TABLE provides (package_id INTEGER, name TEXT NOT NULL);
            CREATE TABLE replaces (package_id INTEGER, name TEXT NOT NULL);
            CREATE TABLE conflicts (package_id INTEGER, name TEXT NOT NULL);
            CREATE TABLE dependencies (package_id INTEGER, kind TEXT NOT NULL, name TEXT NOT NULL);
            CREATE TABLE groups (package_id INTEGER, name TEXT NOT NULL);
            ",
    )
    .unwrap();
    conn.execute(
            "INSERT INTO packages (id, name, real_name, version, revision, abi_breaking, built_against, completed_at, description, homepage, license, filename, size, sha256, sha512)
             VALUES (1, 'app', NULL, '1.0', 1, 0, 'icu78', NULL, NULL, NULL, NULL, 'app.pkg', 1, 'aa', 'bb')",
            [],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO dependencies (package_id, kind, name) VALUES (1, 'runtime', 'icu')",
        [],
    )
    .unwrap();
    conn.execute(
            "INSERT INTO packages (id, name, real_name, version, revision, abi_breaking, built_against, completed_at, description, homepage, license, filename, size, sha256, sha512)
             VALUES (2, 'icu78', 'icu', '78.1', 1, 0, '', NULL, NULL, NULL, NULL, 'icu78.pkg', 1, 'cc', 'dd')",
            [],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO packages (id, name, real_name, version, revision, abi_breaking, built_against, completed_at, description, homepage, license, filename, size, sha256, sha512)
             VALUES (3, 'icu79', 'icu', '79.1', 1, 0, '', NULL, NULL, NULL, NULL, 'icu79.pkg', 1, 'ee', 'ff')",
            [],
        )
        .unwrap();
    drop(conn);
    write_compressed_repo_db(&db_path, &repo_dir.path().join("repo.db.zst"));
    fs::remove_file(&db_path).unwrap();

    let plan = super::build_install_plan(
        &config,
        rootfs.path(),
        super::InstallTarget::PackageName("app".into()),
        PlannerOptions {
            assume_yes: true,
            prefer_binary: true,
            local_sibling_root: None,
            include_test_deps: false,
            lib32_only_requested_specs: false,
        },
    )
    .unwrap();

    let actionable: Vec<_> = plan
        .actionable_steps()
        .map(|step| step.package.as_str())
        .collect();
    assert_eq!(actionable, vec!["icu78", "app"]);
}

#[test]
fn add_dependency_edge_skips_active_binary_cycle_back_edge() {
    let rootfs = tempfile::tempdir().unwrap();
    let config = Config::for_rootfs(rootfs.path());
    let mut resolver = super::Resolver::new(
        &config,
        rootfs.path(),
        PlannerOptions {
            assume_yes: false,
            prefer_binary: true,
            local_sibling_root: None,
            include_test_deps: false,
            lib32_only_requested_specs: false,
        },
    );

    let freetype2 = resolver.graph.add_node(super::NodeData {
        step: super::PlannedStep {
            package: "freetype2".into(),
            action: super::PlanAction::InstallBinary,
            origin: super::PlanOrigin::Installed,
            requested_by: vec!["requested".into()],
        },
    });
    let harfbuzz = resolver.graph.add_node(super::NodeData {
        step: super::PlannedStep {
            package: "harfbuzz".into(),
            action: super::PlanAction::InstallBinary,
            origin: super::PlanOrigin::Installed,
            requested_by: vec!["requested".into()],
        },
    });

    resolver.stack = vec!["freetype2".into(), "harfbuzz".into()];
    resolver
        .add_dependency_edge(freetype2, harfbuzz, "harfbuzz")
        .unwrap();

    assert_eq!(resolver.graph.edge_count(), 0);

    resolver.stack.clear();
    resolver
        .add_dependency_edge(freetype2, harfbuzz, "harfbuzz")
        .unwrap();

    assert_eq!(resolver.graph.edge_count(), 1);
}
