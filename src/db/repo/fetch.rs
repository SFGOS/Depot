use super::*;

pub(super) fn binary_repo_cache_dir(package_cache_dir: &Path, repo_name: &str) -> PathBuf {
    package_cache_dir.join("repos").join(repo_name)
}

pub(super) fn binary_repo_packages_cache_dir(package_cache_dir: &Path, repo_name: &str) -> PathBuf {
    binary_repo_cache_dir(package_cache_dir, repo_name).join("packages")
}

pub(super) fn join_repo_url(base: &str, rel: &str) -> Result<String> {
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
pub(super) enum FileUrlCopyOutcome {
    NotFileUrl,
    Copied,
    Missing,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub(super) struct RepoDbFetchCacheKey {
    pub(super) repo_name: String,
    pub(super) base_url: String,
    pub(super) repo_db_rel: String,
    pub(super) rootfs: PathBuf,
    pub(super) package_cache_dir: PathBuf,
}

pub(super) fn repo_db_fetch_cache() -> &'static Mutex<HashMap<RepoDbFetchCacheKey, PathBuf>> {
    static CACHE: OnceLock<Mutex<HashMap<RepoDbFetchCacheKey, PathBuf>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn get_cached_repo_db_path(cache_key: &RepoDbFetchCacheKey) -> Option<PathBuf> {
    let mut cache = repo_db_fetch_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cached = cache.get(cache_key).cloned()?;
    if cached.exists() {
        return Some(cached);
    }
    cache.remove(cache_key);
    None
}

pub(super) fn remember_repo_db_path(cache_key: RepoDbFetchCacheKey, db_path: PathBuf) {
    let mut cache = repo_db_fetch_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.insert(cache_key, db_path);
}

pub(super) fn copy_file_url_to_path(url: &str, dest: &Path) -> Result<FileUrlCopyOutcome> {
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

pub(super) fn fetch_url_to_path(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
) -> Result<bool> {
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

pub(super) fn extract_html_href_targets(html: &str) -> Vec<String> {
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

pub(super) fn default_repo_public_key_candidate_names(base_url: &str) -> Result<Vec<String>> {
    let mut names = vec![
        "vertex.pub".to_string(),
        "depot.pub".to_string(),
        "depot.minisign.pub".to_string(),
        "minisign.pub".to_string(),
        "repo.pub".to_string(),
    ];

    if let Ok(parsed) = url::Url::parse(base_url)
        && let Some(last_segment) = parsed
            .path_segments()
            .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
    {
        names.push(format!("{}.pub", last_segment));
    }

    names.sort();
    names.dedup();
    Ok(names)
}

pub(super) fn probe_repo_public_key_urls(
    base_url: &str,
    client: &reqwest::blocking::Client,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for key_name in default_repo_public_key_candidate_names(base_url)? {
        let key_url = join_repo_url(base_url, &format!("keys/{}", key_name))?;
        let resp = client
            .get(&key_url)
            .send()
            .with_context(|| format!("Failed to fetch {}", key_url))?;
        if resp.status().is_success() {
            out.push((key_name, key_url));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

pub(super) fn list_repo_public_key_urls(
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

    let mut out = Vec::new();
    let keys_url = join_repo_url(base_url, "keys/")?;
    let resp = client
        .get(&keys_url)
        .send()
        .with_context(|| format!("Failed to fetch {}", keys_url))?;
    if resp.status().is_success() {
        let body = resp
            .text()
            .with_context(|| format!("Failed to read {}", keys_url))?;
        let keys_base = url::Url::parse(&keys_url)
            .with_context(|| format!("Invalid repo keys URL: {}", keys_url))?;

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
    }

    if out.is_empty() {
        out = probe_repo_public_key_urls(base_url, client)?;
    }

    out.sort();
    out.dedup();
    Ok(out)
}

pub(super) fn verify_with_any_trusted_public_key(
    rootfs: &Path,
    input: &Path,
    sig_path: &Path,
) -> Result<PathBuf> {
    let keys = crate::signing::load_trusted_public_keys(rootfs)?;
    if keys.is_empty() {
        anyhow::bail!("No trusted minisign public keys found in rootfs or host");
    }
    crate::signing::verify_zst_file_detached_with_trusted_keys(input, sig_path, &keys)
}

pub(super) fn sanitize_filename_component(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '_',
        })
        .collect()
}

pub(super) fn install_trusted_repo_public_key(
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

pub(super) fn try_trust_repo_public_key_for_repo_db(
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
        if !crate::ui::prompt_yes_no(&prompt, true)? {
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

pub(super) fn normalize_git_mirror_url(url: &str) -> Result<String> {
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

pub(super) fn decompress_zstd_file(src: &Path, dst: &Path) -> Result<()> {
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
    let cache_key = RepoDbFetchCacheKey {
        repo_name: repo_name.to_string(),
        base_url: base_url.to_string(),
        repo_db_rel: repo_db_rel.to_string(),
        rootfs: rootfs.to_path_buf(),
        package_cache_dir: package_cache_dir.to_path_buf(),
    };
    if let Some(cached_db_path) = get_cached_repo_db_path(&cache_key) {
        return Ok(cached_db_path);
    }

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
    crate::log_info!("Fetching binary repo DB for '{}'", repo_name);

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
                remember_repo_db_path(cache_key.clone(), repo_db_sqlite.clone());
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
                    remember_repo_db_path(cache_key.clone(), repo_db_sqlite.clone());
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
        let mut trusted_keys = crate::signing::list_trusted_public_keys(rootfs)?;
        if trusted_keys.is_empty() {
            if try_trust_repo_public_key_for_repo_db(
                repo_name, base_url, rootfs, &cache_dir, &client, &tmp_zst, &tmp_sig,
            )?
            .is_some()
            {
                crate::log_info!("Trusted repo key for '{}' installed", repo_name);
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
            trusted_keys = crate::signing::list_trusted_public_keys(rootfs)?;
        }

        if trusted_keys.is_empty() {
            // No key was trusted/installed, and allow_unsigned=true already handled above.
        } else {
            if let Err(initial_err) = verify_with_any_trusted_public_key(rootfs, &tmp_zst, &tmp_sig)
            {
                if try_trust_repo_public_key_for_repo_db(
                    repo_name, base_url, rootfs, &cache_dir, &client, &tmp_zst, &tmp_sig,
                )?
                .is_some()
                {
                    crate::log_info!("Trusted repo key for '{}' installed", repo_name);
                    verify_with_any_trusted_public_key(rootfs, &tmp_zst, &tmp_sig).with_context(
                        || {
                            format!(
                                "Failed to verify detached signature for binary repo '{}'",
                                repo_name
                            )
                        },
                    )?;
                } else {
                    return Err(initial_err).with_context(|| {
                        format!(
                            "Failed to verify detached signature for binary repo '{}'",
                            repo_name
                        )
                    });
                }
            }
            crate::log_info!(
                "Verified detached signature for binary repo '{}'",
                repo_name
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
    remember_repo_db_path(cache_key, repo_db_sqlite.clone());
    Ok(repo_db_sqlite)
}
