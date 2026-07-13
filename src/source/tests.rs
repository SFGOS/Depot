use super::*;
use crate::package::{
    Alternatives, Build, BuildFlags, BuildType, Dependencies, ManualSource, PackageInfo,
    PackageSpec, Source,
};
use git2::{Oid, Repository};
use std::path::Path;

fn commit_file(repo: &Repository, workdir: &Path, rel: &str, data: &str) -> Oid {
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

fn make_git_source_spec(source_url: String, extract_dir: &str) -> PackageSpec {
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
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: source_url,
            sha256: "skip".into(),
            extract_dir: extract_dir.into(),
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

fn make_remote_git_repo() -> (tempfile::TempDir, String, Oid, Oid) {
    let tmp = tempfile::tempdir().unwrap();
    let remote_dir = tmp.path().join("origin.git");
    let workdir = tmp.path().join("work");

    Repository::init_bare(&remote_dir).unwrap();
    let repo = Repository::init(&workdir).unwrap();
    let tagged = commit_file(&repo, &workdir, "README", "tagged\n");
    let tag_target = repo.find_object(tagged, None).unwrap();
    repo.tag_lightweight("v1.0.0", &tag_target, false).unwrap();
    let hashed = commit_file(&repo, &workdir, "README", "hashed\n");

    let branch_ref = repo.head().unwrap().name().unwrap().to_string();
    let mut remote = repo.remote("origin", remote_dir.to_str().unwrap()).unwrap();
    let push_specs = [
        format!("{branch_ref}:{branch_ref}"),
        "refs/tags/v1.0.0:refs/tags/v1.0.0".to_string(),
    ];
    let push_spec_refs: Vec<&String> = push_specs.iter().collect();
    remote.push(&push_spec_refs, None).unwrap();

    let remote_url = url::Url::from_file_path(&remote_dir).unwrap().to_string();
    (tmp, remote_url, tagged, hashed)
}

fn mk_spec_with_manuals(spec_dir: PathBuf, manuals: Vec<ManualSource>) -> PackageSpec {
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
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: manuals,
        source: vec![Source {
            url: "https://example.com/src.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "src".into(),
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
        spec_dir,
    }
}

#[test]
fn split_git_url_accepts_git_with_rev() {
    let (base, rev) = split_git_url("https://example.com/repo.git#v1.2.3").unwrap();
    assert_eq!(base, "https://example.com/repo.git");
    assert_eq!(rev, "v1.2.3");
}

#[test]
fn split_git_url_accepts_bare_git_url() {
    let (base, rev) = split_git_url("https://example.com/repo.git").unwrap();
    assert_eq!(base, "https://example.com/repo.git");
    assert_eq!(rev, "HEAD");
}

#[test]
fn split_git_url_accepts_bare_git_scheme_url() {
    let (base, rev) = split_git_url("git://git.suckless.org/ubase").unwrap();
    assert_eq!(base, "git://git.suckless.org/ubase");
    assert_eq!(rev, "HEAD");
}

#[test]
fn split_git_url_rejects_archive_urls() {
    assert!(split_git_url("https://example.com/foo.tar.gz#deadbeef").is_none());
    assert!(split_git_url("https://example.com/foo.zip#v1").is_none());
}

#[test]
fn split_git_url_empty_rev_defaults_to_head() {
    let (base, rev) = split_git_url("https://example.com/repo.git#").unwrap();
    assert_eq!(base, "https://example.com/repo.git");
    assert_eq!(rev, "HEAD");
}

#[test]
fn split_git_url_accepts_expanded_tag_or_hash_revision() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "json".into(),
            real_name: None,
            version: "3.11.3".into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://github.com/nlohmann/json.git#v$version".into(),
            sha256: "skip".into(),
            extract_dir: "json-$version".into(),
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
    };

    let expanded = spec.expand_vars(&spec.source[0].url);
    let (base, rev) = split_git_url(&expanded).unwrap();
    assert_eq!(base, "https://github.com/nlohmann/json.git");
    assert_eq!(rev, "v3.11.3");

    let (base, rev) =
        split_git_url("https://github.com/nlohmann/json.git#0123456789abcdef").unwrap();
    assert_eq!(base, "https://github.com/nlohmann/json.git");
    assert_eq!(rev, "0123456789abcdef");
}

#[test]
fn split_hg_url_accepts_revision_and_default_tip() {
    let (base, rev) = split_hg_url("hg+https://hg.example.test/repo#v1").unwrap();
    assert_eq!(base, "https://hg.example.test/repo");
    assert_eq!(rev, "v1");

    let (base, rev) = split_hg_url("hg+https://hg.example.test/repo").unwrap();
    assert_eq!(base, "https://hg.example.test/repo");
    assert_eq!(rev, "tip");
}

#[test]
fn prepare_one_rejects_cherry_pick_for_non_git_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");
    let build_dir = tmp.path().join("build");
    let mut spec = mk_spec_with_manuals(PathBuf::from("."), Vec::new());
    spec.source[0].url = "https://example.com/foo.tar.gz".into();
    spec.source[0].cherry_pick = vec!["deadbeef".into()];

    let err = prepare_one(&spec, &spec.source[0], &cache_dir, &build_dir)
        .expect_err("non-git source with cherry_pick must be rejected");
    assert!(
        err.to_string()
            .contains("source.cherry_pick is only supported for git sources")
    );
}

