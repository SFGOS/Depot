//! Git source support via libgit2 (git2 crate)

use anyhow::{Context, Result};
use git2::{
    CheckoutNotificationType, Cred, CredentialType, FetchOptions, Oid, RemoteCallbacks, Repository,
};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
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
    crate::interrupts::install().context("Failed to enable Ctrl-C handling for git operations")?;
    fs::create_dir_all(git_cache_dir).with_context(|| {
        format!(
            "Failed to create git cache dir: {}",
            git_cache_dir.display()
        )
    })?;

    let mirror_dir = git_cache_dir.join(mirror_key(url));
    ensure_mirror(url, &mirror_dir, pkgname, rev, cherry_pick_revs)?;

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
    let checkout_progress = CheckoutProgress::new(format!("git {}", pkgname));
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout_progress.attach(&mut checkout);

    let mut builder = git2::build::RepoBuilder::new();
    builder.with_checkout(checkout);
    match builder.clone(mirror_url, checkout_dir) {
        Ok(_) => {}
        Err(_err) if crate::interrupts::was_interrupted() => {
            anyhow::bail!("Interrupted by Ctrl-C while cloning {}", url)
        }
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to clone from mirror for {}", url));
        }
    }
    checkout_progress.finish("checkout complete");

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

fn ensure_mirror(
    url: &str,
    mirror_dir: &Path,
    pkgname: &str,
    rev: &str,
    cherry_pick_revs: &[String],
) -> Result<()> {
    let fresh = !mirror_dir.exists();
    let repo = if fresh {
        crate::log_info!("Initializing git mirror for {} ({})...", pkgname, url);
        Repository::init_bare(mirror_dir)
            .with_context(|| format!("Failed to initialize git mirror: {}", mirror_dir.display()))?
    } else {
        Repository::open_bare(mirror_dir)
            .with_context(|| format!("Failed to open git mirror: {}", mirror_dir.display()))?
    };

    if should_skip_fetch_for_cached_revs(&repo, rev, cherry_pick_revs) {
        crate::log_info!("Using cached git revision '{}' for {}.", rev, pkgname);
        return Ok(());
    }

    let mut remote = ensure_origin_remote(&repo, url)?;
    let fetch_attempts = fetch_attempts_for_rev(rev);
    let mut attempted = false;

    for attempt in &fetch_attempts {
        let Some(message) = fetch_attempt_message(attempt, rev, fresh) else {
            continue;
        };
        attempted = true;
        let refspecs = attempt.refspecs();
        fetch_remote_refspecs(&mut remote, url, pkgname, &refspecs)?;
        repair_mirror_refs(&repo)?;
        if has_required_revs(&repo, rev, cherry_pick_revs) {
            crate::log_info!("{}", message);
            return Ok(());
        }
    }

    if !attempted {
        anyhow::bail!("No fetch strategy available for git revision '{}'", rev);
    }

    anyhow::bail!("Failed to fetch git revision '{}'", rev)
}

#[derive(Clone)]
enum FetchAttempt {
    Default,
    HeadRefs,
    Tag(String),
    Branch(String),
}

impl FetchAttempt {
    fn refspecs(&self) -> Vec<&str> {
        match self {
            FetchAttempt::Default => Vec::new(),
            FetchAttempt::HeadRefs => vec!["+refs/heads/*:refs/heads/*"],
            FetchAttempt::Tag(tag) => vec![tag.as_str()],
            FetchAttempt::Branch(branch) => vec![branch.as_str()],
        }
    }
}

fn ensure_origin_remote<'a>(repo: &'a Repository, url: &str) -> Result<git2::Remote<'a>> {
    match repo.find_remote("origin") {
        Ok(remote) => Ok(remote),
        Err(_) => {
            repo.remote("origin", url)
                .with_context(|| format!("Failed to create remote for {}", url))?;
            repo.find_remote("origin")
                .with_context(|| format!("Failed to reopen remote for {}", url))
        }
    }
}

fn fetch_remote_refspecs(
    remote: &mut git2::Remote<'_>,
    url: &str,
    pkgname: &str,
    refspecs: &[&str],
) -> Result<()> {
    let mut fo = FetchOptions::new();
    let transfer_progress = TransferProgress::new(format!("git {}", pkgname));
    fo.remote_callbacks(authenticated_remote_callbacks(
        Some(transfer_progress.bar()),
        url,
    ));

    match remote.fetch(refspecs, Some(&mut fo), None) {
        Ok(_) => {}
        Err(_err) if crate::interrupts::was_interrupted() => {
            anyhow::bail!("Interrupted by Ctrl-C while fetching {}", url)
        }
        Err(err) => {
            return Err(err).with_context(|| format!("Failed to fetch updates for {}", url));
        }
    }
    transfer_progress.finish("git fetch complete");
    Ok(())
}

