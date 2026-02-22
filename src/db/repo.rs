//! Repository management and SQLite database generation

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use zstd::stream::write::Encoder;

fn parse_license_text(metadata: &toml::Value) -> Option<String> {
    if let Some(s) = metadata.get("license").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = metadata.get("license").and_then(|v| v.as_array()) {
        let licenses: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
        if !licenses.is_empty() {
            return Some(licenses.join(", "));
        }
    }
    None
}

pub struct RepoManager {
    pub repo_dir: PathBuf,
}

impl RepoManager {
    pub fn new(repo_dir: PathBuf) -> Self {
        Self { repo_dir }
    }

    /// Create a compressed SQLite repository database from a directory of packages
    pub fn create_repo_db(&self) -> Result<PathBuf> {
        let db_path = self.repo_dir.join("repo.db");
        let compressed_db_path = self.repo_dir.join("repo.db.zst");

        // Remove existing DB if it exists
        if db_path.exists() {
            fs::remove_file(&db_path)?;
        }

        let mut conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to create repo database at {}", db_path.display()))?;

        self.init_repo_schema(&mut conn)?;

        // Find all .depot.pkg.tar.zst files in repo_dir
        for entry in fs::read_dir(&self.repo_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.to_string_lossy().ends_with(".depot.pkg.tar.zst") {
                self.index_package(&mut conn, &path)?;
            }
        }

        conn.close().map_err(|(_, e)| e)?;

        // Compress the database
        self.compress_db(&db_path, &compressed_db_path)?;

        // Remove the uncompressed DB
        fs::remove_file(&db_path)?;

        Ok(compressed_db_path)
    }

    fn init_repo_schema(&self, conn: &mut Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE packages (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                version TEXT NOT NULL,
                revision INTEGER NOT NULL,
                description TEXT,
                homepage TEXT,
                license TEXT,
                filename TEXT NOT NULL,
                size INTEGER NOT NULL,
                sha256 TEXT NOT NULL
            );
            CREATE TABLE provides (
                package_id INTEGER,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE INDEX idx_packages_name ON packages(name);
            CREATE INDEX idx_provides_name ON provides(name);",
        )
        .context("Failed to initialize repo schema")?;
        Ok(())
    }

    fn index_package(&self, conn: &mut Connection, pkg_path: &Path) -> Result<()> {
        crate::log_info!("Indexing package {}...", pkg_path.display());

        let filename = pkg_path.file_name().unwrap().to_string_lossy();
        let size = pkg_path.metadata()?.len();
        let sha256 = self.calculate_sha256(pkg_path)?;

        // Read .metadata.toml from archive
        let file = fs::File::open(pkg_path)?;
        let zstd_decoder = zstd::stream::read::Decoder::new(file)?;
        let mut archive = tar::Archive::new(zstd_decoder);

        let mut name = String::new();
        let mut version = String::new();
        let mut revision = 1;
        let mut description = None;
        let mut homepage = None;
        let mut license = None;
        let mut provides = Vec::new();

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            if path.to_string_lossy() == ".metadata.toml" {
                let mut content = String::new();
                use std::io::Read;
                entry.read_to_string(&mut content)?;
                let metadata: toml::Value = toml::from_str(&content).with_context(|| {
                    format!("Failed to parse .metadata.toml in {}", pkg_path.display())
                })?;

                name = metadata
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                version = metadata
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                revision = metadata
                    .get("revision")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(1) as u32;
                description = metadata
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                homepage = metadata
                    .get("homepage")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                license = parse_license_text(&metadata);

                if let Some(provides_arr) = metadata.get("provides").and_then(|v| v.as_array()) {
                    provides = provides_arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(String::from)
                        .collect();
                }
                break;
            }
        }

        if name.is_empty() {
            // Fallback for packages WITHOUT metadata (e.g. legacy or during transition)
            let name_parts: Vec<&str> = filename.split('-').collect();
            if name_parts.len() < 4 {
                anyhow::bail!(
                    "Invalid package filename and no .metadata.toml: {}",
                    filename
                );
            }
            name = name_parts[0].to_string();
            version = name_parts[1].to_string();
            revision = name_parts[2].parse().unwrap_or(1);
        }

