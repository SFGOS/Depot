use super::*;
use crate::package::{
    Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
};
use crate::test_support::TestEnv;
use std::path::PathBuf;

fn mk_spec(name: &str, version: &str) -> PackageSpec {
    PackageSpec {
        package: PackageInfo {
            name: name.into(),
            real_name: None,
            version: version.into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives {
            provides: vec![format!("{}-virtual", name)],
            conflicts: Vec::new(),
            replaces: Vec::new(),
            lib32: None,
        },
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Custom,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    }
}

#[test]
fn register_package_updates_in_place_and_replaces_file_list() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");

    let spec_v1 = mk_spec("foo", "1.0");
    let dest1 = tmp.path().join("dest1");
    std::fs::create_dir_all(dest1.join("usr/bin")).unwrap();
    std::fs::write(dest1.join("usr/bin/foo"), "v1").unwrap();

    register_package(&db_path, &spec_v1, &dest1).unwrap();

    // Capture package id
    let conn = Connection::open(&db_path).unwrap();
    let id1: i64 = conn
        .query_row(
            "SELECT id FROM packages WHERE name = ?1",
            params!["foo"],
            |r| r.get(0),
        )
        .unwrap();

    // Update with different file set
    let spec_v2 = mk_spec("foo", "2.0");
    let dest2 = tmp.path().join("dest2");
    std::fs::create_dir_all(dest2.join("usr/bin")).unwrap();
    std::fs::write(dest2.join("usr/bin/foo"), "v2").unwrap();
    std::fs::write(dest2.join("usr/bin/new_only"), "x").unwrap();

    register_package(&db_path, &spec_v2, &dest2).unwrap();

    let id2: i64 = conn
        .query_row(
            "SELECT id FROM packages WHERE name = ?1",
            params!["foo"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(id1, id2);

    let files = get_package_files(&db_path, "foo").unwrap();
    assert!(files.contains(&"usr/bin/foo".to_string()));
    assert!(files.contains(&"usr/bin/new_only".to_string()));

    let version = get_package_version(&db_path, "foo").unwrap();
    assert_eq!(version.as_deref(), Some("2.0"));
}

#[test]
fn installed_dependency_names_include_real_name_aliases() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(&destdir).unwrap();

    let mut spec = mk_spec("libressl43", "4.3.2");
    spec.package.real_name = Some("libressl".into());
    register_package(&db_path, &spec, &destdir).unwrap();

    let names = get_installed_dependency_names(&db_path).unwrap();
    assert!(names.contains("libressl43"));
    assert!(names.contains("libressl"));
    assert_eq!(
        get_dependency_version(&db_path, "libressl")
            .unwrap()
            .as_deref(),
        Some("4.3.2")
    );
}

#[test]
fn register_package_uses_metadata_completed_at_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let spec = mk_spec("foo", "1.0");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
    std::fs::write(dest.join("usr/bin/foo"), "bin").unwrap();
    std::fs::write(
        dest.join(".metadata.toml"),
        "completed_at = \"2026-03-10T12:34:56Z\"\n",
    )
    .unwrap();

    register_package(&db_path, &spec, &dest).unwrap();

    let records = list_installed_package_records(&db_path).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].completed_at, Some(1_773_146_096));
}

#[test]
fn register_package_falls_back_to_destdir_mtime_when_metadata_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let spec = mk_spec("foo", "1.0");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
    let file = dest.join("usr/bin/foo");
    std::fs::write(&file, "bin").unwrap();

    let ts = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&file, ts).unwrap();
    filetime::set_file_mtime(dest.join("usr"), ts).unwrap();
    filetime::set_file_mtime(dest.join("usr/bin"), ts).unwrap();
    filetime::set_file_mtime(&dest, ts).unwrap();

    register_package(&db_path, &spec, &dest).unwrap();

    let records = list_installed_package_records(&db_path).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].completed_at, Some(1_700_000_000));
}

