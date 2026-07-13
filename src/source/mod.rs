//! Source fetching and extraction

mod extractor;
mod fetcher;
mod git;
pub mod hooks;

pub(crate) use git::authenticated_remote_callbacks;
pub(crate) use git::{
    checkout as git_checkout, default_checkout_dir_name as git_default_checkout_dir_name,
    prime_cache as git_prime_cache,
};

use crate::package::PackageSpec;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use url::Url;

fn expand_manual_source_value(spec: &PackageSpec, raw: &str) -> String {
    let carch = spec.build.flags.carch.as_str();
    spec.expand_vars(raw)
        .replace("$CARCH", carch)
        .replace("${CARCH}", carch)
}

fn manual_local_entries(manual: &crate::package::ManualSource) -> Vec<String> {
    let mut local_entries = Vec::new();
    if let Some(file) = manual.file.as_ref()
        && !file.trim().is_empty()
    {
        local_entries.push(file.clone());
    }
    local_entries.extend(
        manual
            .files
            .iter()
            .filter(|s| !s.trim().is_empty())
            .cloned(),
    );
    local_entries
}

fn manual_url_entries(manual: &crate::package::ManualSource) -> Vec<String> {
    let mut url_entries = Vec::new();
    if let Some(url) = manual.url.as_ref()
        && !url.trim().is_empty()
    {
        url_entries.push(url.clone());
    }
    url_entries.extend(manual.urls.iter().filter(|s| !s.trim().is_empty()).cloned());
    url_entries
}

/// Validate manual sources early, before dependency installation and build work.
///
/// Local entries are checked for existence and optional checksum correctness.
/// Remote entries are fetched into the manual-source cache so build-time source
/// preparation can reuse the verified result later. Git manual sources prime
/// their mirror cache and validate revision reachability.
pub fn preflight_local_manual_sources(spec: &PackageSpec) -> Result<()> {
    if spec.manual_sources.is_empty() {
        return Ok(());
    }

    for manual in &spec.manual_sources {
        let local_entries = manual_local_entries(manual);
        for raw_file in local_entries {
            let file = expand_manual_source_value(spec, &raw_file);
            let src_path = spec.spec_dir.join(&file);
            if !src_path.exists() {
                bail!(
                    "Manual source not found: {} (expected at {})",
                    file,
                    src_path.display()
                );
            }

            if let Some(expected_hash) = manual.sha256.as_ref().filter(|h| *h != "skip")
                && !verify_file_hash(&src_path, expected_hash)?
            {
                bail!("Checksum mismatch for {}: expected {}", file, expected_hash);
            }
        }
    }

    Ok(())
}

pub fn preflight_manual_sources(spec: &PackageSpec, cache_dir: &Path) -> Result<()> {
    if spec.manual_sources.is_empty() {
        return Ok(());
    }

    preflight_local_manual_sources(spec)?;

    let manual_entry_count: usize = spec
        .manual_sources
        .iter()
        .map(|m| manual_local_entries(m).len() + manual_url_entries(m).len())
        .sum();
    crate::log_info!("Checking {} manual source(s)...", manual_entry_count);

    for manual in &spec.manual_sources {
        let local_entries = manual_local_entries(manual);
        let url_entries = manual_url_entries(manual);

        if !local_entries.is_empty() {
            continue;
        }

        if !url_entries.is_empty() {
            for raw_url in url_entries {
                let expanded_url = expand_manual_source_value(spec, &raw_url);
                if let Some((base, rev)) = split_git_url(&expanded_url) {
                    if let Some(expected_hash) = manual.sha256.as_ref().filter(|h| *h != "skip") {
                        bail!(
                            "Manual git source {} cannot use checksum {}; pin the desired revision in the URL fragment instead",
                            expanded_url,
                            expected_hash
                        );
                    }
                    git_prime_cache(
                        &base,
                        &rev,
                        &cache_dir.join("manual").join("git"),
                        &spec.package.name,
                        &[],
                    )?;
                    continue;
                }

                let parsed = Url::parse(&expanded_url)
                    .with_context(|| format!("Invalid URL: {}", expanded_url))?;

                if parsed.scheme() == "file" {
                    let src_path = parsed
                        .to_file_path()
                        .map_err(|_| anyhow::anyhow!("Invalid file URL: {}", expanded_url))?;
                    if !src_path.exists() {
                        bail!("Manual source file URL not found: {}", src_path.display());
                    }
                    if let Some(expected_hash) = manual.sha256.as_ref().filter(|h| *h != "skip")
                        && !verify_file_hash(&src_path, expected_hash)?
                    {
                        bail!(
                            "Checksum mismatch for {}: expected {}",
                            expanded_url,
                            expected_hash
                        );
                    }
                    continue;
                }

                let source = crate::package::Source {
                    url: expanded_url,
                    sha256: manual.sha256.clone().unwrap_or_else(|| "skip".to_string()),
                    extract_dir: "manual-source".to_string(),
                    patches: Vec::new(),
                    post_extract: Vec::new(),
                    cherry_pick: Vec::new(),
                };
                let _ = fetcher::fetch_archive(spec, &source, &cache_dir.join("manual"))?;
            }
            continue;
        }

        bail!("Manual source must define one of 'file', 'files', 'url', or 'urls'");
    }

    Ok(())
}

