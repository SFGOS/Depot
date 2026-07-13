use super::*;

/// Synchronize git mirrors into /usr/src/depot/<reponame>
pub fn sync_mirrors(
    repo_dir: &std::path::Path,
    mirrors: &std::collections::HashMap<String, String>,
) -> Result<()> {
    use git2::{FetchOptions, Repository, ResetType, build::RepoBuilder};
    use std::os::unix::fs::PermissionsExt;

    let base = repo_dir.to_path_buf();
    if !base.exists() {
        std::fs::create_dir_all(&base)?;
    }

    for (name, url) in mirrors {
        let target = base.join(name);
        let git_url = normalize_git_mirror_url(url)?;
        if !target.exists() {
            crate::log_info!("Cloning mirror '{}' -> {}", name, target.display());

            let mut fo = FetchOptions::new();
            fo.remote_callbacks(crate::source::authenticated_remote_callbacks(
                None, &git_url,
            ));

            let mut builder = RepoBuilder::new();
            builder.fetch_options(fo);
            builder
                .clone(&git_url, &target)
                .with_context(|| format!("Failed to clone {}", url))?;
        } else {
            crate::log_info!("Updating mirror '{}' in {}", name, target.display());
            // Open repository and fetch updates
            let repo = Repository::open(&target)
                .with_context(|| format!("Failed to open repository at {}", target.display()))?;

            let mut fo = FetchOptions::new();
            fo.remote_callbacks(crate::source::authenticated_remote_callbacks(
                None, &git_url,
            ));

            // Fetch from origin
            let mut remote = repo
                .find_remote("origin")
                .or_else(|_| repo.remote_anonymous(&git_url))?;
            remote
                .fetch(&["refs/heads/*:refs/remotes/origin/*"], Some(&mut fo), None)
                .with_context(|| format!("Failed to fetch updates for {}", url))?;

            // Try to fast-forward HEAD to origin/HEAD by resetting to FETCH_HEAD if present
            if let Ok(fetch_head) = repo.find_reference("FETCH_HEAD")
                && let Some(oid) = fetch_head.target()
            {
                let obj = repo.find_object(oid, None)?;
                repo.reset(&obj, ResetType::Hard, None)?;
            }
        }

        // Make the tree readable and writable by everyone
        for entry in walkdir::WalkDir::new(&target) {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))?;
            } else if path.is_file() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
            }
        }
    }

    Ok(())
}

/// Show status for each mirror repository: path, exists, branch/HEAD, latest commit, dirty
pub fn mirrors_status(
    repo_dir: &std::path::Path,
    mirrors: &std::collections::HashMap<String, String>,
) -> Result<()> {
    use git2::Repository;

    let base = repo_dir.to_path_buf();
    if !base.exists() {
        crate::log_info!("Repo base directory does not exist: {}", base.display());
        return Ok(());
    }

    for name in mirrors.keys() {
        let target = base.join(name);
        crate::log_info!("--- {} ---", name);
        if !target.exists() {
            crate::log_info!("Not cloned: {}", target.display());
            continue;
        }

        match Repository::open(&target) {
            Ok(repo) => {
                // Branch / HEAD
                let head = repo.head().ok();
                let branch = head
                    .as_ref()
                    .and_then(|h| h.shorthand().ok().map(|s| s.to_string()))
                    .unwrap_or_else(|| "(no branch)".to_string());

                // Latest commit OID
                let oid = repo.refname_to_id("HEAD").ok();
                let short = oid
                    .map(|o| format!("{}", o))
                    .unwrap_or_else(|| "(unknown)".to_string());

                // Commit time (seconds since epoch) if available
                let mut commit_time = String::new();
                if let Some(oid) = oid
                    && let Ok(commit) = repo.find_commit(oid)
                {
                    let t = commit.time().seconds();
                    commit_time = format!("{}", t);
                }

                // Dirty status
                let statuses = match repo.statuses(None) {
                    Ok(s) => s,
                    Err(_) => {
                        crate::log_warn!("Failed to read status for {}", target.display());
                        continue;
                    }
                };
                let dirty = statuses.iter().any(|s| {
                    s.status().intersects(
                        git2::Status::WT_MODIFIED | git2::Status::WT_NEW | git2::Status::WT_DELETED,
                    )
                });

                crate::log_info!("Path: {}", target.display());
                crate::log_info!("Branch/HEAD: {}", branch);
                crate::log_info!("HEAD OID: {}", short);
                if !commit_time.is_empty() {
                    crate::log_info!("Latest commit time (epoch): {}", commit_time);
                }
                crate::log_info!("Dirty: {}", if dirty { "yes" } else { "no" });
            }
            Err(e) => {
                crate::log_info!("Failed to open repo at {}: {}", target.display(), e);
            }
        }
    }

    Ok(())
}
