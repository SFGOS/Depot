use super::*;

#[test]
fn test_init_repo_schema() {
    let mut conn = Connection::open_in_memory().unwrap();
    let manager = RepoManager::new(PathBuf::from("."));
    manager.init_repo_schema(&mut conn).unwrap();

    // Check if table exists
    let exists: bool = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='packages'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(exists);

    let has_sha512: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'sha512'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap();
    assert!(has_sha512);

    let has_completed_at: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'completed_at'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap();
    assert!(has_completed_at);
}

#[test]
fn test_index_package() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path();
    let pkg_path = repo_dir.join("test-1.0-1-x86_64.depot.pkg.tar.zst");

    // Create a valid .tar.zst with .metadata.toml
    let file = fs::File::create(&pkg_path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut tar = tar::Builder::new(encoder);

    let metadata = r#"
name = "test"
real_name = "icu"
version = "1.0"
revision = 1
abi_breaking = true
built_against = ["icu78"]
description = "test description"
homepage = "https://example.com"
license = "MIT"
completed_at = "2026-03-10T12:34:56Z"
provides = ["test-feature"]

[dependencies]
runtime = []
optional = []
"#;
    let mut header = tar::Header::new_gnu();
    header.set_path(".metadata.toml").unwrap();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, metadata.as_bytes()).unwrap();

    let encoder = tar.into_inner().unwrap();
    encoder.finish().unwrap();
    filetime::set_file_mtime(
        &pkg_path,
        filetime::FileTime::from_unix_time(1_700_000_000, 0),
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let manager = RepoManager::new(repo_dir.to_path_buf());
    manager.init_repo_schema(&mut conn).unwrap();
    let indexed = manager.read_indexed_package(&pkg_path).unwrap();
    manager.insert_indexed_package(&mut conn, indexed).unwrap();

    type PackageRow = (
        String,
        Option<String>,
        String,
        i64,
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
    );

    let (
            name,
            real_name,
            version,
            revision,
            abi_breaking,
            built_against,
            desc,
            home,
            lic,
            sha256,
            sha512,
        ): PackageRow = conn
            .query_row(
                "SELECT name, real_name, version, revision, abi_breaking, built_against, description, homepage, license, sha256, sha512 FROM packages",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
                },
            )
            .unwrap();

    assert_eq!(name, "test");
    assert_eq!(real_name, Some("icu".to_string()));
    assert_eq!(version, "1.0");
    assert_eq!(revision, 1);
    assert_eq!(abi_breaking, 1);
    assert_eq!(built_against, "icu78");
    assert_eq!(desc, Some("test description".to_string()));
    assert_eq!(home, Some("https://example.com".to_string()));
    assert_eq!(lic, Some("MIT".to_string()));
    assert_eq!(sha256.len(), 64);
    assert_eq!(sha512.len(), 128);

    let completed_at: Option<i64> = conn
        .query_row("SELECT completed_at FROM packages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(completed_at, Some(1_773_146_096));

    let provides_count: i64 = conn
        .query_row("SELECT count(*) FROM provides", [], |r| r.get(0))
        .unwrap();
    assert_eq!(provides_count, 1);
}

#[test]
fn test_index_package_with_multiple_licenses() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path();
    let pkg_path = repo_dir.join("test-1.0-1-x86_64.depot.pkg.tar.zst");

    let file = fs::File::create(&pkg_path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut tar = tar::Builder::new(encoder);

    let metadata = r#"
name = "test"
version = "1.0"
revision = 1
license = ["MIT", "Apache-2.0"]
"#;
    let mut header = tar::Header::new_gnu();
    header.set_path(".metadata.toml").unwrap();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, metadata.as_bytes()).unwrap();

    let encoder = tar.into_inner().unwrap();
    encoder.finish().unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let manager = RepoManager::new(repo_dir.to_path_buf());
    manager.init_repo_schema(&mut conn).unwrap();
    let indexed = manager.read_indexed_package(&pkg_path).unwrap();
    manager.insert_indexed_package(&mut conn, indexed).unwrap();

    let lic: Option<String> = conn
        .query_row("SELECT license FROM packages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(lic, Some("MIT, Apache-2.0".to_string()));
}