/// Copy manual sources to the build directory before fetching remote sources.
///
/// Manual sources support:
/// - local mode: `file = "..."` (path relative to spec directory)
/// - remote mode: `url = "..."` (downloaded first, then copied)
pub fn copy_manual_sources(spec: &PackageSpec, cache_dir: &Path, build_dir: &Path) -> Result<()> {
    if spec.manual_sources.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(build_dir)?;
    let manual_entry_count: usize = spec
        .manual_sources
        .iter()
        .map(|m| {
            usize::from(m.file.as_ref().is_some_and(|s| !s.trim().is_empty()))
                + m.files.iter().filter(|s| !s.trim().is_empty()).count()
                + usize::from(m.url.as_ref().is_some_and(|s| !s.trim().is_empty()))
                + m.urls.iter().filter(|s| !s.trim().is_empty()).count()
        })
        .sum();
    crate::log_info!("Copying {} manual source(s)...", manual_entry_count);

    for manual in &spec.manual_sources {
        let local_entries = manual_local_entries(manual);
        let url_entries = manual_url_entries(manual);

        if !local_entries.is_empty() {
            for raw_file in local_entries {
                let file = expand_manual_source_value(spec, &raw_file);
                let src_path = spec.spec_dir.join(&file);
                if !src_path.exists() {
                    bail!(
                        "Manual source not found: {} (expected at {})",
                        file,
                        src_path.display()
                    );
                }

                if let Some(expected_hash) = manual.sha256.as_ref().filter(|h| *h != "skip")
                    && !verify_file_hash(&src_path, expected_hash)?
                {
                    bail!("Checksum mismatch for {}: expected {}", file, expected_hash);
                }

                let default_dest = file.clone();
                copy_manual_source_file(spec, build_dir, &src_path, &file, manual, &default_dest)?;
            }
            continue;
        }

        if !url_entries.is_empty() {
            for raw_url in url_entries {
                let expanded_url = expand_manual_source_value(spec, &raw_url);
                if let Some((base, rev)) = split_git_url(&expanded_url) {
                    checkout_manual_git_source(
                        spec,
                        manual,
                        build_dir,
                        cache_dir,
                        &expanded_url,
                        &base,
                        &rev,
                    )?;
                    continue;
                }

                let parsed = Url::parse(&expanded_url)
                    .with_context(|| format!("Invalid URL: {}", expanded_url))?;

                let (source_path, source_label, default_dest): (PathBuf, String, String) =
                    if parsed.scheme() == "file" {
                        let src_path = parsed
                            .to_file_path()
                            .map_err(|_| anyhow::anyhow!("Invalid file URL: {}", expanded_url))?;
                        if !src_path.exists() {
                            bail!("Manual source file URL not found: {}", src_path.display());
                        }
                        if let Some(expected_hash) = manual.sha256.as_ref().filter(|h| *h != "skip")
                            && !verify_file_hash(&src_path, expected_hash)?
                        {
                            bail!(
                                "Checksum mismatch for {}: expected {}",
                                expanded_url,
                                expected_hash
                            );
                        }
                        let default_dest = fetcher::derive_filename_from_url(&expanded_url);
                        (src_path, expanded_url, default_dest)
                    } else {
                        let source = crate::package::Source {
                            url: expanded_url.clone(),
                            sha256: manual.sha256.clone().unwrap_or_else(|| "skip".to_string()),
                            extract_dir: "manual-source".to_string(),
                            patches: Vec::new(),
                            post_extract: Vec::new(),
                            cherry_pick: Vec::new(),
                        };
                        let fetched =
                            fetcher::fetch_archive(spec, &source, &cache_dir.join("manual"))?;
                        let default_dest = fetcher::derive_filename_from_url(&expanded_url);
                        (fetched, expanded_url, default_dest)
                    };

                copy_manual_source_file(
                    spec,
                    build_dir,
                    &source_path,
                    &source_label,
                    manual,
                    &default_dest,
                )?;
            }
            continue;
        }

        bail!("Manual source must define one of 'file', 'files', 'url', or 'urls'");
    }

    Ok(())
}

