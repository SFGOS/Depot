//! Git source support via libgit2 (git2 crate)

use anyhow::{Context, Result};
use git2::{Cred, CredentialType, FetchOptions, Oid, RemoteCallbacks, Repository};
use inquire::Password;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

/// Checkout a repository URL at a specific revision into `checkout_dir`.
///
/// The URL is expected to be the base URL (without the `#rev` fragment).
/// The revision may be a tag name, branch name, or commit hash.
///
/// Repositories are mirrored under `git_cache_dir` to avoid repeated network fetches.
pub fn checkout(
    url: &str,
    rev: &str,
    checkout_dir: &Path,
    git_cache_dir: &Path,
    pkgname: &str,
    cherry_pick_revs: &[String],
) -> Result<()> {
    fs::create_dir_all(git_cache_dir).with_context(|| {
        format!(
            "Failed to create git cache dir: {}",
            git_cache_dir.display()
        )
    })?;

    let mirror_dir = git_cache_dir.join(mirror_key(url));
    ensure_mirror(url, &mirror_dir, pkgname)?;

    if checkout_dir.exists() {
        fs::remove_dir_all(checkout_dir).with_context(|| {
            format!(
                "Failed to remove existing checkout: {}",
                checkout_dir.display()
            )
        })?;
    }

    // Clone from local mirror for speed.
    let mirror_url = mirror_dir
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid mirror path"))?;

    crate::log_info!("Cloning git source into {}...", checkout_dir.display());
    Repository::clone(mirror_url, checkout_dir)
        .with_context(|| format!("Failed to clone from mirror for {}", url))?;

    let repo = Repository::open(checkout_dir)?;
    checkout_rev(&repo, rev).with_context(|| format!("Failed to checkout revision '{}'", rev))?;
    apply_cherry_picks(&repo, cherry_pick_revs)?;

    Ok(())
}

fn apply_cherry_picks(repo: &Repository, cherry_pick_revs: &[String]) -> Result<()> {
    if cherry_pick_revs.is_empty() {
        return Ok(());
    }

    crate::log_info!("Applying {} git cherry-pick(s)...", cherry_pick_revs.len());

    for rev in cherry_pick_revs {
        let rev = rev.trim();
        if rev.is_empty() {
            anyhow::bail!("Encountered empty entry in source.cherry_pick");
        }

        let obj = resolve_rev_object(repo, rev)
            .with_context(|| format!("Could not resolve cherry-pick rev: {}", rev))?;
        let commit = obj
            .peel_to_commit()
            .with_context(|| format!("Could not peel cherry-pick rev to commit: {}", rev))?;
        repo.cherrypick(&commit, None)
            .with_context(|| format!("Failed to cherry-pick rev: {}", rev))?;

        let parent = repo
            .head()
            .with_context(|| format!("Failed to read HEAD during cherry-pick {}", rev))?
            .peel_to_commit()
            .with_context(|| format!("Failed to peel HEAD to commit during {}", rev))?;

        let mut index = repo
            .index()
            .with_context(|| format!("Failed to open index after cherry-pick {}", rev))?;
        if index.has_conflicts() {
            anyhow::bail!("Cherry-pick produced conflicts for rev {}", rev);
        }

        let tree_id = index
            .write_tree()
            .with_context(|| format!("Failed to write tree after cherry-pick {}", rev))?;
        let tree = repo
            .find_tree(tree_id)
            .with_context(|| format!("Failed to find tree after cherry-pick {}", rev))?;
        let message = commit.summary().unwrap_or("cherry-pick");
        let new_head = repo
            .commit(
                None,
                &commit.author(),
                &commit.committer(),
                message,
                &tree,
                &[&parent],
            )
            .with_context(|| format!("Failed to create cherry-pick commit for rev {}", rev))?;
        repo.set_head_detached(new_head)
            .with_context(|| format!("Failed to update detached HEAD after cherry-pick {}", rev))?;

        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        let obj = repo
            .find_object(new_head, None)
            .with_context(|| format!("Failed to resolve new HEAD after cherry-pick {}", rev))?;
        repo.checkout_tree(&obj, Some(&mut checkout))
            .with_context(|| format!("Failed to update worktree after cherry-pick {}", rev))?;

        crate::log_info!("  cherry_pick: {}", rev);
    }

    Ok(())
}

