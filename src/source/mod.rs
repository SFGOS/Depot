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
mod tests {
    use super::*;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, ManualSource, PackageInfo,
        PackageSpec, Source,
    };
    use git2::{Oid, Repository};
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

    fn make_git_source_spec(source_url: String, extract_dir: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                built_against: Vec::new(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: source_url,
                sha256: "skip".into(),
                extract_dir: extract_dir.into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    fn make_remote_git_repo() -> (tempfile::TempDir, String, Oid, Oid) {
        let tmp = tempfile::tempdir().unwrap();
        let remote_dir = tmp.path().join("origin.git");
        let workdir = tmp.path().join("work");

        Repository::init_bare(&remote_dir).unwrap();
        let repo = Repository::init(&workdir).unwrap();
        let tagged = commit_file(&repo, &workdir, "README", "tagged\n");
        let tag_target = repo.find_object(tagged, None).unwrap();
        repo.tag_lightweight("v1.0.0", &tag_target, false).unwrap();
        let hashed = commit_file(&repo, &workdir, "README", "hashed\n");

        let branch_ref = repo.head().unwrap().name().unwrap().to_string();
        let mut remote = repo.remote("origin", remote_dir.to_str().unwrap()).unwrap();
        let push_specs = [
            format!("{branch_ref}:{branch_ref}"),
            "refs/tags/v1.0.0:refs/tags/v1.0.0".to_string(),
        ];
        let push_spec_refs: Vec<&String> = push_specs.iter().collect();
        remote.push(&push_spec_refs, None).unwrap();

        let remote_url = url::Url::from_file_path(&remote_dir).unwrap().to_string();
        (tmp, remote_url, tagged, hashed)
    }

    fn mk_spec_with_manuals(spec_dir: PathBuf, manuals: Vec<ManualSource>) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                built_against: Vec::new(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: manuals,
            source: vec![Source {
                url: "https://example.com/src.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "src".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir,
        }
    }

    #[test]
    fn split_git_url_accepts_git_with_rev() {
        let (base, rev) = split_git_url("https://example.com/repo.git#v1.2.3").unwrap();
        assert_eq!(base, "https://example.com/repo.git");
        assert_eq!(rev, "v1.2.3");
    }

    #[test]
    fn split_git_url_accepts_bare_git_url() {
        let (base, rev) = split_git_url("https://example.com/repo.git").unwrap();
        assert_eq!(base, "https://example.com/repo.git");
        assert_eq!(rev, "HEAD");
    }

    #[test]
    fn split_git_url_accepts_bare_git_scheme_url() {
        let (base, rev) = split_git_url("git://git.suckless.org/ubase").unwrap();
        assert_eq!(base, "git://git.suckless.org/ubase");
        assert_eq!(rev, "HEAD");
    }

    #[test]
    fn split_git_url_rejects_archive_urls() {
        assert!(split_git_url("https://example.com/foo.tar.gz#deadbeef").is_none());
        assert!(split_git_url("https://example.com/foo.zip#v1").is_none());
    }

    #[test]
    fn split_git_url_empty_rev_defaults_to_head() {
        let (base, rev) = split_git_url("https://example.com/repo.git#").unwrap();
        assert_eq!(base, "https://example.com/repo.git");
        assert_eq!(rev, "HEAD");
    }

    #[test]
    fn split_git_url_accepts_expanded_tag_or_hash_revision() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "json".into(),
                real_name: None,
                version: "3.11.3".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                built_against: Vec::new(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://github.com/nlohmann/json.git#v$version".into(),
                sha256: "skip".into(),
                extract_dir: "json-$version".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let expanded = spec.expand_vars(&spec.source[0].url);
        let (base, rev) = split_git_url(&expanded).unwrap();
        assert_eq!(base, "https://github.com/nlohmann/json.git");
        assert_eq!(rev, "v3.11.3");

        let (base, rev) =
            split_git_url("https://github.com/nlohmann/json.git#0123456789abcdef").unwrap();
        assert_eq!(base, "https://github.com/nlohmann/json.git");
        assert_eq!(rev, "0123456789abcdef");
    }

    #[test]
    fn split_hg_url_accepts_revision_and_default_tip() {
        let (base, rev) = split_hg_url("hg+https://hg.example.test/repo#v1").unwrap();
        assert_eq!(base, "https://hg.example.test/repo");
        assert_eq!(rev, "v1");

        let (base, rev) = split_hg_url("hg+https://hg.example.test/repo").unwrap();
        assert_eq!(base, "https://hg.example.test/repo");
        assert_eq!(rev, "tip");
    }

    #[test]
    fn prepare_one_rejects_cherry_pick_for_non_git_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("cache");
        let build_dir = tmp.path().join("build");
        let mut spec = mk_spec_with_manuals(PathBuf::from("."), Vec::new());
        spec.source[0].url = "https://example.com/foo.tar.gz".into();
        spec.source[0].cherry_pick = vec!["deadbeef".into()];

        let err = prepare_one(&spec, &spec.source[0], &cache_dir, &build_dir)
            .expect_err("non-git source with cherry_pick must be rejected");
        assert!(
            err.to_string()
                .contains("source.cherry_pick is only supported for git sources")
        );
    }

    #[test]
    fn prepare_one_checks_out_git_tag_revision() {
        let (_tmp, remote_url, tagged, _hashed) = make_remote_git_repo();
        let cache_dir = tempfile::tempdir().unwrap();
        let build_dir = tempfile::tempdir().unwrap();
        let spec = make_git_source_spec(format!("{remote_url}#v1.0.0"), "src-tag");

        let checkout_dir =
            prepare_one(&spec, &spec.source[0], cache_dir.path(), build_dir.path()).unwrap();
        let repo = Repository::open(&checkout_dir).unwrap();

        assert_eq!(repo.head().unwrap().target().unwrap(), tagged);
        assert_eq!(
            std::fs::read_to_string(checkout_dir.join("README")).unwrap(),
            "tagged\n"
        );
    }

    #[test]
    fn prepare_one_checks_out_git_commit_hash_revision() {
        let (_tmp, remote_url, _tagged, hashed) = make_remote_git_repo();
        let cache_dir = tempfile::tempdir().unwrap();
        let build_dir = tempfile::tempdir().unwrap();
        let spec = make_git_source_spec(format!("{remote_url}#{hashed}"), "src-hash");

        let checkout_dir =
            prepare_one(&spec, &spec.source[0], cache_dir.path(), build_dir.path()).unwrap();
        let repo = Repository::open(&checkout_dir).unwrap();

        assert_eq!(repo.head().unwrap().target().unwrap(), hashed);
        assert_eq!(
            std::fs::read_to_string(checkout_dir.join("README")).unwrap(),
            "hashed\n"
        );
    }

    #[test]
    fn verify_file_hash_accepts_multiple_algorithms() {
        use sha1::Sha1;
        use sha2::{Digest, Sha256, Sha512};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"abc").unwrap();

        let sha256_hex = {
            let mut h = Sha256::new();
            h.update(b"abc");
            crate::hex::encode_lower(h.finalize())
        };
        let sha512_hex = {
            let mut h = Sha512::new();
            h.update(b"abc");
            crate::hex::encode_lower(h.finalize())
        };
        let sha1_hex = {
            let mut h = Sha1::new();
            h.update(b"abc");
            crate::hex::encode_lower(h.finalize())
        };
        let md5_hex = format!("{:x}", md5::compute(b"abc"));
        let b2_hex = b2sum_rust::Blake2bSum::new(64)
            .read(tmp.path())
            .to_ascii_lowercase();

        assert!(verify_file_hash(tmp.path(), &sha256_hex).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("sha256:{}", sha256_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("sha512:{}", sha512_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("sha1:{}", sha1_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("md5:{}", md5_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("b2:{}", b2_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("b2sum:{}", b2_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!(":{}", sha256_hex)).unwrap());
        assert!(!verify_file_hash(tmp.path(), "md5:deadbeef").unwrap());
    }

    #[test]
    fn build_blocking_client_with_and_without_timeout() {
        use std::time::Duration;
        let ua = "depot/test";
        let c1 = build_blocking_client(ua, None).expect("client build failed");
        assert!(c1.get("https://example.com").build().is_ok());

        let c2 =
            build_blocking_client(ua, Some(Duration::from_secs(5))).expect("client build failed");
        assert!(c2.get("https://example.com").build().is_ok());
    }

    #[test]
    fn copy_manual_sources_local_file_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let cache_dir = tmp.path().join("cache");
        let build_dir = tmp.path().join("build");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::write(spec_dir.join("manual.patch"), "patch-data").unwrap();

        let spec = mk_spec_with_manuals(
            spec_dir.clone(),
            vec![ManualSource {
                file: Some("manual.patch".into()),
                files: Vec::new(),
                url: None,
                urls: Vec::new(),
                sha256: None,
                dest: None,
            }],
        );

        copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(build_dir.join("manual.patch")).unwrap(),
            "patch-data"
        );
    }

    #[test]
    fn copy_manual_sources_url_mode_file_scheme() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let cache_dir = tmp.path().join("cache");
        let build_dir = tmp.path().join("build");
        std::fs::create_dir_all(&spec_dir).unwrap();
        let remote_file = tmp.path().join("remote-resource.txt");
        std::fs::write(&remote_file, "remote-data").unwrap();
        let url = format!("file://{}", remote_file.display());

        let spec = mk_spec_with_manuals(
            spec_dir,
            vec![ManualSource {
                file: None,
                files: Vec::new(),
                url: Some(url),
                urls: Vec::new(),
                sha256: Some("skip".into()),
                dest: Some("assets/manual.txt".into()),
            }],
        );

        copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(build_dir.join("assets/manual.txt")).unwrap(),
            "remote-data"
        );
    }

    #[test]
    fn preflight_manual_sources_accepts_git_url() {
        let (_tmp, remote_url, _tagged, hashed) = make_remote_git_repo();
        let spec = mk_spec_with_manuals(
            PathBuf::from("."),
            vec![ManualSource {
                file: None,
                files: Vec::new(),
                url: Some(format!("{remote_url}#{hashed}")),
                urls: Vec::new(),
                sha256: None,
                dest: None,
            }],
        );
        let cache_dir = tempfile::tempdir().unwrap();

        preflight_manual_sources(&spec, cache_dir.path()).unwrap();
    }

    #[test]
    fn copy_manual_sources_git_url_mode_checks_out_repository() {
        let (_tmp, remote_url, _tagged, hashed) = make_remote_git_repo();
        let spec = mk_spec_with_manuals(
            PathBuf::from("."),
            vec![ManualSource {
                file: None,
                files: Vec::new(),
                url: Some(format!("{remote_url}#{hashed}")),
                urls: Vec::new(),
                sha256: None,
                dest: None,
            }],
        );
        let cache_dir = tempfile::tempdir().unwrap();
        let build_dir = tempfile::tempdir().unwrap();

        copy_manual_sources(&spec, cache_dir.path(), build_dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(build_dir.path().join("origin/README")).unwrap(),
            "hashed\n"
        );
    }

    #[test]
    fn copy_manual_sources_multi_files_in_one_block() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let cache_dir = tmp.path().join("cache");
        let build_dir = tmp.path().join("build");
        std::fs::create_dir_all(spec_dir.join("pam")).unwrap();
        std::fs::write(spec_dir.join("pam/other"), "other").unwrap();
        std::fs::write(spec_dir.join("pam/system-auth"), "auth").unwrap();

        let spec = mk_spec_with_manuals(
            spec_dir.clone(),
            vec![ManualSource {
                file: None,
                files: vec!["pam/other".into(), "pam/system-auth".into()],
                url: None,
                urls: Vec::new(),
                sha256: None,
                dest: None,
            }],
        );

        copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(build_dir.join("pam/other")).unwrap(),
            "other"
        );
        assert_eq!(
            std::fs::read_to_string(build_dir.join("pam/system-auth")).unwrap(),
            "auth"
        );
    }

    #[test]
    fn copy_manual_sources_expands_carch_in_files_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let cache_dir = tmp.path().join("cache");
        let build_dir = tmp.path().join("build");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::write(spec_dir.join("build.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(spec_dir.join("config.armv7"), "armv7-config").unwrap();

        let mut spec = mk_spec_with_manuals(
            spec_dir.clone(),
            vec![ManualSource {
                file: None,
                files: vec!["build.sh".into(), "config.$CARCH".into()],
                url: None,
                urls: Vec::new(),
                sha256: None,
                dest: None,
            }],
        );
        spec.build.flags.carch = "armv7".into();

        copy_manual_sources(&spec, &cache_dir, &build_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(build_dir.join("build.sh")).unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            std::fs::read_to_string(build_dir.join("config.armv7")).unwrap(),
            "armv7-config"
        );
    }
}
