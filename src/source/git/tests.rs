use super::*;
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

#[test]
fn checkout_rev_head_falls_back_to_remote_branch_when_local_head_is_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let workdir = temp.path().join("repo");
    std::fs::create_dir_all(&workdir).unwrap();
    let repo = Repository::init(&workdir).unwrap();

    let commit_oid = commit_file(&repo, &workdir, "README", "hello");
    repo.reference(
        "refs/remotes/origin/main",
        commit_oid,
        true,
        "test setup: remote tracking ref",
    )
    .unwrap();
    repo.reference_symbolic(
        "HEAD",
        "refs/heads/HEAD",
        true,
        "test setup: break local HEAD",
    )
    .unwrap();

    checkout_rev(&repo, "HEAD").unwrap();
    assert!(repo.head_detached().unwrap());
    assert_eq!(repo.head().unwrap().target().unwrap(), commit_oid);
}

#[test]
fn checkout_rev_resolves_named_branch_from_remote_tracking_refs() {
    let temp = tempfile::tempdir().unwrap();
    let workdir = temp.path().join("repo");
    std::fs::create_dir_all(&workdir).unwrap();
    let repo = Repository::init(&workdir).unwrap();

    let commit_oid = commit_file(&repo, &workdir, "src/main.rs", "fn main() {}");
    repo.reference(
        "refs/remotes/origin/feature",
        commit_oid,
        true,
        "test setup: remote feature branch",
    )
    .unwrap();

    checkout_rev(&repo, "feature").unwrap();
    assert_eq!(repo.head().unwrap().target().unwrap(), commit_oid);
}

#[test]
fn apply_cherry_picks_applies_commit_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let workdir = temp.path().join("repo");
    std::fs::create_dir_all(&workdir).unwrap();
    let repo = Repository::init(&workdir).unwrap();

    let base = commit_file(&repo, &workdir, "README", "base");
    let picked = commit_file(&repo, &workdir, "README", "picked");

    checkout_rev(&repo, &base.to_string()).unwrap();
    apply_cherry_picks(&repo, &[picked.to_string()]).unwrap();

    let head = repo.head().unwrap().target().unwrap();
    assert_eq!(head, picked);
    assert_eq!(
        std::fs::read_to_string(workdir.join("README")).unwrap(),
        "picked"
    );
}

#[test]
fn apply_cherry_picks_errors_for_unknown_rev() {
    let temp = tempfile::tempdir().unwrap();
    let workdir = temp.path().join("repo");
    std::fs::create_dir_all(&workdir).unwrap();
    let repo = Repository::init(&workdir).unwrap();

    let base = commit_file(&repo, &workdir, "README", "base");
    checkout_rev(&repo, &base.to_string()).unwrap();

    let err = apply_cherry_picks(&repo, &["deadbeef".to_string()])
        .expect_err("unknown cherry-pick rev should fail");
    assert!(
        err.to_string()
            .contains("Could not resolve cherry-pick rev")
    );
}

#[test]
fn fetch_attempts_for_head_prefers_heads_only_before_full_fetch() {
    let attempts = fetch_attempts_for_rev("HEAD");
    assert!(matches!(
        attempts.as_slice(),
        [FetchAttempt::HeadRefs, FetchAttempt::FullRefs]
    ));
}

#[test]
fn fetch_attempts_for_named_revision_try_tag_then_branch_then_fallback() {
    let attempts = fetch_attempts_for_rev("v1.2.3");
    assert_eq!(attempts.len(), 3);
    assert!(
        matches!(&attempts[0], FetchAttempt::Tag(tag) if tag == "+refs/tags/v1.2.3:refs/tags/v1.2.3")
    );
    assert!(
        matches!(&attempts[1], FetchAttempt::Branch(branch) if branch == "+refs/heads/v1.2.3:refs/heads/v1.2.3")
    );
    assert!(matches!(attempts[2], FetchAttempt::FullRefs));
}

#[test]
fn fetch_attempts_for_oid_use_full_fetch_only() {
    let attempts = fetch_attempts_for_rev("0123456789abcdef");
    assert!(matches!(attempts.as_slice(), [FetchAttempt::FullRefs]));
}