#[test]
fn register_package_detects_conflicting_files() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");

    // Install package 'alpha' owning usr/bin/shared
    let spec_a = mk_spec("alpha", "1.0");
    let dest_a = tmp.path().join("dest_a");
    std::fs::create_dir_all(dest_a.join("usr/bin")).unwrap();
    std::fs::write(dest_a.join("usr/bin/shared"), "a").unwrap();
    register_package(&db_path, &spec_a, &dest_a).unwrap();

    // Try to install package 'beta' that also includes the same path -> should fail
    let spec_b = mk_spec("beta", "1.0");
    let dest_b = tmp.path().join("dest_b");
    std::fs::create_dir_all(dest_b.join("usr/bin")).unwrap();
    std::fs::write(dest_b.join("usr/bin/shared"), "b").unwrap();

    let res = register_package(&db_path, &spec_b, &dest_b);
    assert!(res.is_err());
    let err = format!("{}", res.err().unwrap());
    assert!(err.contains("File ownership conflict detected"));
    assert!(err.contains("usr/bin/shared"));
    assert!(err.contains("alpha"));
}

#[test]
fn register_package_auto_clears_safe_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");

    // Install package 'alpha' owning a known shared Perl path
    let spec_a = mk_spec("alpha", "1.0");
    let dest_a = tmp.path().join("dest_a");
    std::fs::create_dir_all(dest_a.join("usr/share/perl5")).unwrap();
    std::fs::write(dest_a.join("usr/share/perl5/shared.pm"), "package A;").unwrap();
    register_package(&db_path, &spec_a, &dest_a).unwrap();

    // Now install package 'beta' that also provides the same shared path -> should auto-clear
    let spec_b = mk_spec("beta", "1.0");
    let dest_b = tmp.path().join("dest_b");
    std::fs::create_dir_all(dest_b.join("usr/share/perl5")).unwrap();
    std::fs::write(dest_b.join("usr/share/perl5/shared.pm"), "package B;").unwrap();

    // This should succeed and transfer ownership of the shared path to beta
    register_package(&db_path, &spec_b, &dest_b).unwrap();

    // Verify DB: alpha should no longer own the path, beta should
    let files_a = get_package_files(&db_path, "alpha").unwrap();
    assert!(!files_a.contains(&"usr/share/perl5/shared.pm".to_string()));
    let files_b = get_package_files(&db_path, "beta").unwrap();
    assert!(files_b.contains(&"usr/share/perl5/shared.pm".to_string()));
}

#[test]
fn register_package_auto_clears_sbase_conflicts_when_requested() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");
    let db_path = crate::config::Config::for_rootfs(&rootfs).installed_db_path(&rootfs);
    std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    std::fs::write(rootfs.join("usr/bin/find"), "sbase find").unwrap();

    let spec_a = mk_spec("sbase", "1.0");
    let dest_a = tmp.path().join("dest_a");
    std::fs::create_dir_all(dest_a.join("usr/bin")).unwrap();
    std::fs::write(dest_a.join("usr/bin/find"), "sbase find").unwrap();
    register_package(&db_path, &spec_a, &dest_a).unwrap();

    let spec_b = mk_spec("bfs", "4.1");
    let dest_b = tmp.path().join("dest_b");
    std::fs::create_dir_all(dest_b.join("usr/bin")).unwrap();
    std::fs::write(dest_b.join("usr/bin/find"), "bfs find").unwrap();

    let mut env = TestEnv::new();
    env.set_var(DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS, "1");
    register_package(&db_path, &spec_b, &dest_b).unwrap();

    let files_sbase = get_package_files(&db_path, "sbase").unwrap();
    assert!(!files_sbase.contains(&"usr/bin/find".to_string()));
    let files_bfs = get_package_files(&db_path, "bfs").unwrap();
    assert!(files_bfs.contains(&"usr/bin/find".to_string()));
}