        // Insert into database
        conn.execute(
            "INSERT INTO packages (name, version, revision, description, homepage, license, filename, size, sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                name,
                version,
                revision as i64,
                description,
                homepage,
                license,
                filename,
                size as i64,
                sha256
            ],
        )?;

        let package_id = conn.last_insert_rowid();

        // Insert into provides
        for provide in provides {
            conn.execute(
                "INSERT INTO provides (package_id, name) VALUES (?1, ?2)",
                params![package_id, provide],
            )?;
        }

        Ok(())
    }

    fn calculate_sha256(&self, path: &Path) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut file = fs::File::open(path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(format!("{:x}", hasher.finalize()))
    }

    fn compress_db(&self, source: &Path, dest: &Path) -> Result<()> {
        let mut input = fs::File::open(source)?;
        let output = fs::File::create(dest)?;
        let mut encoder = Encoder::new(output, 19)?; // High compression for repo DB
        encoder.multithread(num_cpus() as u32)?;
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
        Ok(())
    }
}

/// Synchronize git mirrors into /usr/src/depot/<reponame>
pub fn sync_mirrors(
    repo_dir: &std::path::Path,
    mirrors: &std::collections::HashMap<String, String>,
) -> Result<()> {
    use git2::{Cred, FetchOptions, RemoteCallbacks, Repository, ResetType, build::RepoBuilder};
    use std::os::unix::fs::PermissionsExt;

    let base = repo_dir.to_path_buf();
    if !base.exists() {
        std::fs::create_dir_all(&base)?;
    }

    for (name, url) in mirrors {
        let target = base.join(name);
        if !target.exists() {
            crate::log_info!("Cloning mirror '{}' -> {}", name, target.display());

            // Use git2 RepoBuilder to clone
            let mut cb = RemoteCallbacks::new();
            cb.credentials(|_url, username_from_url, _allowed| {
                // Try default credentials (ssh-agent / keychain)
                Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"))
            });

            let mut fo = FetchOptions::new();
            fo.remote_callbacks(cb);

            let mut builder = RepoBuilder::new();
            builder.fetch_options(fo);
            builder
                .clone(url, &target)
                .with_context(|| format!("Failed to clone {}", url))?;
        } else {
            crate::log_info!("Updating mirror '{}' in {}", name, target.display());
            // Open repository and fetch updates
            let repo = Repository::open(&target)
                .with_context(|| format!("Failed to open repository at {}", target.display()))?;

            let mut cb = RemoteCallbacks::new();
            cb.credentials(|_url, username_from_url, _allowed| {
                Cred::ssh_key_from_agent(username_from_url.unwrap_or("git"))
            });

            let mut fo = FetchOptions::new();
            fo.remote_callbacks(cb);

            // Fetch from origin
            let mut remote = repo
                .find_remote("origin")
                .or_else(|_| repo.remote_anonymous(url))?;
            remote
                .fetch(&["refs/heads/*:refs/remotes/origin/*"], Some(&mut fo), None)
                .with_context(|| format!("Failed to fetch updates for {}", url))?;

            // Try to fast-forward HEAD to origin/HEAD by resetting to FETCH_HEAD if present
            if let Ok(fetch_head) = repo.find_reference("FETCH_HEAD") {
                if let Some(oid) = fetch_head.target() {
                    let obj = repo.find_object(oid, None)?;
                    repo.reset(&obj, ResetType::Hard, None)?;
                }
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

    for (name, _url) in mirrors {
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
                    .and_then(|h| h.shorthand().map(|s| s.to_string()))
                    .unwrap_or_else(|| "(no branch)".to_string());

                // Latest commit OID
                let oid = repo.refname_to_id("HEAD").ok();
                let short = oid
                    .map(|o| format!("{}", o))
                    .unwrap_or_else(|| "(unknown)".to_string());

                // Commit time (seconds since epoch) if available
                let mut commit_time = String::new();
                if let Some(oid) = oid {
                    if let Ok(commit) = repo.find_commit(oid) {
                        let t = commit.time().seconds();
                        commit_time = format!("{}", t);
                    }
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

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
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
version = "1.0"
revision = 1
description = "test description"
homepage = "https://example.com"
license = "MIT"
provides = ["test-feature"]

[dependencies]
build = []
runtime = []
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
        manager.index_package(&mut conn, &pkg_path).unwrap();

        let (name, version, revision, desc, home, lic): (
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT name, version, revision, description, homepage, license FROM packages",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(name, "test");
        assert_eq!(version, "1.0");
        assert_eq!(revision, 1);
        assert_eq!(desc, Some("test description".to_string()));
        assert_eq!(home, Some("https://example.com".to_string()));
        assert_eq!(lic, Some("MIT".to_string()));

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
        manager.index_package(&mut conn, &pkg_path).unwrap();

        let lic: Option<String> = conn
            .query_row("SELECT license FROM packages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lic, Some("MIT, Apache-2.0".to_string()));
    }
}