#[test]
fn full_fetch_attempt_includes_heads_and_tags_refspecs() {
    let refspecs = FetchAttempt::FullRefs.refspecs();
    assert_eq!(refspecs, vec![ALL_HEADS_REFSPEC, ALL_TAGS_REFSPEC]);
}

#[test]
fn should_skip_fetch_for_cached_tag_revisions() {
    let temp = tempfile::tempdir().unwrap();
    let workdir = temp.path().join("repo");
    std::fs::create_dir_all(&workdir).unwrap();
    let repo = Repository::init(&workdir).unwrap();

    let commit_oid = commit_file(&repo, &workdir, "README", "hello");
    let tag_target = repo.find_object(commit_oid, None).unwrap();
    repo.tag_lightweight("v1.0.0", &tag_target, false).unwrap();

    assert!(should_skip_fetch_for_cached_revs(&repo, "v1.0.0", &[]));
    assert!(!should_skip_fetch_for_cached_revs(&repo, "main", &[]));
    assert!(!should_skip_fetch_for_cached_revs(&repo, "HEAD", &[]));
}

#[test]
fn should_not_skip_fetch_when_cherry_pick_rev_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let workdir = temp.path().join("repo");
    std::fs::create_dir_all(&workdir).unwrap();
    let repo = Repository::init(&workdir).unwrap();

    let commit_oid = commit_file(&repo, &workdir, "README", "hello");
    let tag_target = repo.find_object(commit_oid, None).unwrap();
    repo.tag_lightweight("v1.0.0", &tag_target, false).unwrap();

    assert!(!should_skip_fetch_for_cached_revs(
        &repo,
        "v1.0.0",
        &[String::from("deadbeef")]
    ));
}

#[test]
fn git_operation_should_continue_allows_normal_progress() {
    let bar = ProgressBar::hidden();
    crate::interrupts::reset();
    assert!(git_operation_should_continue(Some(&bar)));
}

#[test]
fn ensure_valid_local_head_prefers_main_when_head_branch_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let bare_dir = temp.path().join("bare.git");
    let workdir = temp.path().join("work");
    let repo = Repository::init_bare(&bare_dir).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let work_repo = Repository::init(&workdir).unwrap();
    work_repo.set_head("refs/heads/main").unwrap();
    commit_file(&work_repo, &workdir, "README", "hello");
    let topic_branch = work_repo
        .branch(
            "topic",
            &work_repo.head().unwrap().peel_to_commit().unwrap(),
            false,
        )
        .unwrap();
    drop(topic_branch);
    let mut remote = work_repo
        .remote("origin", bare_dir.to_str().unwrap())
        .unwrap();
    remote
        .push(
            &[
                "refs/heads/main:refs/heads/main",
                "refs/heads/topic:refs/heads/topic",
            ],
            None,
        )
        .unwrap();
    repo.set_head("refs/heads/missing").unwrap();

    ensure_valid_local_head(&repo).unwrap();

    assert_eq!(
        repo.head().unwrap().resolve().unwrap().name(),
        Ok("refs/heads/main")
    );
}

#[test]
fn checkout_head_succeeds_with_bare_mirror_heads_only_fetch() {
    let temp = tempfile::tempdir().unwrap();
    let origin_dir = temp.path().join("origin.git");
    let workdir = temp.path().join("work");
    let cache_dir = temp.path().join("cache");
    let checkout_dir = temp.path().join("checkout");
    std::fs::create_dir_all(&workdir).unwrap();

    let origin = Repository::init_bare(&origin_dir).unwrap();
    let repo = Repository::init(&workdir).unwrap();
    repo.set_head("refs/heads/main").unwrap();
    let commit_oid = commit_file(&repo, &workdir, "README", "hello\n");
    let mut remote = repo.remote("origin", origin_dir.to_str().unwrap()).unwrap();
    remote
        .push(&["refs/heads/main:refs/heads/main"], None)
        .unwrap();
    origin.set_head("refs/heads/main").unwrap();

    let origin_url = url::Url::from_file_path(&origin_dir).unwrap().to_string();
    checkout(
        &origin_url,
        "HEAD",
        &checkout_dir,
        &cache_dir,
        "test-pkg",
        &[],
    )
    .unwrap();

    let checkout_repo = Repository::open(&checkout_dir).unwrap();
    assert_eq!(checkout_repo.head().unwrap().target(), Some(commit_oid));
    assert_eq!(
        std::fs::read_to_string(checkout_dir.join("README")).unwrap(),
        "hello\n"
    );
}