fn fetch_attempt_message(attempt: &FetchAttempt, rev: &str, fresh: bool) -> Option<String> {
    let state = if fresh {
        "mirror ready"
    } else {
        "mirror updated"
    };
    match attempt {
        FetchAttempt::Default => Some(state.to_string()),
        FetchAttempt::HeadRefs => Some(format!("{state} (heads only)")),
        FetchAttempt::Tag(_) => Some(format!("{state} (tag {})", rev)),
        FetchAttempt::Branch(_) => Some(format!("{state} (branch {})", rev)),
    }
}

fn repair_mirror_refs(repo: &Repository) -> Result<()> {
    sync_remote_tracking_heads(repo)?;
    ensure_valid_local_head(repo)
}

fn sync_remote_tracking_heads(repo: &Repository) -> Result<()> {
    for reference_result in repo.references_glob("refs/remotes/origin/*")? {
        let reference = reference_result?;
        let Some(name) = reference.name() else {
            continue;
        };
        if name == "refs/remotes/origin/HEAD" {
            continue;
        }
        let Some(branch) = name.strip_prefix("refs/remotes/origin/") else {
            continue;
        };
        let Some(target) = reference.target() else {
            continue;
        };
        repo.reference(
            &format!("refs/heads/{branch}"),
            target,
            true,
            "sync mirror branch from origin tracking ref",
        )?;
    }
    Ok(())
}

fn ensure_valid_local_head(repo: &Repository) -> Result<()> {
    if let Ok(head) = repo.head()
        && (head.target().is_some() || head.resolve().is_ok())
    {
        return Ok(());
    }

    let mut candidates: Vec<String> = Vec::new();
    for reference_result in repo.references_glob("refs/heads/*")? {
        let reference = reference_result?;
        let Some(name) = reference.name() else {
            continue;
        };
        candidates.push(name.to_string());
    }

    if candidates.is_empty() {
        return Ok(());
    }

    candidates.sort();
    let preferred = candidates
        .iter()
        .find(|name| name.as_str() == "refs/heads/main")
        .or_else(|| {
            candidates
                .iter()
                .find(|name| name.as_str() == "refs/heads/master")
        })
        .unwrap_or(&candidates[0]);
    repo.set_head(preferred)?;
    Ok(())
}

fn fetch_attempts_for_rev(rev: &str) -> Vec<FetchAttempt> {
    if rev.eq_ignore_ascii_case("HEAD") {
        return vec![FetchAttempt::HeadRefs, FetchAttempt::Default];
    }

    if is_probably_oid(rev) {
        return vec![FetchAttempt::Default];
    }

    vec![
        FetchAttempt::Tag(tag_refspec(rev)),
        FetchAttempt::Branch(branch_refspec(rev)),
        FetchAttempt::Default,
    ]
}

fn should_skip_fetch_for_cached_revs(
    repo: &Repository,
    rev: &str,
    cherry_pick_revs: &[String],
) -> bool {
    if !has_required_revs(repo, rev, cherry_pick_revs) {
        return false;
    }

    if rev.eq_ignore_ascii_case("HEAD") {
        return false;
    }

    if repo.find_reference(&format!("refs/tags/{rev}")).is_ok() {
        return true;
    }

    if is_probably_oid(rev) {
        return resolve_rev_object(repo, rev).is_ok();
    }

    false
}

fn has_required_revs(repo: &Repository, rev: &str, cherry_pick_revs: &[String]) -> bool {
    if resolve_rev_object(repo, rev).is_err() {
        return false;
    }

    cherry_pick_revs
        .iter()
        .all(|cherry_pick_rev| resolve_rev_object(repo, cherry_pick_rev.trim()).is_ok())
}