fn copy_manual_source_file(
    spec: &PackageSpec,
    build_dir: &Path,
    source_path: &Path,
    source_label: &str,
    manual: &crate::package::ManualSource,
    default_dest: &str,
) -> Result<()> {
    let dest_name = manual_source_dest_name(spec, manual, default_dest);
    let dest_path = build_dir.join(&dest_name);

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    crate::log_info!("  {} -> {}", source_label, dest_path.display());
    fs::copy(source_path, &dest_path).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            source_path.display(),
            dest_path.display()
        )
    })?;
    Ok(())
}

fn manual_source_dest_name(
    spec: &PackageSpec,
    manual: &crate::package::ManualSource,
    default_dest: &str,
) -> String {
    if let Some(dest) = manual.dest.as_ref() {
        expand_manual_source_value(spec, dest)
    } else {
        default_dest.to_string()
    }
}

fn remove_existing_path(path: &Path) -> Result<()> {
    if !path.exists() && fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("Failed to stat {}", path.display()))?;
    if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
        fs::remove_dir_all(path)
            .with_context(|| format!("Failed to remove directory {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("Failed to remove file {}", path.display()))?;
    }
    Ok(())
}

fn checkout_manual_git_source(
    spec: &PackageSpec,
    manual: &crate::package::ManualSource,
    build_dir: &Path,
    cache_dir: &Path,
    expanded_url: &str,
    base: &str,
    rev: &str,
) -> Result<()> {
    if let Some(expected_hash) = manual.sha256.as_ref().filter(|h| *h != "skip") {
        bail!(
            "Manual git source {} cannot use checksum {}; pin the desired revision in the URL fragment instead",
            expanded_url,
            expected_hash
        );
    }

    let default_dest = git_default_checkout_dir_name(base);
    let dest_name = manual_source_dest_name(spec, manual, &default_dest);
    let dest_path = build_dir.join(&dest_name);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    remove_existing_path(&dest_path)?;
    crate::log_info!("  {} -> {}", expanded_url, dest_path.display());
    git_checkout(
        base,
        rev,
        &dest_path,
        &cache_dir.join("manual").join("git"),
        &spec.package.name,
        &[],
    )
}

/// Verify a file against an `expected` checksum string.
///
/// Formats accepted: `sha256:HEX`, `sha512:HEX`, `sha1:HEX`, `md5:HEX`,
/// `b2:HEX`, `b2sum:HEX`, or plain `HEX` (assumed sha256).
fn verify_file_hash(path: &Path, expected: &str) -> Result<bool> {
    use anyhow::bail;
    use std::io::Read;

    let exp = expected.trim();
    if exp.eq_ignore_ascii_case("skip") {
        crate::log_info!("Checksum verification skipped");
        return Ok(true);
    }

    // parse `alg:hex` or default to sha256 when no algorithm given
    let (alg, hex) = if let Some(pos) = exp.find(':') {
        let a = exp[..pos].trim().to_ascii_lowercase();
        let h = exp[pos + 1..].trim().to_ascii_lowercase();
        let alg = if a.is_empty() {
            "sha256".to_string()
        } else {
            a
        };
        (alg, h.to_string())
    } else {
        ("sha256".to_string(), exp.to_ascii_lowercase())
    };

    match alg.as_str() {
        "sha256" => {
            let mut f = fs::File::open(path)?;
            let mut hasher = Sha256::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let actual = crate::hex::encode_lower(hasher.finalize());
            Ok(actual == hex)
        }
        "sha512" => {
            use sha2::Sha512;
            let mut f = fs::File::open(path)?;
            let mut hasher = Sha512::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let actual = crate::hex::encode_lower(hasher.finalize());
            Ok(actual == hex)
        }
        "sha1" => {
            use sha1::Sha1;
            let mut f = fs::File::open(path)?;
            let mut hasher = Sha1::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let actual = crate::hex::encode_lower(hasher.finalize());
            Ok(actual == hex)
        }
        "md5" => {
            let mut ctx = md5::Context::new();
            let mut f = fs::File::open(path)?;
            let mut buf = [0u8; 8192];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                ctx.consume(&buf[..n]);
            }
            let digest = ctx.finalize();
            let actual = crate::hex::encode_lower(*digest);
            Ok(actual == hex)
        }
        "b2" | "b2sum" => {
            let actual = b2sum_rust::Blake2bSum::new(64).read(path);
            Ok(actual.eq_ignore_ascii_case(&hex))
        }
        _ => bail!("Unsupported checksum algorithm: {}", alg),
    }
}