#[test]
fn prepare_one_checks_out_git_tag_revision() {
    let (_tmp, remote_url, tagged, _hashed) = make_remote_git_repo();
    let cache_dir = tempfile::tempdir().unwrap();
    let build_dir = tempfile::tempdir().unwrap();
    let spec = make_git_source_spec(format!("{remote_url}#v1.0.0"), "src-tag");

    let checkout_dir =
        prepare_one(&spec, &spec.source[0], cache_dir.path(), build_dir.path()).unwrap();
    let repo = Repository::open(&checkout_dir).unwrap();

    assert_eq!(repo.head().unwrap().target().unwrap(), tagged);
    assert_eq!(
        std::fs::read_to_string(checkout_dir.join("README")).unwrap(),
        "tagged\n"
    );
}

#[test]
fn prepare_one_checks_out_git_commit_hash_revision() {
    let (_tmp, remote_url, _tagged, hashed) = make_remote_git_repo();
    let cache_dir = tempfile::tempdir().unwrap();
    let build_dir = tempfile::tempdir().unwrap();
    let spec = make_git_source_spec(format!("{remote_url}#{hashed}"), "src-hash");

    let checkout_dir =
        prepare_one(&spec, &spec.source[0], cache_dir.path(), build_dir.path()).unwrap();
    let repo = Repository::open(&checkout_dir).unwrap();

    assert_eq!(repo.head().unwrap().target().unwrap(), hashed);
    assert_eq!(
        std::fs::read_to_string(checkout_dir.join("README")).unwrap(),
        "hashed\n"
    );
}

#[test]
fn verify_file_hash_accepts_multiple_algorithms() {
    use sha1::Sha1;
    use sha2::{Digest, Sha256, Sha512};

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"abc").unwrap();

    let sha256_hex = {
        let mut h = Sha256::new();
        h.update(b"abc");
        crate::hex::encode_lower(h.finalize())
    };
    let sha512_hex = {
        let mut h = Sha512::new();
        h.update(b"abc");
        crate::hex::encode_lower(h.finalize())
    };
    let sha1_hex = {
        let mut h = Sha1::new();
        h.update(b"abc");
        crate::hex::encode_lower(h.finalize())
    };
    let md5_hex = format!("{:x}", md5::compute(b"abc"));
    let b2_hex = b2sum_rust::Blake2bSum::new(64)
        .read(tmp.path())
        .to_ascii_lowercase();

    assert!(verify_file_hash(tmp.path(), &sha256_hex).unwrap());
    assert!(verify_file_hash(tmp.path(), &format!("sha256:{}", sha256_hex)).unwrap());
    assert!(verify_file_hash(tmp.path(), &format!("sha512:{}", sha512_hex)).unwrap());
    assert!(verify_file_hash(tmp.path(), &format!("sha1:{}", sha1_hex)).unwrap());
    assert!(verify_file_hash(tmp.path(), &format!("md5:{}", md5_hex)).unwrap());
    assert!(verify_file_hash(tmp.path(), &format!("b2:{}", b2_hex)).unwrap());
    assert!(verify_file_hash(tmp.path(), &format!("b2sum:{}", b2_hex)).unwrap());
    assert!(verify_file_hash(tmp.path(), &format!(":{}", sha256_hex)).unwrap());
    assert!(!verify_file_hash(tmp.path(), "md5:deadbeef").unwrap());
}

#[test]
fn build_blocking_client_with_and_without_timeout() {
    use std::time::Duration;
    let ua = "depot/test";
    let c1 = build_blocking_client(ua, None).expect("client build failed");
    assert!(c1.get("https://example.com").build().is_ok());

    let c2 = build_blocking_client(ua, Some(Duration::from_secs(5))).expect("client build failed");
    assert!(c2.get("https://example.com").build().is_ok());
}