#[test]
fn test_index_package_records_symlink_paths_for_repo_owns() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path();
    let pkg_path = repo_dir.join("test-1.0-1-x86_64.depot.pkg.tar.zst");

    let file = fs::File::create(&pkg_path).unwrap();
    let encoder = zstd::stream::write::Encoder::new(file, 3).unwrap();
    let mut tar = tar::Builder::new(encoder);

    let metadata = r#"
name = "test"
version = "1.0"
revision = 1
"#;
    let mut header = tar::Header::new_gnu();
    header.set_path(".metadata.toml").unwrap();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, metadata.as_bytes()).unwrap();

    let mut file_header = tar::Header::new_gnu();
    file_header.set_path("usr/bin/coreutils").unwrap();
    file_header.set_size(4);
    file_header.set_mode(0o755);
    file_header.set_cksum();
    tar.append(&file_header, &b"test"[..]).unwrap();

    let mut link_header = tar::Header::new_gnu();
    link_header.set_entry_type(tar::EntryType::Symlink);
    link_header.set_path("usr/bin/ls").unwrap();
    link_header.set_link_name("coreutils").unwrap();
    link_header.set_size(0);
    link_header.set_mode(0o777);
    link_header.set_cksum();
    tar.append(&link_header, std::io::empty()).unwrap();

    let encoder = tar.into_inner().unwrap();
    encoder.finish().unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let manager = RepoManager::new(repo_dir.to_path_buf());
    manager.init_repo_schema(&mut conn).unwrap();
    let indexed = manager.read_indexed_package(&pkg_path).unwrap();
    manager.insert_indexed_package(&mut conn, indexed).unwrap();

    let db_path = repo_dir.join("repo.db");
    let mut file_conn = Connection::open(&db_path).unwrap();
    manager.init_repo_schema(&mut file_conn).unwrap();
    let indexed = manager.read_indexed_package(&pkg_path).unwrap();
    manager
        .insert_indexed_package(&mut file_conn, indexed)
        .unwrap();
    drop(file_conn);

    let hits = cached_binary_repo_owns_path("repo", &db_path, "usr/bin/ls").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].package_name, "test");
    assert_eq!(hits[0].path, "usr/bin/ls");
}

#[test]
fn test_search_cached_binary_repo_db_matches_name_and_provides() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repo.db");
    let mut conn = Connection::open(&db_path).unwrap();
    let manager = RepoManager::new(tmp.path().to_path_buf());
    manager.init_repo_schema(&mut conn).unwrap();

    conn.execute(
            "INSERT INTO packages (id, name, version, revision, description, homepage, license, filename, size, sha256, sha512)
             VALUES (1, 'foo', '1.2.3', 1, 'Foo package', 'https://example.test', 'MIT', 'foo-1.2.3-1-x86_64.depot.pkg.tar.zst', 1234, 'a', 'b')",
            [],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO provides (package_id, name) VALUES (1, 'libfoo.so')",
        [],
    )
    .unwrap();
    drop(conn);

    let name_hits = search_cached_binary_repo_db("testrepo", &db_path, "foo").unwrap();
    assert_eq!(name_hits.len(), 1);
    assert_eq!(name_hits[0].name, "foo");
    assert_eq!(name_hits[0].repo_name, "testrepo");
    assert!(name_hits[0].provides.iter().any(|p| p == "libfoo.so"));

    let provide_hits = search_cached_binary_repo_db("testrepo", &db_path, "libfoo").unwrap();
    assert_eq!(provide_hits.len(), 1);
    assert_eq!(provide_hits[0].name, "foo");
}