fn mirror_key(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    format!("{:x}", digest)
}

fn ensure_mirror(url: &str, mirror_dir: &Path, pkgname: &str) -> Result<()> {
    if !mirror_dir.exists() {
        crate::log_info!("Cloning git mirror for {} ({})...", pkgname, url);
        let mut fo = FetchOptions::new();
        fo.remote_callbacks(remote_callbacks());
        let mut builder = git2::build::RepoBuilder::new();
        builder.fetch_options(fo);
        builder.bare(true);
        builder
            .clone(url, mirror_dir)
            .with_context(|| format!("Failed to clone git mirror: {}", url))?;
        return Ok(());
    }

    // Fetch updates
    let repo = Repository::open_bare(mirror_dir)
        .with_context(|| format!("Failed to open git mirror: {}", mirror_dir.display()))?;

    let mut remote = repo
        .find_remote("origin")
        .or_else(|_| repo.remote_anonymous(url))
        .with_context(|| format!("Failed to create remote for {}", url))?;

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(remote_callbacks());

    // Fetch all remote refs (tags + heads). Empty refspec uses default.
    remote
        .fetch(&[] as &[&str], Some(&mut fo), None)
        .with_context(|| format!("Failed to fetch updates for {}", url))?;

    Ok(())
}

fn checkout_rev(repo: &Repository, rev: &str) -> Result<()> {
    let obj = resolve_rev_object(repo, rev)
        .with_context(|| format!("Could not resolve git rev: {}", rev))?;

    // Peel tags to commit if needed.
    let commit = obj
        .peel_to_commit()
        .with_context(|| format!("Could not peel rev to commit: {}", rev))?;

    let oid: Oid = commit.id();
    repo.set_head_detached(oid)?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.checkout_tree(commit.as_object(), Some(&mut checkout))?;
    Ok(())
}

fn resolve_rev_object<'a>(repo: &'a Repository, rev: &str) -> Result<git2::Object<'a>> {
    if let Ok(obj) = repo.revparse_single(rev) {
        return Ok(obj);
    }

    if rev.eq_ignore_ascii_case("HEAD") {
        if let Ok(obj) = repo.revparse_single("refs/remotes/origin/HEAD") {
            return Ok(obj);
        }
        if let Ok(obj) = repo.revparse_single("origin/HEAD") {
            return Ok(obj);
        }
        if let Ok(obj) = repo.revparse_single("FETCH_HEAD") {
            return Ok(obj);
        }

        // Some mirror clones may not have a valid local HEAD but still have remote branch refs.
        if let Some(obj) = resolve_remote_head_like(repo)? {
            return Ok(obj);
        }
    } else {
        if let Ok(obj) = repo.revparse_single(&format!("refs/tags/{rev}")) {
            return Ok(obj);
        }
        if let Ok(obj) = repo.revparse_single(&format!("refs/heads/{rev}")) {
            return Ok(obj);
        }
        if let Ok(obj) = repo.revparse_single(&format!("refs/remotes/origin/{rev}")) {
            return Ok(obj);
        }
    }

    if let Ok(oid) = Oid::from_str(rev)
        && let Ok(obj) = repo.find_object(oid, None)
    {
        return Ok(obj);
    }

    anyhow::bail!("revspec not found")
}

fn resolve_remote_head_like<'a>(repo: &'a Repository) -> Result<Option<git2::Object<'a>>> {
    let mut candidates: Vec<(String, Oid)> = Vec::new();
    for reference_result in repo.references_glob("refs/remotes/origin/*")? {
        let reference = reference_result?;
        let Some(name) = reference.name() else {
            continue;
        };
        if name == "refs/remotes/origin/HEAD" {
            continue;
        }
        let Some(oid) = reference.target() else {
            continue;
        };
        candidates.push((name.to_string(), oid));
    }

    if candidates.is_empty() {
        return Ok(None);
    }

    // Prefer conventional default branches first, then deterministic lexical order.
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some((_, oid)) = candidates
        .iter()
        .find(|(name, _)| name == "refs/remotes/origin/main")
        .or_else(|| {
            candidates
                .iter()
                .find(|(name, _)| name == "refs/remotes/origin/master")
        })
    {
        return Ok(Some(repo.find_object(*oid, None)?));
    }

    Ok(Some(repo.find_object(candidates[0].1, None)?))
}