/// Build a blocking reqwest HTTP client using the configured Cargo TLS backend.
/// Any error building the client is returned directly (no fallback).
pub(crate) fn build_blocking_client(
    user_agent: &str,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder().user_agent(user_agent.to_string());
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }

    builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {}", e))
}

/// Fetch + extract all sources.
///
/// Returns the primary source directory (the first source entry, or work_dir for manual-only packages).
pub fn prepare(spec: &PackageSpec, cache_dir: &Path, build_dir: &Path) -> Result<PathBuf> {
    // If no remote sources, create work_dir and copy manual sources there
    if spec.sources().is_empty() {
        let work_dir = build_dir.join(&spec.package.name);
        fs::create_dir_all(&work_dir)?;
        copy_manual_sources(spec, cache_dir, &work_dir)?;
        return Ok(work_dir);
    }

    // Copy manual sources first (before any remote fetching)
    copy_manual_sources(spec, cache_dir, build_dir)?;

    let mut primary: Option<PathBuf> = None;

    for (idx, src) in spec.sources().iter().enumerate() {
        crate::interrupts::check()?;
        let src_dir = prepare_one(spec, src, cache_dir, build_dir)
            .with_context(|| format!("Failed to prepare source #{}", idx + 1))?;
        if idx == 0 {
            primary = Some(src_dir);
        }
    }

    primary.ok_or_else(|| anyhow::anyhow!("No sources in spec"))
}