#[test]
fn test_find_cached_binary_repo_packages_by_group() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repo.db");
    let mut conn = Connection::open(&db_path).unwrap();
    let manager = RepoManager::new(tmp.path().to_path_buf());
    manager.init_repo_schema(&mut conn).unwrap();

    conn.execute(
            "INSERT INTO packages (id, name, version, revision, description, homepage, license, filename, size, sha256, sha512)
             VALUES (1, 'foo', '1.2.3', 1, 'Foo package', 'https://example.test', 'MIT', 'foo-1.2.3-1-x86_64.depot.pkg.tar.zst', 1234, 'a', 'b')",
            [],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO groups (package_id, name) VALUES (1, 'base')",
        [],
    )
    .unwrap();
    drop(conn);

    let hits = find_cached_binary_repo_packages_by_group("testrepo", &db_path, "base").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "foo");
    assert_eq!(hits[0].groups, vec!["base".to_string()]);
}

#[test]
#[cfg(unix)]
fn test_repo_owns_query_candidates_follow_rootfs_symlink_targets() {
    let rootfs = tempfile::tempdir().unwrap();
    let usr_bin = rootfs.path().join("usr/bin");
    fs::create_dir_all(&usr_bin).unwrap();
    fs::write(usr_bin.join("coreutils"), b"payload").unwrap();
    std::os::unix::fs::symlink("coreutils", usr_bin.join("ls")).unwrap();
    std::os::unix::fs::symlink("usr/bin", rootfs.path().join("bin")).unwrap();

    let ls_candidates = repo_owns_query_candidates(rootfs.path(), "/usr/bin/ls");
    assert!(
        ls_candidates
            .iter()
            .any(|candidate| candidate == "usr/bin/ls")
    );
    assert!(
        ls_candidates
            .iter()
            .any(|candidate| candidate == "usr/bin/coreutils")
    );

    let bin_candidates = repo_owns_query_candidates(rootfs.path(), "/bin/ls");
    assert!(bin_candidates.iter().any(|candidate| candidate == "bin/ls"));
    assert!(
        bin_candidates
            .iter()
            .any(|candidate| candidate == "usr/bin/coreutils")
    );
}

#[test]
fn test_find_cached_binary_repo_package_prefers_exact_name() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repo.db");
    let mut conn = Connection::open(&db_path).unwrap();
    let manager = RepoManager::new(tmp.path().to_path_buf());
    manager.init_repo_schema(&mut conn).unwrap();

    conn.execute(
            "INSERT INTO packages (id, name, version, revision, description, homepage, license, filename, size, sha256, sha512)
             VALUES (1, 'foo', '1.0', 1, NULL, NULL, NULL, 'foo-1.0-1.depot.pkg.tar.zst', 10, 'aa', 'bb')",
            [],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO packages (id, name, version, revision, description, homepage, license, filename, size, sha256, sha512)
             VALUES (2, 'bar', '1.0', 1, NULL, NULL, NULL, 'bar-1.0-1.depot.pkg.tar.zst', 10, 'cc', 'dd')",
            [],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO provides (package_id, name) VALUES (2, 'foo')",
        [],
    )
    .unwrap();
    drop(conn);

    let recs = find_cached_binary_repo_packages("repo", &db_path, "foo").unwrap();
    let rec = recs.first().expect("expected a match");
    assert_eq!(rec.name, "foo");
    assert_eq!(rec.filename, "foo-1.0-1.depot.pkg.tar.zst");
}

