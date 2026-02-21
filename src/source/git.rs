//! Git source support via libgit2 (git2 crate)

use anyhow::{Context, Result};
use git2::{Cred, FetchOptions, Oid, RemoteCallbacks, Repository};
use sha2::{Digest, Sha256};
use std::fs;
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

    println!("Cloning git source into {}...", checkout_dir.display());
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
        println!("Cloning git mirror for {} ({})...", pkgname, url);
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

    // Try SSH agent / default key locations.
    callbacks.credentials(|_url, username_from_url, _allowed| {
        if let Some(name) = username_from_url {
            Cred::ssh_key_from_agent(name)
        } else {
            Cred::default()
        }
    });

    callbacks
}