#[test]
fn register_package_auto_clear_preserves_new_payload_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("rootfs");
    let db_path = crate::config::Config::for_rootfs(&rootfs).installed_db_path(&rootfs);
    std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    std::fs::write(rootfs.join("usr/bin/find"), "sbase find").unwrap();

    let spec_a = mk_spec("sbase", "1.0");
    let dest_a = tmp.path().join("dest_a");
    std::fs::create_dir_all(dest_a.join("usr/bin")).unwrap();
    std::fs::write(dest_a.join("usr/bin/find"), "sbase find").unwrap();
    register_package(&db_path, &spec_a, &dest_a).unwrap();

    std::fs::write(rootfs.join("usr/bin/find"), "bfs find").unwrap();
    let spec_b = mk_spec("bfs", "4.1");
    let dest_b = tmp.path().join("dest_b");
    std::fs::create_dir_all(dest_b.join("usr/bin")).unwrap();
    std::fs::write(dest_b.join("usr/bin/find"), "bfs find").unwrap();

    let mut env = TestEnv::new();
    env.set_var(DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS, "1");
    register_package(&db_path, &spec_b, &dest_b).unwrap();

    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/find")).unwrap(),
        "bfs find"
    );
    let files_sbase = get_package_files(&db_path, "sbase").unwrap();
    assert!(!files_sbase.contains(&"usr/bin/find".to_string()));
    let files_bfs = get_package_files(&db_path, "bfs").unwrap();
    assert!(files_bfs.contains(&"usr/bin/find".to_string()));
}

#[test]
fn get_package_files_missing_package_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");

    // Create an empty database file with schema but no packages
    let conn = Connection::open(&db_path).unwrap();
    init_db(&conn).unwrap();
    drop(conn);

    // Querying files for a package that doesn't exist should return an empty list
    let files = get_package_files(&db_path, "nonexistent").unwrap();
    assert!(files.is_empty());
}

#[test]
fn get_package_version_missing_db_returns_none_without_creating_db() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");

    let version = get_package_version(&db_path, "nonexistent").unwrap();
    assert!(version.is_none());
    assert!(!db_path.exists());
}

#[test]
fn calculate_upgrade_paths_handles_existing_db_file_without_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    std::fs::File::create(&db_path).unwrap();
    let manifest = staging::Manifest {
        files: vec!["usr/bin/foo".to_string()],
        directories: Vec::new(),
    };

    let remove_paths = calculate_upgrade_paths(&db_path, "nonexistent", &manifest).unwrap();
    assert!(remove_paths.is_empty());
}

#[test]
fn remove_package_tolerates_missing_files_and_cleans_db() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let rootfs = tmp.path().join("root");
    std::fs::create_dir_all(&rootfs).unwrap();

    let spec = mk_spec("foo", "1.0");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
    std::fs::write(dest.join("usr/bin/foo"), "bin").unwrap();
    register_package(&db_path, &spec, &dest).unwrap();

    // Create the installed file in rootfs (one real)
    std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    std::fs::write(rootfs.join("usr/bin/foo"), "bin").unwrap();

    // Inject an extra missing file into DB to ensure we tolerate it.
    let conn = Connection::open(&db_path).unwrap();
    let pkg_id: i64 = conn
        .query_row(
            "SELECT id FROM packages WHERE name = ?1",
            params!["foo"],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO files (package_id, path) VALUES (?1, ?2)",
        params![pkg_id, "usr/bin/does_not_exist"],
    )
    .unwrap();

    remove_package(&db_path, "foo", &rootfs).unwrap();
    assert!(get_package_version(&db_path, "foo").unwrap().is_none());
}