#[test]
fn test_find_cached_binary_repo_packages_matches_real_name_and_built_against() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repo.db");
    let mut conn = Connection::open(&db_path).unwrap();
    let manager = RepoManager::new(tmp.path().to_path_buf());
    manager.init_repo_schema(&mut conn).unwrap();

    conn.execute(
            "INSERT INTO packages (id, name, real_name, version, revision, abi_breaking, built_against, description, homepage, license, filename, size, sha256, sha512)
             VALUES (1, 'icu78', 'icu', '78.1', 1, 0, '', NULL, NULL, NULL, 'icu78.pkg', 10, 'aa', 'bb')",
            [],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO packages (id, name, real_name, version, revision, abi_breaking, built_against, description, homepage, license, filename, size, sha256, sha512)
             VALUES (2, 'app', NULL, '1.0', 1, 0, 'icu78', NULL, NULL, NULL, 'app.pkg', 10, 'cc', 'dd')",
            [],
        )
        .unwrap();
    conn.execute(
        "INSERT INTO dependencies (package_id, kind, name) VALUES (2, 'runtime', 'icu')",
        [],
    )
    .unwrap();
    drop(conn);

    let icu_matches = find_cached_binary_repo_packages("repo", &db_path, "icu").unwrap();
    assert_eq!(icu_matches.len(), 1);
    assert_eq!(icu_matches[0].name, "icu78");
    assert_eq!(icu_matches[0].real_name.as_deref(), Some("icu"));

    let app = find_cached_binary_repo_packages("repo", &db_path, "app")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(app.runtime_dependencies, vec!["icu".to_string()]);
    assert_eq!(app.built_against, vec!["icu78".to_string()]);
}

#[test]
fn test_verify_binary_package_record_checksums_accepts_valid_hashes() {
    use sha2::{Digest, Sha512};

    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg.depot.pkg.tar.zst");
    fs::write(&pkg, b"payload").unwrap();

    let sha512 = {
        let mut h = Sha512::new();
        h.update(b"payload");
        crate::hex::encode_lower(h.finalize())
    };

    let rec = BinaryRepoPackageRecord {
        repo_name: "repo".into(),
        name: "pkg".into(),
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: None,
        filename: "pkg.depot.pkg.tar.zst".into(),
        size: 7,
        sha512,
        description: None,
        homepage: None,
        license: None,
        provides: Vec::new(),
        conflicts: Vec::new(),
        replaces: Vec::new(),
        runtime_dependencies: Vec::new(),
        optional_dependencies: Vec::new(),
        groups: Vec::new(),
    };

    verify_binary_package_record_checksums(&pkg, &rec).unwrap();
}

#[test]
fn test_verify_binary_package_record_checksums_requires_valid_sha512() {
    use sha2::{Digest, Sha512};

    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg.depot.pkg.tar.zst");
    fs::write(&pkg, b"payload").unwrap();

    let mut rec = test_record_for_payload("pkg.depot.pkg.tar.zst", b"payload");
    verify_binary_package_record_checksums(&pkg, &rec).unwrap();

    let mut wrong_sha512 = Sha512::new();
    wrong_sha512.update(b"different payload");
    rec.sha512 = crate::hex::encode_lower(wrong_sha512.finalize());
    let err = verify_binary_package_record_checksums(&pkg, &rec).unwrap_err();
    assert!(err.to_string().contains("SHA-512 mismatch"));
}