#[test]
fn checkout_fetches_cherry_pick_revs_after_tag_checkout_resolves() {
    let temp = tempfile::tempdir().unwrap();
    let origin_dir = temp.path().join("origin.git");
    let workdir = temp.path().join("work");
    let cache_dir = temp.path().join("cache");
    let checkout_dir = temp.path().join("checkout");
    std::fs::create_dir_all(&workdir).unwrap();

    let origin = Repository::init_bare(&origin_dir).unwrap();
    let repo = Repository::init(&workdir).unwrap();
    repo.set_head("refs/heads/main").unwrap();

    let base = commit_file(&repo, &workdir, "README", "base\n");
    let release_target = repo.find_object(base, None).unwrap();
    repo.tag_lightweight("v1.0.0", &release_target, false)
        .unwrap();

    let base_commit = repo.find_commit(base).unwrap();
    repo.branch("topic", &base_commit, false).unwrap();
    repo.set_head("refs/heads/topic").unwrap();
    let mut branch_checkout = git2::build::CheckoutBuilder::new();
    branch_checkout.force();
    repo.checkout_head(Some(&mut branch_checkout)).unwrap();
    let cherry_pick = commit_file(&repo, &workdir, "TOPIC", "topic\n");

    let mut remote = repo.remote("origin", origin_dir.to_str().unwrap()).unwrap();
    remote
        .push(
            &[
                "refs/heads/main:refs/heads/main",
                "refs/heads/topic:refs/heads/topic",
                "refs/tags/v1.0.0:refs/tags/v1.0.0",
            ],
            None,
        )
        .unwrap();
    origin.set_head("refs/heads/main").unwrap();

    let origin_url = url::Url::from_file_path(&origin_dir).unwrap().to_string();
    checkout(
        &origin_url,
        "v1.0.0",
        &checkout_dir,
        &cache_dir,
        "test-pkg",
        &[cherry_pick.to_string()],
    )
    .unwrap();

    let checkout_repo = Repository::open(&checkout_dir).unwrap();
    assert_ne!(checkout_repo.head().unwrap().target(), Some(base));
    assert_eq!(
        std::fs::read_to_string(checkout_dir.join("TOPIC")).unwrap(),
        "topic\n"
    );
}

#[test]
fn checkout_resolves_annotated_tags_from_remote() {
    let temp = tempfile::tempdir().unwrap();
    let origin_dir = temp.path().join("origin.git");
    let workdir = temp.path().join("work");
    let cache_dir = temp.path().join("cache");
    let checkout_dir = temp.path().join("checkout");
    std::fs::create_dir_all(&workdir).unwrap();

    let origin = Repository::init_bare(&origin_dir).unwrap();
    let repo = Repository::init(&workdir).unwrap();
    repo.set_head("refs/heads/main").unwrap();

    let release_commit = commit_file(&repo, &workdir, "README", "release\n");
    let release_target = repo.find_object(release_commit, None).unwrap();
    let sig = git2::Signature::now("depot-test", "depot@example.test").unwrap();
    repo.tag("v1.0.0", &release_target, &sig, "release tag", false)
        .unwrap();

    let mut remote = repo.remote("origin", origin_dir.to_str().unwrap()).unwrap();
    remote
        .push(
            &[
                "refs/heads/main:refs/heads/main",
                "refs/tags/v1.0.0:refs/tags/v1.0.0",
            ],
            None,
        )
        .unwrap();
    origin.set_head("refs/heads/main").unwrap();

    let origin_url = url::Url::from_file_path(&origin_dir).unwrap().to_string();
    checkout(
        &origin_url,
        "v1.0.0",
        &checkout_dir,
        &cache_dir,
        "test-pkg",
        &[],
    )
    .unwrap();

    let checkout_repo = Repository::open(&checkout_dir).unwrap();
    assert_eq!(checkout_repo.head().unwrap().target(), Some(release_commit));
    assert_eq!(
        std::fs::read_to_string(checkout_dir.join("README")).unwrap(),
        "release\n"
    );
}
