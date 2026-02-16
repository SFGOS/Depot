//! Source tarball fetching with checksum verification

use crate::package::{PackageSpec, Source};
use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use url::Url;

/// Fetch an archive source tarball, returning path to downloaded file.
pub fn fetch_archive(spec: &PackageSpec, source: &Source, cache_dir: &Path) -> Result<PathBuf> {
    let url = spec.expand_vars(&source.url);
    let filename = derive_filename_from_url(&url);
    let dest_path = cache_dir.join(&filename);

    // Create cache directory if needed
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("Failed to create cache dir: {}", cache_dir.display()))?;

    // Check if already cached and verified
    if dest_path.exists() && verify_checksum(&dest_path, &source.sha256)? {
        println!("Using cached source: {}", dest_path.display());
        return Ok(dest_path);
    }

    println!("Fetching: {}", url);

    // Download with progress bar
    let client = reqwest::blocking::Client::new();
    let mut response = client
        .get(&url)
        .send()
        .with_context(|| format!("Failed to fetch: {}", url))?;

    let total_size = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("#>-"),
    );

    let mut file = File::create(&dest_path)
        .with_context(|| format!("Failed to create: {}", dest_path.display()))?;

    let mut buffer = [0u8; 8192];
    let mut downloaded = 0u64;

    loop {
        let bytes_read = response.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read])?;
        downloaded += bytes_read as u64;
        pb.set_position(downloaded);
    }

    pb.finish_with_message("Download complete");

    // Verify checksum
    if !verify_checksum(&dest_path, &source.sha256)? {
        fs::remove_file(&dest_path)?;
        bail!("Checksum verification failed for {}", filename);
    }

    println!("Checksum verified: {}", filename);
    Ok(dest_path)
}

/// Verify SHA256 checksum of a file
fn verify_checksum(path: &Path, expected: &str) -> Result<bool> {
    // Skip verification if requested
    if expected.to_lowercase() == "skip" {
        println!("Checksum verification skipped");
        return Ok(true);
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    let result = hasher.finalize();
    let actual = format!("{:x}", result);

    Ok(actual == expected.to_lowercase())
}

/// Derive a stable filename from a URL.
///
/// Rules:
/// - Parse as URL and use the last path segment if it looks like a filename (contains a dot)
/// - Otherwise fall back to a stable hash-based name: source-{sha256(url)[..12]}.download
fn derive_filename_from_url(url: &str) -> String {
    // try to parse the URL
    if let Some(last) = Url::parse(url).ok().and_then(|parsed| {
        parsed
            .path_segments()?
            .rfind(|s| !s.is_empty())
            .filter(|l| l.contains('.'))
            .map(|l| l.to_string())
    }) {
        return last;
    }

    // fallback: stable short-hash name
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    let h = hasher.finalize();
    let hex = format!("{:x}", h);
    format!("source-{}.download", &hex[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_from_simple_url() {
        assert_eq!(
            derive_filename_from_url("https://example.com/foo-1.2.3.tar.gz"),
            "foo-1.2.3.tar.gz"
        );
    }

    #[test]
    fn filename_from_url_with_query() {
        assert_eq!(
            derive_filename_from_url("https://example.com/foo.tar.gz?raw=1"),
            "foo.tar.gz"
        );
    }

    #[test]
    fn filename_from_url_without_real_name() {
        let name = derive_filename_from_url("https://github.com/org/repo/releases/download?id=123");
        assert!(name.starts_with("source-") && name.ends_with(".download"));
    }

    #[test]
    fn filename_from_non_url_string() {
        let name = derive_filename_from_url("not-a-url-at-all");
        assert!(name.starts_with("source-") && name.ends_with(".download"));
    }
}