fn test_record_for_payload(filename: &str, payload: &[u8]) -> BinaryRepoPackageRecord {
    use sha2::{Digest, Sha512};

    let sha512 = {
        let mut h = Sha512::new();
        h.update(payload);
        crate::hex::encode_lower(h.finalize())
    };

    BinaryRepoPackageRecord {
        repo_name: "repo".into(),
        name: "pkg".into(),
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        abi_breaking: false,
        built_against: Vec::new(),
        completed_at: None,
        filename: filename.to_string(),
        size: payload.len() as u64,
        sha512,
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

#[test]
fn test_fetch_binary_package_archive_requires_signature_when_unsigned_disallowed() {
    let rootfs = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();

    let filename = "pkg-1.0-1-x86_64.depot.pkg.tar.zst";
    let payload = b"package payload";
    std::fs::write(repo_dir.path().join(filename), payload).unwrap();

    let rec = test_record_for_payload(filename, payload);
    let repo_url = url::Url::from_directory_path(repo_dir.path())
        .expect("file URL")
        .to_string();
    let repo_cfg = crate::config::BinaryRepo {
        url: repo_url,
        allow_unsigned: false,
        ..Default::default()
    };

    let err =
        fetch_binary_package_archive("repo", &repo_cfg, rootfs.path(), &rec, cache_dir.path())
            .expect_err("missing detached signature should fail");
    assert!(err.to_string().to_ascii_lowercase().contains("signature"));
}

#[test]
fn test_fetch_binary_package_archive_verifies_signature_and_checksum() {
    let rootfs = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();

    let trusted_dir = crate::signing::trusted_public_keys_dir(rootfs.path());
    std::fs::create_dir_all(&trusted_dir).unwrap();

    let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    std::fs::write(
        trusted_dir.join("repo.pub"),
        keypair.pk.to_box().unwrap().to_bytes(),
    )
    .unwrap();

    let filename = "pkg-1.0-1-x86_64.depot.pkg.tar.zst";
    let payload = b"signed package payload";
    let package_path = repo_dir.path().join(filename);
    std::fs::write(&package_path, payload).unwrap();

    let sig = minisign::sign(
        Some(&keypair.pk),
        &keypair.sk,
        std::fs::File::open(&package_path).unwrap(),
        None,
        Some("test signature"),
    )
    .unwrap();
    std::fs::write(format!("{}.sig", package_path.display()), sig.to_bytes()).unwrap();

    let rec = test_record_for_payload(filename, payload);
    let repo_url = url::Url::from_directory_path(repo_dir.path())
        .expect("file URL")
        .to_string();
    let repo_cfg = crate::config::BinaryRepo {
        url: repo_url,
        allow_unsigned: false,
        ..Default::default()
    };

    let fetched =
        fetch_binary_package_archive("repo", &repo_cfg, rootfs.path(), &rec, cache_dir.path())
            .unwrap();
    assert_eq!(std::fs::read(&fetched).unwrap(), payload);
    assert!(PathBuf::from(format!("{}.sig", fetched.display())).exists());
}

#[test]
fn test_cache_binary_package_archive_supports_combined_integrity_verification() {
    let rootfs = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();

    let trusted_dir = crate::signing::trusted_public_keys_dir(rootfs.path());
    std::fs::create_dir_all(&trusted_dir).unwrap();

    let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    std::fs::write(
        trusted_dir.join("repo.pub"),
        keypair.pk.to_box().unwrap().to_bytes(),
    )
    .unwrap();

    let filename = "pkg-1.0-1-x86_64.depot.pkg.tar.zst";
    let payload = b"staged verification payload";
    let package_path = repo_dir.path().join(filename);
    std::fs::write(&package_path, payload).unwrap();

    let sig = minisign::sign(
        Some(&keypair.pk),
        &keypair.sk,
        std::fs::File::open(&package_path).unwrap(),
        None,
        Some("test signature"),
    )
    .unwrap();
    std::fs::write(format!("{}.sig", package_path.display()), sig.to_bytes()).unwrap();

    let rec = test_record_for_payload(filename, payload);
    let repo_url = url::Url::from_directory_path(repo_dir.path())
        .expect("file URL")
        .to_string();
    let repo_cfg = crate::config::BinaryRepo {
        url: repo_url,
        allow_unsigned: false,
        ..Default::default()
    };

    let cached = cache_binary_package_archive("repo", &repo_cfg, &rec, cache_dir.path())
        .expect("cache should succeed");
    assert!(cached.package_path.exists());
    assert!(cached.signature_path.exists());

    verify_binary_package_archive_checksums(&cached.package_path, &rec)
        .expect("checksum verification should succeed");
    let trusted_keys = crate::signing::load_trusted_public_keys(rootfs.path()).unwrap();
    verify_binary_package_archive_integrity_with_trusted_keys(
        "repo",
        &repo_cfg,
        &rec,
        &cached.package_path,
        &cached.signature_path,
        &trusted_keys,
    )
    .expect("combined integrity verification should succeed");

    let mut wrong_record = rec.clone();
    wrong_record.sha512 = crate::hex::encode_lower(Sha512::digest(b"wrong payload"));
    let error = verify_binary_package_archive_integrity_with_trusted_keys(
        "repo",
        &repo_cfg,
        &wrong_record,
        &cached.package_path,
        &cached.signature_path,
        &trusted_keys,
    )
    .expect_err("combined verification must reject a checksum mismatch");
    assert!(error.to_string().contains("SHA-512 mismatch"));
}

#[test]
fn test_fetch_binary_package_archive_allows_missing_signature_when_configured() {
    let rootfs = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();

    let filename = "pkg-1.0-1-x86_64.depot.pkg.tar.zst";
    let payload = b"unsigned package payload";
    std::fs::write(repo_dir.path().join(filename), payload).unwrap();

    let rec = test_record_for_payload(filename, payload);
    let repo_url = url::Url::from_directory_path(repo_dir.path())
        .expect("file URL")
        .to_string();
    let repo_cfg = crate::config::BinaryRepo {
        url: repo_url,
        allow_unsigned: true,
        ..Default::default()
    };

    let fetched =
        fetch_binary_package_archive("repo", &repo_cfg, rootfs.path(), &rec, cache_dir.path())
            .unwrap();
    assert_eq!(std::fs::read(&fetched).unwrap(), payload);
    assert!(!PathBuf::from(format!("{}.sig", fetched.display())).exists());
}

#[test]
fn test_copy_file_url_to_path_supports_file_scheme() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("repo.db.zst");
    let dst = tmp.path().join("copy.zst");
    fs::write(&src, b"repo-db").unwrap();

    let url = format!("file://{}", src.display());
    let outcome = copy_file_url_to_path(&url, &dst).unwrap();
    assert_eq!(outcome, FileUrlCopyOutcome::Copied);
    assert_eq!(fs::read(&dst).unwrap(), b"repo-db");
}

#[test]
fn test_copy_file_url_to_path_reports_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing.db.zst");
    let dst = tmp.path().join("copy.zst");

    let url = format!("file://{}", missing.display());
    let outcome = copy_file_url_to_path(&url, &dst).unwrap();
    assert_eq!(outcome, FileUrlCopyOutcome::Missing);
    assert!(!dst.exists());
}

