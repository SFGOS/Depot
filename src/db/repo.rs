//! Repository management and SQLite database generation

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::fs;
use std::io::{Read, Write};
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

/// Search hit returned from a cached binary repository database.
#[derive(Debug, Clone)]
pub struct BinaryRepoSearchHit {
    pub repo_name: String,
    pub name: String,
    pub version: String,
    pub revision: u32,
    pub description: Option<String>,
    pub filename: String,
    pub size: u64,
    pub provides: Vec<String>,
}

/// Exact package record from a binary repository database, including checksums
/// used to verify the downloaded package archive.
#[derive(Debug, Clone)]
pub struct BinaryRepoPackageRecord {
    pub repo_name: String,
    pub name: String,
    pub version: String,
    pub revision: u32,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub sha512: String,
    pub description: Option<String>,
    pub provides: Vec<String>,
    pub runtime_dependencies: Vec<String>,
}

/// File search hit returned from a cached binary repo database.
#[derive(Debug, Clone)]
pub struct BinaryRepoFileSearchHit {
    pub repo_name: String,
    pub package_name: String,
    pub version: String,
    pub revision: u32,
    pub path: String,
    pub size: u64,
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
                sha256 TEXT NOT NULL,
                sha512 TEXT NOT NULL
            );
            CREATE TABLE provides (
                package_id INTEGER,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE TABLE dependencies (
                package_id INTEGER,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE TABLE files (
                package_id INTEGER,
                path TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE INDEX idx_packages_name ON packages(name);
            CREATE INDEX idx_provides_name ON provides(name);
            CREATE INDEX idx_dependencies_name ON dependencies(name);
            CREATE INDEX idx_dependencies_kind ON dependencies(kind);
            CREATE INDEX idx_repo_files_path ON files(path);",
        )
        .context("Failed to initialize repo schema")?;
        Ok(())
    }

    fn index_package(&self, conn: &mut Connection, pkg_path: &Path) -> Result<()> {
        crate::log_info!("Indexing package {}...", pkg_path.display());

        let filename = pkg_path.file_name().unwrap().to_string_lossy();
        let size = pkg_path.metadata()?.len();
        let (sha256, sha512) = self.calculate_hashes(pkg_path)?;

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
        let mut runtime_dependencies = Vec::new();
        let mut optional_dependencies = Vec::new();
        let mut archive_files = Vec::new();

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            let path_str = path.to_string_lossy().to_string();
            if path_str == ".metadata.toml" {
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
                if let Some(runtime_arr) = metadata
                    .get("dependencies")
                    .and_then(|v| v.get("runtime"))
                    .and_then(|v| v.as_array())
                {
                    runtime_dependencies = runtime_arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(String::from)
                        .collect();
                }
                if let Some(optional_arr) = metadata
                    .get("dependencies")
                    .and_then(|v| v.get("optional"))
                    .and_then(|v| v.as_array())
                {
                    optional_dependencies = optional_arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(String::from)
                        .collect();
                }
                continue;
            }

            if entry.header().entry_type().is_file() {
                let normalized = path_str.trim_start_matches("./").to_string();
                if normalized == ".metadata.toml" {
                    continue;
                }
                archive_files.push(normalized);
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
            "INSERT INTO packages (name, version, revision, description, homepage, license, filename, size, sha256, sha512)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                name,
                version,
                revision as i64,
                description,
                homepage,
                license,
                filename,
                size as i64,
                sha256,
                sha512
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

        for dep in runtime_dependencies {
            conn.execute(
                "INSERT INTO dependencies (package_id, kind, name) VALUES (?1, 'runtime', ?2)",
                params![package_id, dep],
            )?;
        }
        for dep in optional_dependencies {
            conn.execute(
                "INSERT INTO dependencies (package_id, kind, name) VALUES (?1, 'optional', ?2)",
                params![package_id, dep],
            )?;
        }

        for file_path in archive_files {
            conn.execute(
                "INSERT INTO files (package_id, path) VALUES (?1, ?2)",
                params![package_id, file_path],
            )?;
        }

        Ok(())
    }

    fn calculate_hashes(&self, path: &Path) -> Result<(String, String)> {
        use sha2::{Digest, Sha256, Sha512};
        let mut file = fs::File::open(path)?;
        let mut sha256 = Sha256::new();
        let mut sha512 = Sha512::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sha256.update(&buf[..n]);
            sha512.update(&buf[..n]);
        }
        Ok((
            format!("{:x}", sha256.finalize()),
            format!("{:x}", sha512.finalize()),
        ))
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

fn binary_repo_cache_dir(package_cache_dir: &Path, repo_name: &str) -> PathBuf {
    package_cache_dir.join("repos").join(repo_name)
}

fn binary_repo_packages_cache_dir(package_cache_dir: &Path, repo_name: &str) -> PathBuf {
    binary_repo_cache_dir(package_cache_dir, repo_name).join("packages")
}

fn join_repo_url(base: &str, rel: &str) -> Result<String> {
    let base = if base.ends_with('/') {
        base.to_string()
    } else {
        format!("{base}/")
    };
    let url = url::Url::parse(&base).with_context(|| format!("Invalid repo URL: {base}"))?;
    Ok(url
        .join(rel)
        .with_context(|| format!("Invalid repo db path '{}'", rel))?
        .to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileUrlCopyOutcome {
    NotFileUrl,
    Copied,
    Missing,
}

fn copy_file_url_to_path(url: &str, dest: &Path) -> Result<FileUrlCopyOutcome> {
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(FileUrlCopyOutcome::NotFileUrl),
    };
    if parsed.scheme() != "file" {
        return Ok(FileUrlCopyOutcome::NotFileUrl);
    }

    let src = parsed
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("Invalid file:// URL: {}", url))?;
    if !src.exists() {
        return Ok(FileUrlCopyOutcome::Missing);
    }
    if !src.is_file() {
        anyhow::bail!("file:// URL is not a file: {}", src.display());
    }

    fs::copy(&src, dest)
        .with_context(|| format!("Failed to copy {} to {}", src.display(), dest.display()))?;
    Ok(FileUrlCopyOutcome::Copied)
}

fn fetch_url_to_path(client: &reqwest::blocking::Client, url: &str, dest: &Path) -> Result<bool> {
    match copy_file_url_to_path(url, dest)? {
        FileUrlCopyOutcome::Copied => return Ok(true),
        FileUrlCopyOutcome::Missing => return Ok(false),
        FileUrlCopyOutcome::NotFileUrl => {}
    }

    let mut resp = client
        .get(url)
        .send()
        .with_context(|| format!("Failed to fetch {}", url))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !resp.status().is_success() {
        anyhow::bail!("Failed to fetch {}: HTTP {}", url, resp.status());
    }

    let mut out =
        fs::File::create(dest).with_context(|| format!("Failed to create {}", dest.display()))?;
    std::io::copy(&mut resp, &mut out)
        .with_context(|| format!("Failed to save {}", dest.display()))?;
    out.flush()
        .with_context(|| format!("Failed to flush {}", dest.display()))?;
    Ok(true)
}

fn extract_html_href_targets(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    let html_bytes = html.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < lower_bytes.len() {
        let Some(rel) = lower[i..].find("href") else {
            break;
        };
        let mut j = i + rel + 4;
        while j < lower_bytes.len() && lower_bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= lower_bytes.len() || lower_bytes[j] != b'=' {
            i = j;
            continue;
        }
        j += 1;
        while j < lower_bytes.len() && lower_bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= lower_bytes.len() {
            break;
        }

        let (start, end) = if lower_bytes[j] == b'"' || lower_bytes[j] == b'\'' {
            let quote = lower_bytes[j];
            let start = j + 1;
            let mut k = start;
            while k < lower_bytes.len() && lower_bytes[k] != quote {
                k += 1;
            }
            (start, k)
        } else {
            let start = j;
            let mut k = start;
            while k < lower_bytes.len()
                && !lower_bytes[k].is_ascii_whitespace()
                && lower_bytes[k] != b'>'
            {
                k += 1;
            }
            (start, k)
        };

        if start < end && end <= html_bytes.len() {
            out.push(String::from_utf8_lossy(&html_bytes[start..end]).to_string());
        }
        i = end.saturating_add(1);
    }

    out
}

fn list_repo_public_key_urls(
    base_url: &str,
    client: &reqwest::blocking::Client,
) -> Result<Vec<(String, String)>> {
    let parsed =
        url::Url::parse(base_url).with_context(|| format!("Invalid repo URL: {base_url}"))?;
    if parsed.scheme() == "file" {
        let repo_dir = parsed
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Invalid file:// URL: {}", base_url))?;
        let keys_dir = repo_dir.join("keys");
        if !keys_dir.exists() {
            return Ok(Vec::new());
        }
        if !keys_dir.is_dir() {
            anyhow::bail!(
                "Binary repo keys path is not a directory: {}",
                keys_dir.display()
            );
        }

        let mut out = Vec::new();
        for entry in fs::read_dir(&keys_dir)
            .with_context(|| format!("Failed to read {}", keys_dir.display()))?
        {
            let path = entry?.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.to_ascii_lowercase().ends_with(".pub") {
                continue;
            }
            let key_url = url::Url::from_file_path(&path)
                .map_err(|_| anyhow::anyhow!("Failed to build file:// URL for {}", path.display()))?
                .to_string();
            out.push((name.to_string(), key_url));
        }
        out.sort();
        out.dedup();
        return Ok(out);
    }

    let keys_url = join_repo_url(base_url, "keys/")?;
    let resp = client
        .get(&keys_url)
        .send()
        .with_context(|| format!("Failed to fetch {}", keys_url))?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let body = resp
        .text()
        .with_context(|| format!("Failed to read {}", keys_url))?;
    let keys_base = url::Url::parse(&keys_url)
        .with_context(|| format!("Invalid repo keys URL: {}", keys_url))?;

    let mut out = Vec::new();
    for href in extract_html_href_targets(&body) {
        if href.is_empty() || href.starts_with('#') || href.starts_with('?') {
            continue;
        }
        let Ok(url) = keys_base.join(&href) else {
            continue;
        };
        let Some(name) = url.path_segments().and_then(|mut s| s.next_back()) else {
            continue;
        };
        if name.is_empty() || !name.to_ascii_lowercase().ends_with(".pub") {
            continue;
        }
        out.push((name.to_string(), url.to_string()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn verify_with_any_trusted_public_key(
    rootfs: &Path,
    input: &Path,
    sig_path: &Path,
) -> Result<PathBuf> {
    let keys = crate::signing::list_trusted_public_keys(rootfs)?;
    if keys.is_empty() {
        anyhow::bail!("No trusted minisign public keys found in rootfs or host");
    }

    let mut last_failure: Option<(PathBuf, anyhow::Error)> = None;
    for key_path in keys {
        match crate::signing::verify_zst_file_detached_with_public_key(input, sig_path, &key_path) {
            Ok(()) => return Ok(key_path),
            Err(err) => last_failure = Some((key_path, err)),
        }
    }

    let (key_path, err) = last_failure.expect("non-empty key list must produce a failure");
    Err(err).with_context(|| {
        format!(
            "Detached signature verification failed with all trusted public keys (last tried {})",
            key_path.display()
        )
    })
}

fn sanitize_filename_component(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '_',
        })
        .collect()
}

fn install_trusted_repo_public_key(
    rootfs: &Path,
    repo_name: &str,
    source_key_path: &Path,
    source_name: &str,
) -> Result<PathBuf> {
    let trusted_dir = crate::signing::trusted_public_keys_dir(rootfs);
    fs::create_dir_all(&trusted_dir)
        .with_context(|| format!("Failed to create {}", trusted_dir.display()))?;

    let base_name = source_name
        .split('/')
        .next_back()
        .filter(|name| !name.is_empty())
        .unwrap_or("repo.pub");
    let base_name = sanitize_filename_component(base_name);
    let repo_prefix = sanitize_filename_component(repo_name);

    let source_bytes = fs::read(source_key_path)
        .with_context(|| format!("Failed to read {}", source_key_path.display()))?;
    let mut candidates = Vec::new();
    candidates.push(trusted_dir.join(&base_name));
    if !repo_prefix.is_empty() {
        candidates.push(trusted_dir.join(format!("{}-{}", repo_prefix, base_name)));
    }

    for candidate in &candidates {
        if candidate.exists() {
            let existing = fs::read(candidate)
                .with_context(|| format!("Failed to read {}", candidate.display()))?;
            if existing == source_bytes {
                return Ok(candidate.clone());
            }
        } else {
            fs::write(candidate, &source_bytes)
                .with_context(|| format!("Failed to write {}", candidate.display()))?;
            return Ok(candidate.clone());
        }
    }

    for idx in 1usize.. {
        let candidate = trusted_dir.join(format!("{}-{}.{}", repo_prefix, base_name, idx));
        if candidate.exists() {
            let existing = fs::read(&candidate)
                .with_context(|| format!("Failed to read {}", candidate.display()))?;
            if existing == source_bytes {
                return Ok(candidate);
            }
            continue;
        }
        fs::write(&candidate, &source_bytes)
            .with_context(|| format!("Failed to write {}", candidate.display()))?;
        return Ok(candidate);
    }

    unreachable!("infinite loop returns on first available candidate")
}

fn try_trust_repo_public_key_for_repo_db(
    repo_name: &str,
    base_url: &str,
    rootfs: &Path,
    cache_dir: &Path,
    client: &reqwest::blocking::Client,
    repo_db_zst_path: &Path,
    repo_db_sig_path: &Path,
) -> Result<Option<PathBuf>> {
    let repo_keys = list_repo_public_key_urls(base_url, client)?;
    if repo_keys.is_empty() {
        return Ok(None);
    }

    let repo_keys_cache_dir = cache_dir.join("repo_keys");
    fs::create_dir_all(&repo_keys_cache_dir)
        .with_context(|| format!("Failed to create {}", repo_keys_cache_dir.display()))?;

    for (key_name, key_url) in repo_keys {
        let cache_name = sanitize_filename_component(&key_name);
        let key_tmp_path = repo_keys_cache_dir.join(&cache_name);
        if !fetch_url_to_path(client, &key_url, &key_tmp_path)? {
            continue;
        }

        if crate::signing::verify_zst_file_detached_with_public_key(
            repo_db_zst_path,
            repo_db_sig_path,
            &key_tmp_path,
        )
        .is_err()
        {
            continue;
        }

        let trusted_dir = crate::signing::trusted_public_keys_dir(rootfs);
        let prompt = format!(
            "Trust repo key '{}' from binary repo '{}' and copy it to {}?",
            key_name,
            repo_name,
            trusted_dir.display()
        );
        if !crate::ui::prompt_yes_no(&prompt, false)? {
            crate::log_warn!(
                "Skipped trusting repo key '{}' for binary repo '{}'",
                key_name,
                repo_name
            );
            continue;
        }

        let installed =
            install_trusted_repo_public_key(rootfs, repo_name, &key_tmp_path, &key_name)?;
        return Ok(Some(installed));
    }

    Ok(None)
}

fn normalize_git_mirror_url(url: &str) -> Result<String> {
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(url.to_string()),
    };
    if parsed.scheme() != "file" {
        return Ok(url.to_string());
    }
    let path = parsed
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("Invalid file:// mirror URL: {}", url))?;
    Ok(path.to_string_lossy().into_owned())
}

fn decompress_zstd_file(src: &Path, dst: &Path) -> Result<()> {
    let mut input =
        fs::File::open(src).with_context(|| format!("Failed to open {}", src.display()))?;
    let mut decoder = zstd::stream::read::Decoder::new(&mut input)
        .with_context(|| format!("Failed to open zstd decoder for {}", src.display()))?;
    let tmp = dst.with_extension("tmp");
    let mut output =
        fs::File::create(&tmp).with_context(|| format!("Failed to create {}", tmp.display()))?;
    std::io::copy(&mut decoder, &mut output)
        .with_context(|| format!("Failed to decompress {}", src.display()))?;
    output
        .flush()
        .with_context(|| format!("Failed to flush {}", tmp.display()))?;
    fs::rename(&tmp, dst)
        .with_context(|| format!("Failed to move {} to {}", tmp.display(), dst.display()))?;
    Ok(())
}

/// Fetch (or refresh) a binary repo `repo.db.zst` into the configured package cache.
///
/// Returns the path to the decompressed SQLite database file.
pub fn fetch_binary_repo_db(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
) -> Result<PathBuf> {
    let machine_arch = std::env::consts::ARCH;
    let base_url = repo.effective_url_for_arch(machine_arch).with_context(|| {
        format!(
            "Binary repo '{}' is not configured for machine arch '{}'",
            repo_name, machine_arch
        )
    })?;
    let repo_db_rel = repo
        .effective_repo_db_for_arch(machine_arch)
        .with_context(|| {
            format!(
                "Binary repo '{}' is not configured for machine arch '{}'",
                repo_name, machine_arch
            )
        })?;

    let cache_dir = binary_repo_cache_dir(package_cache_dir, repo_name);
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("Failed to create {}", cache_dir.display()))?;

    let repo_db_zst = cache_dir.join("repo.db.zst");
    let repo_db_sig = cache_dir.join("repo.db.zst.sig");
    let repo_db_sqlite = cache_dir.join("repo.db");
    let tmp_zst = cache_dir.join("repo.db.zst.tmp");
    let tmp_sig = cache_dir.join("repo.db.zst.sig.tmp");

    let repo_db_url = join_repo_url(base_url, repo_db_rel)?;
    let repo_sig_url = join_repo_url(base_url, &format!("{}.sig", repo_db_rel))?;
    crate::log_info!(
        "Fetching binary repo DB for '{}' from {}",
        repo_name,
        repo_db_url
    );

    let client = reqwest::blocking::Client::builder()
        .build()
        .context("Failed to build HTTP client for binary repo fetch")?;
    match copy_file_url_to_path(&repo_db_url, &tmp_zst)? {
        FileUrlCopyOutcome::Copied => {}
        FileUrlCopyOutcome::Missing => {
            if repo_db_sqlite.exists() {
                crate::log_warn!(
                    "Failed to refresh binary repo '{}' (missing local file), using cached DB: {}",
                    repo_name,
                    repo_db_url
                );
                return Ok(repo_db_sqlite);
            }
            anyhow::bail!("Failed to fetch {}: local file not found", repo_db_url);
        }
        FileUrlCopyOutcome::NotFileUrl => {
            let resp = client
                .get(&repo_db_url)
                .send()
                .with_context(|| format!("Failed to fetch {}", repo_db_url))?;

            if !resp.status().is_success() {
                if repo_db_sqlite.exists() {
                    crate::log_warn!(
                        "Failed to refresh binary repo '{}' (HTTP {}), using cached DB",
                        repo_name,
                        resp.status()
                    );
                    return Ok(repo_db_sqlite);
                }
                anyhow::bail!("Failed to fetch {}: HTTP {}", repo_db_url, resp.status());
            }

            let mut resp = resp;
            let mut out = fs::File::create(&tmp_zst)
                .with_context(|| format!("Failed to create {}", tmp_zst.display()))?;
            std::io::copy(&mut resp, &mut out)
                .with_context(|| format!("Failed to save {}", tmp_zst.display()))?;
            out.flush()
                .with_context(|| format!("Failed to flush {}", tmp_zst.display()))?;
        }
    }

    let sig_downloaded = match copy_file_url_to_path(&repo_sig_url, &tmp_sig)? {
        FileUrlCopyOutcome::Copied => true,
        FileUrlCopyOutcome::Missing => {
            if !repo.allow_unsigned {
                anyhow::bail!(
                    "Failed to fetch detached signature for binary repo '{}' (local file not found): {}",
                    repo_name,
                    repo_sig_url
                );
            }
            crate::log_warn!(
                "Binary repo '{}' has no detached signature (missing local file) for {}; allow_unsigned=true so continuing",
                repo_name,
                repo_db_url
            );
            false
        }
        FileUrlCopyOutcome::NotFileUrl => {
            let sig_resp = client
                .get(&repo_sig_url)
                .send()
                .with_context(|| format!("Failed to fetch {}", repo_sig_url))?;
            if sig_resp.status().is_success() {
                let mut sig_resp = sig_resp;
                let mut sig_out = fs::File::create(&tmp_sig)
                    .with_context(|| format!("Failed to create {}", tmp_sig.display()))?;
                std::io::copy(&mut sig_resp, &mut sig_out)
                    .with_context(|| format!("Failed to save {}", tmp_sig.display()))?;
                sig_out
                    .flush()
                    .with_context(|| format!("Failed to flush {}", tmp_sig.display()))?;
                true
            } else {
                if !repo.allow_unsigned {
                    anyhow::bail!(
                        "Failed to fetch detached signature for binary repo '{}' (HTTP {}): {}",
                        repo_name,
                        sig_resp.status(),
                        repo_sig_url
                    );
                }
                crate::log_warn!(
                    "Binary repo '{}' has no detached signature (HTTP {}) for {}; allow_unsigned=true so continuing",
                    repo_name,
                    sig_resp.status(),
                    repo_db_url
                );
                false
            }
        }
    };

    if sig_downloaded {
        let trusted_keys = crate::signing::list_trusted_public_keys(rootfs)?;
        if trusted_keys.is_empty() {
            if let Some(installed_key) = try_trust_repo_public_key_for_repo_db(
                repo_name, base_url, rootfs, &cache_dir, &client, &tmp_zst, &tmp_sig,
            )? {
                crate::log_info!(
                    "Trusted repo key for '{}' installed at {}",
                    repo_name,
                    installed_key.display()
                );
            } else if !repo.allow_unsigned {
                anyhow::bail!(
                    "No trusted minisign public key found for binary repo '{}' and no trusted key was accepted from {}/keys/",
                    repo_name,
                    base_url.trim_end_matches('/')
                );
            } else {
                crate::log_warn!(
                    "No trusted minisign public key found; skipping verification for binary repo '{}' because allow_unsigned=true",
                    repo_name
                );
            }
        }

        if crate::signing::list_trusted_public_keys(rootfs)?.is_empty() {
            // No key was trusted/installed, and allow_unsigned=true already handled above.
        } else {
            let verified_key = verify_with_any_trusted_public_key(rootfs, &tmp_zst, &tmp_sig)
                .with_context(|| {
                    format!(
                        "Failed to verify detached signature for binary repo '{}'",
                        repo_name
                    )
                })?;
            crate::log_info!(
                "Verified detached signature for binary repo '{}' using {}",
                repo_name,
                verified_key.display()
            );
        }
    }

    fs::rename(&tmp_zst, &repo_db_zst).with_context(|| {
        format!(
            "Failed to move {} to {}",
            tmp_zst.display(),
            repo_db_zst.display()
        )
    })?;
    if sig_downloaded {
        fs::rename(&tmp_sig, &repo_db_sig).with_context(|| {
            format!(
                "Failed to move {} to {}",
                tmp_sig.display(),
                repo_db_sig.display()
            )
        })?;
    } else if repo_db_sig.exists() {
        let _ = fs::remove_file(&repo_db_sig);
    }

    decompress_zstd_file(&repo_db_zst, &repo_db_sqlite)?;
    Ok(repo_db_sqlite)
}

/// Search a cached binary repository SQLite DB by package name or provided feature.
pub fn search_cached_binary_repo_db(
    repo_name: &str,
    db_path: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoSearchHit>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let like = format!("%{}%", query.to_ascii_lowercase());
    let mut stmt = conn.prepare(
        "SELECT
            p.name,
            p.version,
            p.revision,
            p.description,
            p.filename,
            p.size,
            GROUP_CONCAT(DISTINCT pr_all.name)
         FROM packages p
         LEFT JOIN provides pr_all ON pr_all.package_id = p.id
         WHERE lower(p.name) LIKE ?1
            OR EXISTS (
                SELECT 1 FROM provides pr
                WHERE pr.package_id = p.id
                  AND lower(pr.name) LIKE ?1
            )
         GROUP BY p.id
         ORDER BY
            CASE
                WHEN lower(p.name) = lower(?2) THEN 0
                WHEN lower(p.name) LIKE lower(?3) THEN 1
                ELSE 2
            END,
            p.name ASC",
    )?;

    let starts = format!("{}%", query.to_ascii_lowercase());
    let rows = stmt.query_map(params![like, query, starts], |row| {
        let provides_csv: Option<String> = row.get(6)?;
        Ok(BinaryRepoSearchHit {
            repo_name: repo_name.to_string(),
            name: row.get(0)?,
            version: row.get(1)?,
            revision: row.get::<_, i64>(2)? as u32,
            description: row.get(3)?,
            filename: row.get(4)?,
            size: row.get::<_, i64>(5)? as u64,
            provides: provides_csv
                .map(|s| {
                    s.split(',')
                        .filter(|v| !v.is_empty())
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        })
    })?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Fetch and search a binary repo by name or provide.
pub fn search_binary_repo(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoSearchHit>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    search_cached_binary_repo_db(repo_name, &db_path, query)
}

/// Search a cached binary repo DB by file path substring.
pub fn search_cached_binary_repo_files(
    repo_name: &str,
    db_path: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let has_files_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_files_table {
        return Ok(Vec::new());
    }

    let like = format!("%{}%", query.to_ascii_lowercase());
    let mut stmt = conn.prepare(
        "SELECT p.name, p.version, p.revision, f.path, p.size
         FROM files f
         JOIN packages p ON p.id = f.package_id
         WHERE lower(f.path) LIKE ?1
         ORDER BY p.name ASC, f.path ASC",
    )?;
    let rows = stmt.query_map(params![like], |row| {
        Ok(BinaryRepoFileSearchHit {
            repo_name: repo_name.to_string(),
            package_name: row.get(0)?,
            version: row.get(1)?,
            revision: row.get::<_, i64>(2)? as u32,
            path: row.get(3)?,
            size: row.get::<_, i64>(4)? as u64,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Fetch and search a binary repo by file path substring.
pub fn search_binary_repo_files(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    search_cached_binary_repo_files(repo_name, &db_path, query)
}

/// Find the package(s) that own a file path in a cached binary repo DB.
pub fn cached_binary_repo_owns_path(
    repo_name: &str,
    db_path: &Path,
    path: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let has_files_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_files_table {
        return Ok(Vec::new());
    }

    let normalized = path.trim_start_matches('/').trim_start_matches("./");
    let mut stmt = conn.prepare(
        "SELECT p.name, p.version, p.revision, f.path, p.size
         FROM files f
         JOIN packages p ON p.id = f.package_id
         WHERE f.path = ?1
         ORDER BY p.name ASC",
    )?;
    let rows = stmt.query_map(params![normalized], |row| {
        Ok(BinaryRepoFileSearchHit {
            repo_name: repo_name.to_string(),
            package_name: row.get(0)?,
            version: row.get(1)?,
            revision: row.get::<_, i64>(2)? as u32,
            path: row.get(3)?,
            size: row.get::<_, i64>(4)? as u64,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Fetch repo metadata and resolve file ownership in a binary repo.
pub fn binary_repo_owns_path(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    path: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    cached_binary_repo_owns_path(repo_name, &db_path, path)
}

fn query_package_provides(conn: &Connection, package_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM provides WHERE package_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn query_package_runtime_deps(conn: &Connection, package_id: i64) -> Result<Vec<String>> {
    let has_dependencies_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dependencies'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_dependencies_table {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT name FROM dependencies WHERE package_id = ?1 AND kind = 'runtime' ORDER BY name",
    )?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn find_cached_binary_repo_packages(
    repo_name: &str,
    db_path: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let mut stmt = conn.prepare(
        "SELECT
            p.id,
            p.name,
            p.version,
            p.revision,
            p.filename,
            p.size,
            p.sha256,
            p.sha512,
            p.description
         FROM packages p
         WHERE lower(p.name) = lower(?1)
            OR EXISTS (
                SELECT 1 FROM provides pr
                WHERE pr.package_id = p.id
                  AND lower(pr.name) = lower(?1)
            )
         ORDER BY
            CASE WHEN lower(p.name) = lower(?1) THEN 0 ELSE 1 END,
            p.name ASC",
    )?;

    let rows = stmt.query_map(params![query], |row| {
        let package_id = row.get::<_, i64>(0)?;
        Ok((
            package_id,
            BinaryRepoPackageRecord {
                repo_name: repo_name.to_string(),
                name: row.get(1)?,
                version: row.get(2)?,
                revision: row.get::<_, i64>(3)? as u32,
                filename: row.get(4)?,
                size: row.get::<_, i64>(5)? as u64,
                sha256: row.get(6)?,
                sha512: row.get(7)?,
                description: row.get(8)?,
                provides: Vec::new(),
                runtime_dependencies: Vec::new(),
            },
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (package_id, mut rec) = row?;
        rec.provides = query_package_provides(&conn, package_id)?;
        rec.runtime_dependencies = query_package_runtime_deps(&conn, package_id)?;
        out.push(rec);
    }
    Ok(out)
}

/// Resolve an exact package name/provide match from a binary repo after verifying
/// and caching its signed `repo.db.zst`.
pub fn find_binary_repo_package(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Option<BinaryRepoPackageRecord>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    let mut matches = find_cached_binary_repo_packages(repo_name, &db_path, query)?;
    if matches.len() > 1 {
        crate::log_warn!(
            "Multiple binary packages matched '{}' in repo '{}'; using the first match",
            query,
            repo_name
        );
    }
    Ok(matches.drain(..).next())
}

/// Resolve exact package name/provide matches from a binary repo.
pub fn find_binary_repo_packages(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    find_cached_binary_repo_packages(repo_name, &db_path, query)
}

fn verify_hex_digest(path: &Path, algorithm: &str, expected_hex: &str) -> Result<bool> {
    let expected = expected_hex.trim().to_ascii_lowercase();
    if expected.is_empty() {
        return Ok(false);
    }

    let mut file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut buf = [0u8; 64 * 1024];

    let actual = match algorithm {
        "sha256" => {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            format!("{:x}", h.finalize())
        }
        "sha512" => {
            use sha2::{Digest, Sha512};
            let mut h = Sha512::new();
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            format!("{:x}", h.finalize())
        }
        _ => anyhow::bail!("Unsupported checksum algorithm: {}", algorithm),
    };

    Ok(actual == expected)
}

fn verify_binary_package_record_checksums(
    path: &Path,
    rec: &BinaryRepoPackageRecord,
) -> Result<()> {
    if !verify_hex_digest(path, "sha256", &rec.sha256)? {
        anyhow::bail!(
            "SHA-256 mismatch for {} from repo '{}'",
            path.display(),
            rec.repo_name
        );
    }
    if !verify_hex_digest(path, "sha512", &rec.sha512)? {
        anyhow::bail!(
            "SHA-512 mismatch for {} from repo '{}'",
            path.display(),
            rec.repo_name
        );
    }
    Ok(())
}

fn fetch_binary_package_signature(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    client: &reqwest::blocking::Client,
    sig_url: &str,
    sig_path: &Path,
) -> Result<bool> {
    let found = fetch_url_to_path(client, sig_url, sig_path)?;
    if !found {
        if !repo.allow_unsigned {
            anyhow::bail!(
                "Failed to fetch detached signature for binary package in repo '{}' at {}",
                repo_name,
                sig_url
            );
        }
        crate::log_warn!(
            "Detached package signature missing for binary repo '{}' at {}; allow_unsigned=true so continuing",
            repo_name,
            sig_url
        );
    }
    Ok(found)
}

fn verify_binary_package_signature(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    pkg_path: &Path,
    sig_path: &Path,
) -> Result<()> {
    if !sig_path.exists() {
        if repo.allow_unsigned {
            return Ok(());
        }
        anyhow::bail!(
            "Detached package signature required but missing for {}",
            pkg_path.display()
        );
    }

    let trusted_keys = crate::signing::list_trusted_public_keys(rootfs)?;
    if trusted_keys.is_empty() {
        if repo.allow_unsigned {
            crate::log_warn!(
                "No trusted minisign public key found; skipping package signature verification for binary repo '{}' because allow_unsigned=true",
                repo_name
            );
            return Ok(());
        }
        anyhow::bail!(
            "No trusted minisign public key found for detached package signature verification in binary repo '{}'",
            repo_name
        );
    }

    let verified_key = verify_with_any_trusted_public_key(rootfs, pkg_path, sig_path)
        .with_context(|| {
            format!(
                "Failed to verify detached package signature for {}",
                pkg_path.display()
            )
        })?;
    crate::log_info!(
        "Verified detached package signature for '{}' using {}",
        pkg_path.display(),
        verified_key.display()
    );
    Ok(())
}

/// Download a binary package archive and verify it against detached signatures
/// and checksums from signed repository metadata.
pub fn fetch_binary_package_archive(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    rec: &BinaryRepoPackageRecord,
    package_cache_dir: &Path,
) -> Result<PathBuf> {
    let machine_arch = std::env::consts::ARCH;
    let base_url = repo.effective_url_for_arch(machine_arch).with_context(|| {
        format!(
            "Binary repo '{}' is not configured for machine arch '{}'",
            repo_name, machine_arch
        )
    })?;

    let cache_dir = binary_repo_packages_cache_dir(package_cache_dir, repo_name);
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("Failed to create {}", cache_dir.display()))?;
    let dest_path = cache_dir.join(&rec.filename);
    let dest_sig_path = cache_dir.join(format!("{}.sig", rec.filename));
    let tmp_path = cache_dir.join(format!("{}.tmp", rec.filename));
    let tmp_sig_path = cache_dir.join(format!("{}.sig.tmp", rec.filename));
    let pkg_url = join_repo_url(base_url, &rec.filename)?;
    let sig_url = join_repo_url(base_url, &format!("{}.sig", rec.filename))?;

    let client = reqwest::blocking::Client::builder()
        .build()
        .context("Failed to build HTTP client for binary package fetch")?;

    if dest_path.exists() {
        verify_binary_package_record_checksums(&dest_path, rec).with_context(|| {
            format!(
                "Cached binary package failed checksum verification: {}",
                dest_path.display()
            )
        })?;
        if !dest_sig_path.exists() {
            let sig_downloaded =
                fetch_binary_package_signature(repo_name, repo, &client, &sig_url, &tmp_sig_path)?;
            if sig_downloaded {
                fs::rename(&tmp_sig_path, &dest_sig_path).with_context(|| {
                    format!(
                        "Failed to move {} to {}",
                        tmp_sig_path.display(),
                        dest_sig_path.display()
                    )
                })?;
            } else {
                let _ = fs::remove_file(&dest_sig_path);
            }
        }
        verify_binary_package_signature(repo_name, repo, rootfs, &dest_path, &dest_sig_path)
            .with_context(|| {
                format!(
                    "Cached binary package failed signature verification: {}",
                    dest_path.display()
                )
            })?;
        crate::log_info!("Using cached binary package: {}", dest_path.display());
        return Ok(dest_path);
    }

    crate::log_info!("Fetching binary package: {}", pkg_url);

    match copy_file_url_to_path(&pkg_url, &tmp_path)? {
        FileUrlCopyOutcome::Copied => {}
        FileUrlCopyOutcome::Missing => {
            anyhow::bail!("Failed to fetch {}: local file not found", pkg_url);
        }
        FileUrlCopyOutcome::NotFileUrl => {
            let mut resp = client
                .get(&pkg_url)
                .send()
                .with_context(|| format!("Failed to fetch {}", pkg_url))?;
            if !resp.status().is_success() {
                anyhow::bail!("Failed to fetch {}: HTTP {}", pkg_url, resp.status());
            }

            let mut out = fs::File::create(&tmp_path)
                .with_context(|| format!("Failed to create {}", tmp_path.display()))?;
            std::io::copy(&mut resp, &mut out)
                .with_context(|| format!("Failed to save {}", tmp_path.display()))?;
            out.flush()
                .with_context(|| format!("Failed to flush {}", tmp_path.display()))?;
        }
    }

    verify_binary_package_record_checksums(&tmp_path, rec).with_context(|| {
        format!(
            "Downloaded binary package failed checksum verification: {}",
            rec.filename
        )
    })?;

    let sig_downloaded =
        fetch_binary_package_signature(repo_name, repo, &client, &sig_url, &tmp_sig_path)?;

    fs::rename(&tmp_path, &dest_path).with_context(|| {
        format!(
            "Failed to move {} to {}",
            tmp_path.display(),
            dest_path.display()
        )
    })?;
    if sig_downloaded {
        fs::rename(&tmp_sig_path, &dest_sig_path).with_context(|| {
            format!(
                "Failed to move {} to {}",
                tmp_sig_path.display(),
                dest_sig_path.display()
            )
        })?;
        if let Err(err) =
            verify_binary_package_signature(repo_name, repo, rootfs, &dest_path, &dest_sig_path)
        {
            let _ = fs::remove_file(&dest_path);
            let _ = fs::remove_file(&dest_sig_path);
            return Err(err).with_context(|| {
                format!(
                    "Downloaded binary package failed signature verification: {}",
                    rec.filename
                )
            });
        }
    } else {
        let _ = fs::remove_file(&dest_sig_path);
    }
    Ok(dest_path)
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
        let git_url = normalize_git_mirror_url(url)?;
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
                .clone(&git_url, &target)
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
                    .and_then(|h| h.shorthand().map(|s| s.to_string()))
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

        let mut conn = Connection::open_in_memory().unwrap();
        let manager = RepoManager::new(repo_dir.to_path_buf());
        manager.init_repo_schema(&mut conn).unwrap();
        manager.index_package(&mut conn, &pkg_path).unwrap();

        let (name, version, revision, desc, home, lic, sha256, sha512): (
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT name, version, revision, description, homepage, license, sha256, sha512 FROM packages",
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
        assert_eq!(sha256.len(), 64);
        assert_eq!(sha512.len(), 128);

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
    fn test_verify_binary_package_record_checksums_accepts_valid_hashes() {
        use sha2::{Digest, Sha256, Sha512};

        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("pkg.depot.pkg.tar.zst");
        fs::write(&pkg, b"payload").unwrap();

        let sha256 = {
            let mut h = Sha256::new();
            h.update(b"payload");
            format!("{:x}", h.finalize())
        };
        let sha512 = {
            let mut h = Sha512::new();
            h.update(b"payload");
            format!("{:x}", h.finalize())
        };

        let rec = BinaryRepoPackageRecord {
            repo_name: "repo".into(),
            name: "pkg".into(),
            version: "1.0".into(),
            revision: 1,
            filename: "pkg.depot.pkg.tar.zst".into(),
            size: 7,
            sha256,
            sha512,
            description: None,
            provides: Vec::new(),
            runtime_dependencies: Vec::new(),
        };

        verify_binary_package_record_checksums(&pkg, &rec).unwrap();
    }

    fn test_record_for_payload(filename: &str, payload: &[u8]) -> BinaryRepoPackageRecord {
        use sha2::{Digest, Sha256, Sha512};

        let sha256 = {
            let mut h = Sha256::new();
            h.update(payload);
            format!("{:x}", h.finalize())
        };
        let sha512 = {
            let mut h = Sha512::new();
            h.update(payload);
            format!("{:x}", h.finalize())
        };

        BinaryRepoPackageRecord {
            repo_name: "repo".into(),
            name: "pkg".into(),
            version: "1.0".into(),
            revision: 1,
            filename: filename.to_string(),
            size: payload.len() as u64,
            sha256,
            sha512,
            description: None,
            provides: Vec::new(),
            runtime_dependencies: Vec::new(),
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
    fn test_normalize_git_mirror_url_converts_file_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo.git");
        fs::create_dir_all(&repo_dir).unwrap();

        let url = format!("file://{}", repo_dir.display());
        let normalized = normalize_git_mirror_url(&url).unwrap();
        assert_eq!(normalized, repo_dir.to_string_lossy());
    }
}
