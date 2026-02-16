//! Source fetching and extraction

mod extractor;
mod fetcher;
mod git;
pub mod hooks;

use crate::package::PackageSpec;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Copy manual (local) sources to the build directory before fetching remote sources.
///
/// Manual sources are checked first, allowing local files like utility source code
/// to be available during the build process.
pub fn copy_manual_sources(spec: &PackageSpec, build_dir: &Path) -> Result<()> {
    if spec.manual_sources.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(build_dir)?;
    println!("Copying {} manual source(s)...", spec.manual_sources.len());

    for manual in &spec.manual_sources {
        let src_path = spec.spec_dir.join(&manual.file);
        if !src_path.exists() {
            bail!(
                "Manual source not found: {} (expected at {})",
                manual.file,
                src_path.display()
            );
        }

        // Verify SHA256 if provided (unless "skip")
        if let Some(expected_hash) = manual.sha256.as_ref().filter(|h| *h != "skip") {
            let actual_hash = compute_sha256(&src_path)?;
            if actual_hash != *expected_hash {
                bail!(
                    "SHA256 mismatch for {}: expected {}, got {}",
                    manual.file,
                    expected_hash,
                    actual_hash
                );
            }
        }

        // Determine destination
        let dest_name = manual.dest.as_deref().unwrap_or(&manual.file);
        let dest_path = build_dir.join(dest_name);

        // Create parent directories if needed
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }

        println!("  {} -> {}", manual.file, dest_path.display());
        fs::copy(&src_path, &dest_path).with_context(|| {
            format!(
                "Failed to copy {} to {}",
                src_path.display(),
                dest_path.display()
            )
        })?;
    }

    Ok(())
}

fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Fetch + extract all sources.
///
/// Returns the primary source directory (the first source entry, or work_dir for manual-only packages).
pub fn prepare(spec: &PackageSpec, cache_dir: &Path, build_dir: &Path) -> Result<PathBuf> {
    // If no remote sources, create work_dir and copy manual sources there
    if spec.sources().is_empty() {
        let work_dir = build_dir.join(&spec.package.name);
        fs::create_dir_all(&work_dir)?;
        copy_manual_sources(spec, &work_dir)?;
        return Ok(work_dir);
    }

    // Copy manual sources first (before any remote fetching)
    copy_manual_sources(spec, build_dir)?;

    let mut primary: Option<PathBuf> = None;

    for (idx, src) in spec.sources().iter().enumerate() {
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

    // Local file:// handling (directories or archives)
    if let Some(path_str) = url.strip_prefix("file://") {
        let local_path = PathBuf::from(path_str);
        if local_path.is_dir() {
            let dst = build_dir.join(&extract_dir_name);
            if dst.exists() {
                fs::remove_dir_all(&dst)?;
            }
            copy_dir_recursive(&local_path, &dst)?;
            hooks::post_extract(spec, source, &dst, cache_dir)?;
            return Ok(dst);
        } else if local_path.is_file() {
            let src_dir = extractor::extract_archive(&local_path, spec, source, build_dir)?;
            hooks::post_extract(spec, source, &src_dir, cache_dir)?;
            return Ok(src_dir);
        } else {
            bail!("Local file source not found: {}", local_path.display());
        }
    }

    // Heuristic: if the URL contains '#', treat it as a git URL with a revision.
    // (except when it clearly looks like an archive URL)
    if let Some((base, rev)) = split_git_url(&url) {
        let checkout_dir = build_dir.join(&extract_dir_name);
        git::checkout(
            &base,
            &rev,
            &checkout_dir,
            &cache_dir.join("git"),
            &spec.package.name,
        )?;
        hooks::post_extract(spec, source, &checkout_dir, cache_dir)?;
        return Ok(checkout_dir);
    }

    let archive_path = fetcher::fetch_archive(spec, source, cache_dir)?;
    let src_dir = extractor::extract_archive(&archive_path, spec, source, build_dir)?;
    hooks::post_extract(spec, source, &src_dir, cache_dir)?;
    Ok(src_dir)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_symlink() {
            let target_link = fs::read_link(entry.path())?;
            unix_fs::symlink(target_link, &target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn split_git_url(url: &str) -> Option<(String, String)> {
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

    // Check for bare .git URL without revision - default to HEAD
    let lower = url.to_ascii_lowercase();
    if lower.ends_with(".git") {
        return Some((url.to_string(), "HEAD".to_string()));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::split_git_url;

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
}