fn is_probably_oid(rev: &str) -> bool {
    let len = rev.len();
    (7..=40).contains(&len) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

fn tag_refspec(rev: &str) -> String {
    format!("refs/tags/{rev}:refs/tags/{rev}")
}

fn branch_refspec(rev: &str) -> String {
    format!("+refs/heads/{rev}:refs/heads/{rev}")
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

pub(crate) fn authenticated_remote_callbacks(
    progress_bar: Option<ProgressBar>,
    _label: &str,
) -> RemoteCallbacks<'static> {
    let mut callbacks = RemoteCallbacks::new();
    let mut credential_state = CredentialState::default();

    callbacks.credentials(move |url, username_from_url, allowed| {
        credential_state.provide(url, username_from_url, allowed)
    });
    if let Some(progress_bar) = progress_bar {
        let sideband_bar = progress_bar.clone();
        callbacks
            .sideband_progress(move |_message| git_operation_should_continue(Some(&sideband_bar)));

        callbacks.transfer_progress(move |stats| {
            let total_objects = stats.total_objects() as u64;
            if total_objects > 0 {
                progress_bar.set_length(total_objects);
                progress_bar.set_position(stats.received_objects() as u64);
            }

            progress_bar.set_message(format!(
                "{} obj, {} delta, {} bytes",
                stats.received_objects(),
                stats.indexed_deltas(),
                stats.received_bytes()
            ));

            git_operation_should_continue(Some(&progress_bar))
        });
    }

    callbacks
}

struct TransferProgress {
    bar: ProgressBar,
}

impl TransferProgress {
    fn new(prefix: String) -> Self {
        let bar = ProgressBar::new(1);
        bar.set_draw_target(progress_draw_target());
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{prefix:.bold} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        bar.set_prefix(prefix);
        bar.set_message("starting transfer");
        Self { bar }
    }

    fn bar(&self) -> ProgressBar {
        self.bar.clone()
    }

    fn finish(&self, message: &str) {
        self.bar.finish_and_clear();
        crate::log_info!("{}", message);
    }
}

struct CheckoutProgress {
    bar: ProgressBar,
}

impl CheckoutProgress {
    fn new(prefix: String) -> Self {
        let bar = ProgressBar::new(1);
        bar.set_draw_target(progress_draw_target());
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{prefix:.bold} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("#>-"),
        );
        bar.set_prefix(prefix);
        bar.set_message("preparing checkout");
        Self { bar }
    }

    fn attach(&self, checkout: &mut git2::build::CheckoutBuilder<'static>) {
        checkout.notify_on(
            CheckoutNotificationType::CONFLICT
                | CheckoutNotificationType::DIRTY
                | CheckoutNotificationType::UPDATED
                | CheckoutNotificationType::UNTRACKED
                | CheckoutNotificationType::IGNORED,
        );

        let notify_bar = self.bar.clone();
        checkout.notify(move |_, path, _, _, _| {
            if let Some(path) = path {
                notify_bar.set_message(path.display().to_string());
            }
            git_operation_should_continue(Some(&notify_bar))
        });

        let bar = self.bar.clone();
        checkout.progress(move |path, current, total| {
            let total = total as u64;
            if total > 0 {
                bar.set_length(total);
                bar.set_position(current as u64);
            }
            if let Some(path) = path {
                bar.set_message(path.display().to_string());
            }
        });
    }

    fn finish(&self, message: &str) {
        self.bar.finish_and_clear();
        crate::log_info!("{}", message);
    }
}

fn progress_draw_target() -> ProgressDrawTarget {
    if io::stderr().is_terminal() {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::hidden()
    }
}

fn git_operation_should_continue(progress_bar: Option<&ProgressBar>) -> bool {
    if !crate::interrupts::was_interrupted() {
        return true;
    }

    if let Some(progress_bar) = progress_bar {
        progress_bar.finish_and_clear();
    }
    false
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
            [FetchAttempt::HeadRefs, FetchAttempt::Default]
        ));
    }

    #[test]
    fn fetch_attempts_for_named_revision_try_tag_then_branch_then_fallback() {
        let attempts = fetch_attempts_for_rev("v1.2.3");
        assert_eq!(attempts.len(), 3);
        assert!(
            matches!(&attempts[0], FetchAttempt::Tag(tag) if tag == "refs/tags/v1.2.3:refs/tags/v1.2.3")
        );
        assert!(
            matches!(&attempts[1], FetchAttempt::Branch(branch) if branch == "+refs/heads/v1.2.3:refs/heads/v1.2.3")
        );
        assert!(matches!(attempts[2], FetchAttempt::Default));
    }

    #[test]
    fn fetch_attempts_for_oid_use_full_fetch_only() {
        let attempts = fetch_attempts_for_rev("0123456789abcdef");
        assert!(matches!(attempts.as_slice(), [FetchAttempt::Default]));
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
            Some("refs/heads/main")
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
}
