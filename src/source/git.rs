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
    // Try a few reasonable ways to resolve the rev.
    let obj = repo
        .revparse_single(rev)
        .or_else(|_| repo.revparse_single(&format!("refs/tags/{rev}")))
        .or_else(|_| repo.revparse_single(&format!("refs/heads/{rev}")))
        .with_context(|| format!("Could not resolve git rev: {}", rev))?;

    // Peel tags to commit if needed.
    let commit = obj
        .peel_to_commit()
        .with_context(|| format!("Could not peel rev to commit: {}", rev))?;

    let oid: Oid = commit.id();
    repo.set_head_detached(oid)?;
    repo.checkout_tree(commit.as_object(), None)?;
    Ok(())
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