#[test]
fn test_repo_db_fetch_cache_roundtrip_and_prunes_stale_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("repo.db");
    fs::write(&db_path, b"db").unwrap();

    let key = RepoDbFetchCacheKey {
        repo_name: "core".to_string(),
        base_url: "https://repo.example.test/core".to_string(),
        repo_db_rel: "repo.db.zst".to_string(),
        rootfs: PathBuf::from("/tmp/rootfs-test"),
        package_cache_dir: PathBuf::from("/tmp/pkg-cache-test"),
    };

    remember_repo_db_path(key.clone(), db_path.clone());
    assert_eq!(get_cached_repo_db_path(&key), Some(db_path.clone()));

    fs::remove_file(&db_path).unwrap();
    assert_eq!(get_cached_repo_db_path(&key), None);
}

#[test]
fn test_extract_html_href_targets_parses_common_forms() {
    let html = r#"
            <html><body>
              <a href="alpha.pub">alpha</a>
              <a HREF='nested/beta.pub'>beta</a>
              <a href=gamma.pub>gamma</a>
              <a href="../">parent</a>
            </body></html>
        "#;
    let hrefs = extract_html_href_targets(html);
    assert!(hrefs.iter().any(|h| h == "alpha.pub"));
    assert!(hrefs.iter().any(|h| h == "nested/beta.pub"));
    assert!(hrefs.iter().any(|h| h == "gamma.pub"));
    assert!(hrefs.iter().any(|h| h == "../"));
}