#[test]
fn copy_manual_sources_local_file_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let cache_dir = tmp.path().join("cache");
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::write(spec_dir.join("manual.patch"), "patch-data").unwrap();

    let spec = mk_spec_with_manuals(
        spec_dir.clone(),
        vec![ManualSource {
            file: Some("manual.patch".into()),
            files: Vec::new(),
            url: None,
            urls: Vec::new(),
            sha256: None,
            dest: None,
        }],
    );

    copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
    assert_eq!(
        std::fs::read_to_string(build_dir.join("manual.patch")).unwrap(),
        "patch-data"
    );
}

#[test]
fn copy_manual_sources_url_mode_file_scheme() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let cache_dir = tmp.path().join("cache");
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(&spec_dir).unwrap();
    let remote_file = tmp.path().join("remote-resource.txt");
    std::fs::write(&remote_file, "remote-data").unwrap();
    let url = format!("file://{}", remote_file.display());

    let spec = mk_spec_with_manuals(
        spec_dir,
        vec![ManualSource {
            file: None,
            files: Vec::new(),
            url: Some(url),
            urls: Vec::new(),
            sha256: Some("skip".into()),
            dest: Some("assets/manual.txt".into()),
        }],
    );

    copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
    assert_eq!(
        std::fs::read_to_string(build_dir.join("assets/manual.txt")).unwrap(),
        "remote-data"
    );
}

#[test]
fn preflight_manual_sources_accepts_git_url() {
    let (_tmp, remote_url, _tagged, hashed) = make_remote_git_repo();
    let spec = mk_spec_with_manuals(
        PathBuf::from("."),
        vec![ManualSource {
            file: None,
            files: Vec::new(),
            url: Some(format!("{remote_url}#{hashed}")),
            urls: Vec::new(),
            sha256: None,
            dest: None,
        }],
    );
    let cache_dir = tempfile::tempdir().unwrap();

    preflight_manual_sources(&spec, cache_dir.path()).unwrap();
}

#[test]
fn copy_manual_sources_git_url_mode_checks_out_repository() {
    let (_tmp, remote_url, _tagged, hashed) = make_remote_git_repo();
    let spec = mk_spec_with_manuals(
        PathBuf::from("."),
        vec![ManualSource {
            file: None,
            files: Vec::new(),
            url: Some(format!("{remote_url}#{hashed}")),
            urls: Vec::new(),
            sha256: None,
            dest: None,
        }],
    );
    let cache_dir = tempfile::tempdir().unwrap();
    let build_dir = tempfile::tempdir().unwrap();

    copy_manual_sources(&spec, cache_dir.path(), build_dir.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(build_dir.path().join("origin/README")).unwrap(),
        "hashed\n"
    );
}

#[test]
fn copy_manual_sources_multi_files_in_one_block() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let cache_dir = tmp.path().join("cache");
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(spec_dir.join("pam")).unwrap();
    std::fs::write(spec_dir.join("pam/other"), "other").unwrap();
    std::fs::write(spec_dir.join("pam/system-auth"), "auth").unwrap();

    let spec = mk_spec_with_manuals(
        spec_dir.clone(),
        vec![ManualSource {
            file: None,
            files: vec!["pam/other".into(), "pam/system-auth".into()],
            url: None,
            urls: Vec::new(),
            sha256: None,
            dest: None,
        }],
    );

    copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
    assert_eq!(
        std::fs::read_to_string(build_dir.join("pam/other")).unwrap(),
        "other"
    );
    assert_eq!(
        std::fs::read_to_string(build_dir.join("pam/system-auth")).unwrap(),
        "auth"
    );
}

#[test]
fn copy_manual_sources_expands_carch_in_files_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let cache_dir = tmp.path().join("cache");
    let build_dir = tmp.path().join("build");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::write(spec_dir.join("build.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::write(spec_dir.join("config.armv7"), "armv7-config").unwrap();

    let mut spec = mk_spec_with_manuals(
        spec_dir.clone(),
        vec![ManualSource {
            file: None,
            files: vec!["build.sh".into(), "config.$CARCH".into()],
            url: None,
            urls: Vec::new(),
            sha256: None,
            dest: None,
        }],
    );
    spec.build.flags.carch = "armv7".into();

    copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
    assert_eq!(
        std::fs::read_to_string(build_dir.join("build.sh")).unwrap(),
        "#!/bin/sh\necho hi\n"
    );
    assert_eq!(
        std::fs::read_to_string(build_dir.join("config.armv7")).unwrap(),
        "armv7-config"
    );
}