fn remote_callbacks() -> RemoteCallbacks<'static> {
    let mut callbacks = RemoteCallbacks::new();
    let mut credential_state = CredentialState::default();

    callbacks.credentials(move |url, username_from_url, allowed| {
        credential_state.provide(url, username_from_url, allowed)
    });

    callbacks
}

#[derive(Default)]
struct CredentialState {
    username: Option<String>,
    prompted_userpass: bool,
}

impl CredentialState {
    fn provide(
        &mut self,
        url: &str,
        username_from_url: Option<&str>,
        allowed: CredentialType,
    ) -> std::result::Result<Cred, git2::Error> {
        if allowed.contains(CredentialType::USERNAME)
            && !allowed.intersects(
                CredentialType::SSH_KEY
                    | CredentialType::USER_PASS_PLAINTEXT
                    | CredentialType::DEFAULT,
            )
        {
            let username = self.username(url, username_from_url)?;
            return Cred::username(&username);
        }

        if allowed.contains(CredentialType::SSH_KEY) {
            let ssh_username = username_from_url
                .or(self.username.as_deref())
                .unwrap_or("git");
            if let Ok(cred) = Cred::ssh_key_from_agent(ssh_username) {
                return Ok(cred);
            }
        }

        if allowed.contains(CredentialType::DEFAULT)
            && let Ok(cred) = Cred::default()
        {
            return Ok(cred);
        }

        if allowed.contains(CredentialType::USER_PASS_PLAINTEXT) {
            let (username, password) = self.userpass(url, username_from_url)?;
            return Cred::userpass_plaintext(&username, &password);
        }

        if allowed.contains(CredentialType::USERNAME) {
            let username = self.username(url, username_from_url)?;
            return Cred::username(&username);
        }

        Err(git2::Error::from_str(
            "Unsupported authentication method requested by git remote",
        ))
    }

    fn username(
        &mut self,
        url: &str,
        username_from_url: Option<&str>,
    ) -> std::result::Result<String, git2::Error> {
        if let Some(username) = username_from_url
            && !username.trim().is_empty()
        {
            let username = username.trim().to_string();
            self.username.get_or_insert_with(|| username.clone());
            return Ok(username);
        }
        if let Some(username) = self.username.as_ref() {
            return Ok(username.clone());
        }

        ensure_prompt_terminal(url)?;
        crate::log_warn!("Git remote requires credentials: {}", url);

        let mut input = String::new();
        loop {
            print!("Git username for {}: ", url);
            io::stdout()
                .flush()
                .map_err(|e| git2::Error::from_str(&format!("Failed to flush prompt: {e}")))?;
            input.clear();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| git2::Error::from_str(&format!("Failed to read username: {e}")))?;
            let trimmed = input.trim();
            if !trimmed.is_empty() {
                let username = trimmed.to_string();
                self.username = Some(username.clone());
                return Ok(username);
            }
            crate::log_warn!("Username cannot be empty.");
        }
    }

    fn userpass(
        &mut self,
        url: &str,
        username_from_url: Option<&str>,
    ) -> std::result::Result<(String, String), git2::Error> {
        if self.prompted_userpass {
            return Err(git2::Error::from_str(
                "Git credentials were rejected by the remote",
            ));
        }

        let username = self.username(url, username_from_url)?;
        ensure_prompt_terminal(url)?;
        self.prompted_userpass = true;

        let prompt = format!("Git password/token for {} ({}):", url, username);
        let password = Password::new(&prompt)
            .without_confirmation()
            .prompt()
            .map_err(|e| {
                git2::Error::from_str(&format!("Failed to read git password/token: {e}"))
            })?;
        if password.is_empty() {
            return Err(git2::Error::from_str("Git password/token cannot be empty"));
        }

        Ok((username, password))
    }
}

fn ensure_prompt_terminal(url: &str) -> std::result::Result<(), git2::Error> {
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        return Ok(());
    }

    Err(git2::Error::from_str(&format!(
        "Authentication required for {url}, but no interactive terminal is available"
    )))
}

#[cfg(test)]
mod tests {
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
        assert_eq!(repo.head_detached().unwrap(), true);
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
}