#[test]
fn test_list_repo_public_key_urls_reads_file_repo_keys_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    let keys_dir = repo_dir.join("keys");
    fs::create_dir_all(&keys_dir).unwrap();
    fs::write(keys_dir.join("repo.pub"), b"pubkey").unwrap();
    fs::write(keys_dir.join("ignore.txt"), b"nope").unwrap();
    fs::create_dir_all(keys_dir.join("subdir")).unwrap();

    let base_url = url::Url::from_directory_path(&repo_dir)
        .expect("file URL")
        .to_string();
    let client = reqwest::blocking::Client::builder().build().unwrap();
    let keys = list_repo_public_key_urls(&base_url, &client).unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].0, "repo.pub");
    assert!(keys[0].1.ends_with("/repo.pub"));
}

#[test]
fn test_list_repo_public_key_urls_probes_common_names_when_index_missing() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for _ in 0..7 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }

            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let (status, body) = if path == "/core/keys/vertex.pub" {
                ("200 OK", "trusted-key")
            } else {
                ("404 Not Found", "missing")
            };

            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            stream.flush().unwrap();
        }
    });

    let base_url = format!("http://{}/core", addr);
    let client = reqwest::blocking::Client::builder().build().unwrap();
    let keys = list_repo_public_key_urls(&base_url, &client).unwrap();
    server.join().unwrap();

    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].0, "vertex.pub");
    assert!(keys[0].1.ends_with("/core/keys/vertex.pub"));
}

#[test]
fn test_fetch_binary_repo_db_can_recover_from_stale_trusted_key() {
    use std::io::Write;

    struct AssumeYesReset;
    impl Drop for AssumeYesReset {
        fn drop(&mut self) {
            crate::ui::set_assume_yes(false);
        }
    }

    crate::ui::set_assume_yes(true);
    let _reset = AssumeYesReset;

    let rootfs = tempfile::tempdir().unwrap();
    let repo_dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();

    let stale_keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let repo_keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();

    let trusted_dir = crate::signing::trusted_public_keys_dir(rootfs.path());
    fs::create_dir_all(&trusted_dir).unwrap();
    fs::write(
        trusted_dir.join("vertex.pub"),
        stale_keypair.pk.to_box().unwrap().to_bytes(),
    )
    .unwrap();

    let repo_keys_dir = repo_dir.path().join("keys");
    fs::create_dir_all(&repo_keys_dir).unwrap();
    fs::write(
        repo_keys_dir.join("vertex.pub"),
        repo_keypair.pk.to_box().unwrap().to_bytes(),
    )
    .unwrap();

    let repo_db_path = repo_dir.path().join("repo.db.zst");
    let mut encoder =
        zstd::stream::write::Encoder::new(fs::File::create(&repo_db_path).unwrap(), 3).unwrap();
    encoder.write_all(b"repo-db-content").unwrap();
    encoder.finish().unwrap();

    let sig = minisign::sign(
        Some(&repo_keypair.pk),
        &repo_keypair.sk,
        fs::File::open(&repo_db_path).unwrap(),
        None,
        Some("repo db signature"),
    )
    .unwrap();
    fs::write(repo_dir.path().join("repo.db.zst.sig"), sig.to_bytes()).unwrap();

    let repo_cfg = crate::config::BinaryRepo {
        url: url::Url::from_directory_path(repo_dir.path())
            .expect("file URL")
            .to_string(),
        allow_unsigned: false,
        ..Default::default()
    };

    let sqlite_db =
        fetch_binary_repo_db("core", &repo_cfg, rootfs.path(), cache_dir.path()).unwrap();
    assert_eq!(fs::read(sqlite_db).unwrap(), b"repo-db-content");

    let installed_key = trusted_dir.join("core-vertex.pub");
    assert!(installed_key.exists());
    assert_eq!(
        fs::read(installed_key).unwrap(),
        repo_keypair.pk.to_box().unwrap().to_bytes()
    );
}

#[test]
fn test_normalize_git_mirror_url_converts_file_scheme() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo.git");
    fs::create_dir_all(&repo_dir).unwrap();

    let url = format!("file://{}", repo_dir.display());
    let normalized = normalize_git_mirror_url(&url).unwrap();
    assert_eq!(normalized, repo_dir.to_string_lossy());
}