fn prepare_one(
    spec: &PackageSpec,
    source: &crate::package::Source,
    cache_dir: &Path,
    build_dir: &Path,
) -> Result<PathBuf> {
    let url = spec.expand_vars(&source.url);
    let extract_dir_name = spec.expand_vars(&source.extract_dir);
    let cherry_pick_revs: Vec<String> = source
        .cherry_pick
        .iter()
        .map(|rev| spec.expand_vars(rev))
        .collect();

    // Treat `<url>#<rev>` and bare `*.git` sources as git before local file://
    // handling so `file://...repo.git#tag` resolves through the git checkout path.
    if let Some((base, rev)) = split_hg_url(&url) {
        let checkout_dir = build_dir.join(&extract_dir_name);
        if checkout_dir.exists() && checkout_dir.join(".depot_state").exists() {
            crate::log_info!(
                "Resuming build in existing hg directory: {}",
                checkout_dir.display()
            );
            return Ok(checkout_dir);
        }
        checkout_hg(&base, &rev, &checkout_dir)?;
        hooks::post_extract(spec, source, &checkout_dir, cache_dir)?;
        return Ok(checkout_dir);
    }

    if let Some((base, rev)) = split_git_url(&url) {
        let checkout_dir = build_dir.join(&extract_dir_name);
        if checkout_dir.exists() && checkout_dir.join(".depot_state").exists() {
            crate::log_info!(
                "Resuming build in existing git directory: {}",
                checkout_dir.display()
            );
            return Ok(checkout_dir);
        }
        git::checkout(
            &base,
            &rev,
            &checkout_dir,
            &cache_dir.join("git"),
            &spec.package.name,
            &cherry_pick_revs,
        )?;
        hooks::post_extract(spec, source, &checkout_dir, cache_dir)?;
        return Ok(checkout_dir);
    }

    // Local file:// handling (directories or archives)
    if let Some(path_str) = url.strip_prefix("file://") {
        let local_path = PathBuf::from(path_str);
        if local_path.is_dir() {
            let dst = build_dir.join(&extract_dir_name);
            if dst.exists() {
                // If it exists and has a state file, assume we are resuming
                if dst.join(".depot_state").exists() {
                    crate::log_info!(
                        "Resuming build in existing source directory: {}",
                        dst.display()
                    );
                    return Ok(dst);
                }
                fs::remove_dir_all(&dst)?;
            }
            copy_dir_recursive(&local_path, &dst)?;
            hooks::post_extract(spec, source, &dst, cache_dir)?;
            return Ok(dst);
        } else if local_path.is_file() {
            let src_dir = build_dir.join(&extract_dir_name);
            if src_dir.exists() {
                if src_dir.join(".depot_state").exists() {
                    crate::log_info!(
                        "Resuming build in existing source directory: {}",
                        src_dir.display()
                    );
                    return Ok(src_dir);
                }
                // If no state file, or we want a fresh start, we'd delete it.
                // For now, let's just delete if no state file.
                fs::remove_dir_all(&src_dir)?;
            }
            let src_dir = extractor::extract_archive(&local_path, spec, source, build_dir)?;
            hooks::post_extract(spec, source, &src_dir, cache_dir)?;
            return Ok(src_dir);
        } else {
            bail!("Local file source not found: {}", local_path.display());
        }
    }

    if !source.cherry_pick.is_empty() {
        bail!(
            "source.cherry_pick is only supported for git sources (got URL: {})",
            source.url
        );
    }

    let src_dir = build_dir.join(&extract_dir_name);
    if src_dir.exists() && src_dir.join(".depot_state").exists() {
        crate::log_info!(
            "Resuming build in existing source directory: {}",
            src_dir.display()
        );
        return Ok(src_dir);
    }

    let archive_path = fetcher::fetch_archive(spec, source, cache_dir)?;
    let src_dir = extractor::extract_archive(&archive_path, spec, source, build_dir)?;
    hooks::post_extract(spec, source, &src_dir, cache_dir)?;
    Ok(src_dir)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    crate::fs_copy::copy_tree_preserving_links(src, dst)
}

pub(crate) fn split_git_url(url: &str) -> Option<(String, String)> {
    // Check for explicit revision with #
    if let Some((base, rev)) = url.split_once('#') {
        // Ignore fragment for obvious archives.
        let lower = base.to_ascii_lowercase();
        let is_archive = lower.ends_with(".tar.gz")
            || lower.ends_with(".tgz")
            || lower.ends_with(".tar.xz")
            || lower.ends_with(".txz")
            || lower.ends_with(".tar.bz2")
            || lower.ends_with(".tbz2")
            || lower.ends_with(".zip")
            || lower.ends_with(".tar");
        if is_archive {
            return None;
        }
        if rev.trim().is_empty() {
            // Empty revision after # - use HEAD
            return Some((base.to_string(), "HEAD".to_string()));
        }
        return Some((base.to_string(), rev.to_string()));
    }

    // Check for bare .git URL or explicit git:// URL without revision - default to HEAD.
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".git") || lower.starts_with("git://") {
        return Some((url.to_string(), "HEAD".to_string()));
    }

    None
}

fn split_hg_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("hg+")?;
    if let Some((base, rev)) = rest.split_once('#') {
        let revision = if rev.trim().is_empty() {
            "tip"
        } else {
            rev.trim()
        };
        return Some((base.to_string(), revision.to_string()));
    }
    Some((rest.to_string(), "tip".to_string()))
}

fn checkout_hg(url: &str, rev: &str, checkout_dir: &Path) -> Result<()> {
    if checkout_dir.exists() {
        fs::remove_dir_all(checkout_dir).with_context(|| {
            format!(
                "Failed to remove existing Mercurial checkout dir: {}",
                checkout_dir.display()
            )
        })?;
    }

    crate::log_info!("Cloning Mercurial source {} @ {}...", url, rev);
    let mut cmd = Command::new("hg");
    cmd.arg("clone")
        .arg("-u")
        .arg(rev)
        .arg(url)
        .arg(checkout_dir);
    cmd.env("PATH", crate::runtime_env::safe_script_path());
    let status = crate::interrupts::command_status(&mut cmd)
        .with_context(|| format!("Failed to run hg clone for {}", url))?;
    if !status.success() {
        bail!("Mercurial clone failed for {} @ {}", url, rev);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
