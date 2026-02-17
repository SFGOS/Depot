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

    // Parse URL early so we can handle non-HTTP schemes (FTP support)
    let parsed_url = Url::parse(&url).with_context(|| format!("Invalid URL: {}", url))?;

    // If this is an FTP URL, fetch via the ftp crate into the cache and continue
    if parsed_url.scheme() == "ftp" {
        // Connect and login (anonymous fallback)
        let host = parsed_url.host_str().context("FTP URL missing host")?;
        let port = parsed_url.port_or_known_default().unwrap_or(21);
        let addr = format!("{}:{}", host, port);
        let mut ftp_stream = ftp::FtpStream::connect(addr.as_str())
            .with_context(|| format!("Failed to connect to FTP host: {}", addr))?;
        let user = if parsed_url.username().is_empty() { "anonymous" } else { parsed_url.username() };
        let pass = parsed_url.password().unwrap_or("anonymous@");
        ftp_stream.login(user, pass).with_context(|| format!("FTP login failed for {}", host))?;

        // Retrieve the path (try with and without leading slash)
        let path = parsed_url.path();
        let candidates = [path.to_string(), path.trim_start_matches('/').to_string()];
        let mut retrieved = false;
        for p in candidates.iter().filter(|s| !s.is_empty()) {
            match ftp_stream.retr(p, |reader: &mut dyn Read| -> std::result::Result<(), ftp::FtpError> {
                let mut file = File::create(&dest_path).map_err(ftp::FtpError::ConnectionError)?;
                let mut buffer = [0u8; 8192];
                loop {
                    let bytes_read = reader.read(&mut buffer).map_err(ftp::FtpError::ConnectionError)?;
                    if bytes_read == 0 { break; }
                    file.write_all(&buffer[..bytes_read]).map_err(ftp::FtpError::ConnectionError)?;
                }
                Ok(())
            }) {
                Ok(_) => {
                    retrieved = true;
                    break;
                }
                Err(_) => continue,
            }
        }
        ftp_stream.quit().ok();
        if !retrieved {
            bail!("FTP error fetching {}", url);
        }
    }

    // Download with progress bar
    // Use a sensible default User-Agent so servers that reject empty/unknown agents (e.g. IANA)
    // will accept requests. Include package name/version at compile time.
    let ua = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let client = reqwest::blocking::Client::builder()
        .user_agent(ua)
        .build()
        .with_context(|| "Failed to build HTTP client")?;

    let mut response = client
        .get(&url)
        .send()
        .with_context(|| format!("Failed to fetch: {}", url))?;
    // If the server returned a non-success status, read a short body preview and fail early.
    // This prevents saving HTML error pages (which then fail checksum) and gives a clearer
    // diagnostic to the user.
    let status = response.status();
    if !status.is_success() {
        let mut preview_bytes = Vec::new();
        // read up to 1 KiB for a preview (ignore errors while reading preview)
        let _ = response.take(1024).read_to_end(&mut preview_bytes);
        let preview = String::from_utf8_lossy(&preview_bytes);
        bail!(
            "HTTP error fetching {}: {}{}",
            url,
            status,
            if preview.trim().is_empty() {
                "".to_string()
            } else {
                format!(" — preview: {}", preview.trim())
            }
        );
    }
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

    // Quick validation: ensure the downloaded file looks like the expected
    // archive (detect obvious HTML error pages or wrong formats by magic).
    // If validate_downloaded_archive returns an alternate URL (e.g. SourceForge
    // mirror), retry the download with that URL once.
    if let Some(alt) = validate_downloaded_archive(&dest_path, &filename, &url)? {
        println!("Retrying download from mirror: {}", alt);
        fs::remove_file(&dest_path).ok();

        // If mirror URL is FTP -> use ftp crate; otherwise use HTTP retry.
        if let Ok(alt_url) = Url::parse(&alt) {
            if alt_url.scheme() == "ftp" {
                // FTP mirror retrieval
                let host = alt_url.host_str().context("FTP mirror URL missing host")?;
                let port = alt_url.port_or_known_default().unwrap_or(21);
                let addr = format!("{}:{}", host, port);
                let mut ftp_stream = ftp::FtpStream::connect(addr.as_str())
                    .with_context(|| format!("Failed to connect to FTP host: {}", addr))?;
                let user = if alt_url.username().is_empty() { "anonymous" } else { alt_url.username() };
                let pass = alt_url.password().unwrap_or("anonymous@");
                ftp_stream.login(user, pass).with_context(|| format!("FTP login failed for {}", host))?;

                let path = alt_url.path();
                let candidates = [path.to_string(), path.trim_start_matches('/').to_string()];
                let mut retrieved = false;
                for p in candidates.iter().filter(|s| !s.is_empty()) {
                    match ftp_stream.retr(p, |reader: &mut dyn Read| -> std::result::Result<(), ftp::FtpError> {
                        let mut file = File::create(&dest_path).map_err(ftp::FtpError::ConnectionError)?;
                        let mut buffer = [0u8; 8192];
                        let mut downloaded = 0u64;
                        loop {
                            let bytes_read = reader.read(&mut buffer).map_err(ftp::FtpError::ConnectionError)?;
                            if bytes_read == 0 { break; }
                            file.write_all(&buffer[..bytes_read]).map_err(ftp::FtpError::ConnectionError)?;
                            downloaded += bytes_read as u64;
                            pb.set_position(downloaded);
                        }
                        Ok(())
                    }) {
                        Ok(_) => {
                            retrieved = true;
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                ftp_stream.quit().ok();
                if !retrieved {
                    bail!("FTP mirror error fetching {}", alt);
                }
            } else {
                // HTTP(S) mirror retry (recreate client for retry)
                let ua = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                let client = reqwest::blocking::Client::builder()
                    .user_agent(ua)
                    .build()
                    .with_context(|| "Failed to build HTTP client")?;
                let mut response = client
                    .get(&alt)
                    .send()
                    .with_context(|| format!("Failed to fetch mirror URL: {}", alt))?;

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
            }
        } else {
            // Fallback: unknown/malformed alt URL -> bail
            bail!("Malformed mirror URL: {}", alt);
        }

        pb.finish_with_message("Download complete (mirror)");

        // Re-validate the mirrored file
        validate_downloaded_archive(&dest_path, &filename, &alt)?;
    }

    // Verify checksum
    if !verify_checksum(&dest_path, &source.sha256)? {
        fs::remove_file(&dest_path)?;
        bail!("Checksum verification failed for {}", filename);
    }

    println!("Checksum verified: {}", filename);
    Ok(dest_path)
}

/// Verify checksum of a file supporting optional algorithm prefix.
///
/// Supported formats:
/// - `sha256:<hex>` (or just `<hex>` — default)
/// - `sha512:<hex>`
/// - `md5:<hex>`
/// - `skip` to bypass verification
fn verify_checksum(path: &Path, expected: &str) -> Result<bool> {
    // Delegate to the shared checker in the parent `source` module.
    // The helper also understands the same string forms and "skip".
    super::verify_file_hash(path, expected)
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

/// Validate downloaded file's magic header to make sure it is the expected
/// archive format (avoids saving HTML pages or other unexpected content).
fn validate_downloaded_archive(path: &std::path::Path, filename: &str, orig_url: &str) -> Result<Option<String>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf)?;
    let head = &buf[..n.min(4096)];

    // Detect obvious HTML error pages (case-insensitive)
    let head_str = String::from_utf8_lossy(head).to_ascii_lowercase();
    if head_str.starts_with("<!doctype html") || head_str.starts_with("<html") || head_str.contains("<html") {
        // If this came from SourceForge project URL, try to extract a direct
        // downloads.sourceforge.net mirror link and return it for a retry.
        let body = String::from_utf8_lossy(head).to_string();
        if orig_url.contains("sourceforge.net") {
            // helper: find nearest href attribute before `pos` and support
            // both double- and single-quoted attributes.
            let find_href_before = |body: &str, pos: usize| -> Option<String> {
                if let Some(start) = body[..pos].rfind("href=\"") {
                    let rest = &body[start + 6..];
                    if let Some(end) = rest.find('"') {
                        return Some(rest[..end].to_string());
                    }
                }
                if let Some(start) = body[..pos].rfind("href='") {
                    let rest = &body[start + 6..];
                    if let Some(end) = rest.find('\'') {
                        return Some(rest[..end].to_string());
                    }
                }
                None
            };

            // look for downloads.sourceforge.net links in HTML
            if let Some(pos) = body.find("downloads.sourceforge.net") {
                if let Some(href) = find_href_before(&body, pos) {
                    let alt = if href.starts_with("//") {
                        format!("https:{}", href)
                    } else if href.starts_with("/") {
                        format!("https://downloads.sourceforge.net{}", href)
                    } else {
                        href
                    };
                    return Ok(Some(alt));
                }
            }

            // Also try to find '/download' link anywhere in the body
            if let Some(pos) = body.find("/download") {
                if let Some(href) = find_href_before(&body, pos) {
                    let alt = if href.starts_with("//") {
                        format!("https:{}", href)
                    } else {
                        href
                    };
                    return Ok(Some(alt));
                }
            }

            // Final fallback for SF project pages: append '/download' to the
            // original URL (this normally triggers the mirror redirect).
            return Ok(Some(format!("{}/download", orig_url.trim_end_matches('/'))));
        }

        let preview = String::from_utf8_lossy(&head[..head.len().min(1024)]);
        anyhow::bail!(
            "Downloaded file '{}' looks like HTML (not an archive). Preview: {}",
            filename,
            preview.trim()
        );
    }

    // Validate by extension (best-effort)
    let lower = filename.to_ascii_lowercase();
    let is_ok = if lower.ends_with(".tar.xz") || lower.ends_with(".txz") || lower.ends_with(".xz") {
        head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00])
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") || lower.ends_with(".gz") {
        head.starts_with(&[0x1F, 0x8B])
    } else if lower.ends_with(".tar.zst") || lower.ends_with(".tzst") || lower.ends_with(".zst") {
        head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD])
    } else if lower.ends_with(".zip") {
        head.starts_with(b"PK\x03\x04")
    } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
        head.starts_with(&[0x42, 0x5A, 0x68])
    } else if lower.ends_with(".tar") {
        // check for ustar magic at offset 257
        if let Ok(mut f2) = std::fs::File::open(path) {
            let mut hdr = [0u8; 262];
            if f2.read_exact(&mut hdr).is_ok() {
                &hdr[257..262] == b"ustar"
            } else {
                true // can't validate; be permissive
            }
        } else {
            true
        }
    } else if lower.ends_with(".deb") {
        // ar archive starts with "!<arch>\n"
        head.starts_with(b"!<arch>")
    } else if lower.ends_with(".rpm") {
        // rpm contains cpio magic later; best-effort: accept
        true
    } else {
        // Unknown extension -> be permissive
        true
    };

    if !is_ok {
        let preview = String::from_utf8_lossy(&head[..head.len().min(1024)]);
        anyhow::bail!(
            "Downloaded file '{}' does not match expected archive magic; preview: {}",
            filename,
            preview.trim()
        );
    }

    Ok(None)
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
    fn filename_from_ftp_url() {
        assert_eq!(derive_filename_from_url("ftp://example.com/foo-1.2.3.tar.gz"), "foo-1.2.3.tar.gz");
    }

    #[test]
    fn filename_from_non_url_string() {
        let name = derive_filename_from_url("not-a-url-at-all");
        assert!(name.starts_with("source-") && name.ends_with(".download"));
    }

    #[test]
    fn sourceforge_html_no_link_falls_back_to_download_suffix() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "<!doctype html><html><body>No direct link</body></html>").unwrap();
        let alt = validate_downloaded_archive(
            tmp.path(),
            "zsh-5.9.tar.xz",
            "https://sourceforge.net/projects/zsh/files/zsh/5.9/zsh-5.9.tar.xz",
        )
        .unwrap();
        assert_eq!(
            alt,
            Some(
                "https://sourceforge.net/projects/zsh/files/zsh/5.9/zsh-5.9.tar.xz/download"
                    .to_string()
            )
        );
    }

    #[test]
    fn sourceforge_html_single_quoted_href_extracts_downloads_link() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "<!doctype html><a href='//downloads.sourceforge.net/project/zsh/zsh/5.9/zsh-5.9.tar.xz?download'>download</a>").unwrap();
        let alt = validate_downloaded_archive(
            tmp.path(),
            "zsh-5.9.tar.xz",
            "https://sourceforge.net/projects/zsh/files/zsh/5.9/zsh-5.9.tar.xz",
        )
        .unwrap();
        assert_eq!(
            alt,
            Some("https://downloads.sourceforge.net/project/zsh/zsh/5.9/zsh-5.9.tar.xz?download".to_string())
        );
    }

    #[test]
    fn verify_checksum_accepts_md5_sha512_and_default_sha256() {
        use sha2::Digest;
        use sha2::Sha256;
        use sha2::Sha512;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"abc").unwrap();

        // compute expected values using the same libraries
        let sha256_hex = {
            let mut h = Sha256::new();
            h.update(b"abc");
            format!("{:x}", h.finalize())
        };

        let sha512_hex = {
            let mut h = Sha512::new();
            h.update(b"abc");
            format!("{:x}", h.finalize())
        };

        let md5_hex = format!("{:x}", md5::compute(b"abc"));

        // unprefixed should default to sha256
        assert!(verify_checksum(tmp.path(), &sha256_hex).unwrap());
        // explicit prefixes
        assert!(verify_checksum(tmp.path(), &format!("sha256:{}", sha256_hex)).unwrap());
        assert!(verify_checksum(tmp.path(), &format!("sha512:{}", sha512_hex)).unwrap());
        assert!(verify_checksum(tmp.path(), &format!("md5:{}", md5_hex)).unwrap());
        // empty algorithm before colon -> assume sha256
        assert!(verify_checksum(tmp.path(), &format!(":{}", sha256_hex)).unwrap());
        // negative: wrong value fails
        assert!(!verify_checksum(tmp.path(), "md5:deadbeef").unwrap());
    }
}