#[test]
fn test_package_upgrade_removes_orphaned_files() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let rootfs = tmp.path().join("root");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();

    // 1. Install v1: usr/bin/foo, usr/bin/shared_dir/old_file
    let spec_v1 = mk_spec("foo", "1.0");
    let dest1 = tmp.path().join("dest1");
    std::fs::create_dir_all(dest1.join("usr/bin/shared_dir")).unwrap();
    std::fs::write(dest1.join("usr/bin/foo"), "v1").unwrap();
    std::fs::write(dest1.join("usr/bin/shared_dir/old_file"), "old").unwrap();

    register_package(&db_path, &spec_v1, &dest1).unwrap();
    let _ = crate::staging::install_atomic(&dest1, &rootfs, &tx_base, &[], &[]).unwrap();

    assert!(rootfs.join("usr/bin/foo").exists());
    assert!(rootfs.join("usr/bin/shared_dir/old_file").exists());

    // 2. Prepare v2: usr/bin/foo (updated), usr/bin/new_file
    // (shared_dir/old_file is removed from spec)
    let spec_v2 = mk_spec("foo", "2.0");
    let dest2 = tmp.path().join("dest2");
    std::fs::create_dir_all(dest2.join("usr/bin")).unwrap();
    std::fs::write(dest2.join("usr/bin/foo"), "v2").unwrap();
    std::fs::write(dest2.join("usr/bin/new_file"), "new").unwrap();

    let manifest2 = crate::staging::generate_manifest_with_dirs(&dest2).unwrap();
    let remove_paths = calculate_upgrade_paths(&db_path, "foo", &manifest2).unwrap();

    assert_eq!(
        remove_paths,
        vec![
            "usr/bin/shared_dir/old_file".to_string(),
            "usr/bin/shared_dir".to_string()
        ]
    );

    let tx = crate::staging::install_atomic(&dest2, &rootfs, &tx_base, &remove_paths, &[]).unwrap();
    register_package(&db_path, &spec_v2, &dest2).unwrap();
    tx.commit().unwrap();

    // 3. Verify filesystem
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/foo")).unwrap(),
        "v2"
    );
    assert!(rootfs.join("usr/bin/new_file").exists());
    assert!(!rootfs.join("usr/bin/shared_dir/old_file").exists());

    // Check DB
    let files = get_package_files(&db_path, "foo").unwrap();
    assert!(files.contains(&"usr/bin/foo".to_string()));
    assert!(files.contains(&"usr/bin/new_file".to_string()));
    assert!(!files.contains(&"usr/bin/shared_dir/old_file".to_string()));
}

#[test]
fn register_package_persists_replacements() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
    std::fs::write(dest.join("usr/bin/vx"), "vx").unwrap();

    let mut spec = mk_spec("vx", "1.0");
    spec.alternatives.replaces = vec!["grep".into(), "patch".into()];

    register_package(&db_path, &spec, &dest).unwrap();

    let replaces = get_all_replaces(&db_path).unwrap();
    assert!(replaces.contains("grep"));
    assert!(replaces.contains("patch"));
}

#[test]
fn register_package_persists_groups() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
    std::fs::write(dest.join("usr/bin/foo"), "foo").unwrap();

    let mut spec = mk_spec("foo", "1.0");
    spec.dependencies.groups = vec!["base".into(), "desktop".into()];

    register_package(&db_path, &spec, &dest).unwrap();

    assert_eq!(
        get_package_groups(&db_path, "foo").unwrap(),
        vec!["base".to_string(), "desktop".to_string()]
    );
}

#[test]
fn register_package_records_built_against_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::write(dest.join("payload"), "x").unwrap();

    let mut spec = mk_spec("app", "1.0");
    spec.package.built_against = vec!["icu78".into()];
    register_package(&db_path, &spec, &dest).unwrap();

    let records = list_installed_package_records(&db_path).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].built_against, vec!["icu78".to_string()]);
}

#[test]
fn installed_group_helpers_round_trip_membership() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("packages.db");
    let dest = tmp.path().join("dest");
    std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
    std::fs::write(dest.join("usr/bin/foo"), "foo").unwrap();

    let mut spec = mk_spec("foo", "1.0");
    spec.dependencies.groups = vec!["base".into()];
    register_package(&db_path, &spec, &dest).unwrap();

    record_installed_groups(&db_path, &[String::from("base")]).unwrap();
    assert!(is_installed_group(&db_path, "base").unwrap());
    assert_eq!(
        get_packages_in_installed_group(&db_path, "base").unwrap(),
        vec!["foo".to_string()]
    );

    remove_installed_group(&db_path, "base").unwrap();
    assert!(!is_installed_group(&db_path, "base").unwrap());
}
