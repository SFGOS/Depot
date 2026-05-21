//! Source tarball fetching with checksum verification

use crate::package::{PackageSpec, Source};
use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

const MAX_MIRROR_RETRIES: usize = 8;
const HTTP_FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const HTTP_FETCH_RETRY_LIMIT: usize = 2;
const MUSL_1_2_6_SNAPSHOT_URL: &str =
    "https://git.musl-libc.org/cgit/musl/snapshot/musl-1.2.6.tar.gz";
const MUSL_1_2_6_RELEASE_URL: &str = "https://musl.libc.org/releases/musl-1.2.6.tar.gz";
const MUSL_1_2_6_GENTOO_MIRROR_URL: &str =
    "https://tw.archive.ubuntu.com/gentoo/distfiles/9d/musl-1.2.6.tar.gz";

fn scheme_uses_http_transport(scheme: &str) -> bool {
    matches!(scheme, "http" | "https")
}

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
        crate::log_info!("Using cached source: {}", dest_path.display());
        return Ok(dest_path);
    }

    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
            )
            .unwrap()
            .progress_chars("#>-"),
    );

    let candidate_urls = archive_fetch_candidates(&url);
    let mut attempt_errors = Vec::new();

    for (index, candidate_url) in candidate_urls.iter().enumerate() {
        if index == 0 {
            crate::log_info!("Fetching: {}", candidate_url);
        } else {
            crate::log_info!("Primary source failed; trying fallback: {}", candidate_url);
        }

        pb.set_position(0);
        pb.set_length(0);
        fs::remove_file(&dest_path).ok();

        match fetch_archive_from_candidate(candidate_url, &dest_path, &filename, &pb) {
            Ok(()) => {
                if !verify_checksum(&dest_path, &source.sha256)? {
                    fs::remove_file(&dest_path)?;
                    attempt_errors.push(format!(
                        "{}: checksum verification failed for {}",
                        candidate_url, filename
                    ));
                    continue;
                }

                crate::log_info!("Checksum verified: {}", filename);
                return Ok(dest_path);
            }
            Err(err) => {
                attempt_errors.push(format!("{}: {err:#}", candidate_url));
                fs::remove_file(&dest_path).ok();
            }
        }
    }

    bail!(
        "Failed to fetch {} from any candidate URL:\n{}",
        filename,
        attempt_errors.join("\n")
    )
}

/// Verify checksum of a file supporting optional algorithm prefix.
///
/// Supported formats:
/// - `sha256:<hex>` (or just `<hex>` — default)
/// - `sha512:<hex>`
/// - `sha1:<hex>`
/// - `md5:<hex>`
/// - `b2:<hex>` / `b2sum:<hex>`
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
pub(crate) fn derive_filename_from_url(url: &str) -> String {
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
    let hex = crate::hex::encode_lower(h);
    format!("source-{}.download", &hex[..12])
}

fn archive_fetch_candidates(primary_url: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    if matches!(
        primary_url,
        MUSL_1_2_6_SNAPSHOT_URL | MUSL_1_2_6_RELEASE_URL | MUSL_1_2_6_GENTOO_MIRROR_URL
    ) {
        candidates.push(MUSL_1_2_6_GENTOO_MIRROR_URL.to_string());
    }
    if !candidates.iter().any(|url| url == primary_url) {
        candidates.push(primary_url.to_string());
    }
    if let Some(mirror_url) = musl_release_mirror_url(primary_url)
        && !candidates.contains(&mirror_url)
    {
        candidates.push(mirror_url);
    }
    candidates
}

fn musl_release_mirror_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if parsed.host_str()? != "git.musl-libc.org" {
        return None;
    }

    let mut segments = parsed.path_segments()?;
    match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some("cgit"), Some("musl"), Some("snapshot"), Some(filename), None)
            if !filename.trim().is_empty() =>
        {
            Some(format!("https://musl.libc.org/releases/{filename}"))
        }
        _ => None,
    }
}

fn fetch_archive_from_candidate(
    candidate_url: &str,
    dest_path: &Path,
    filename: &str,
    pb: &ProgressBar,
) -> Result<()> {
    let mut current_url = candidate_url.to_string();
    let mut seen_alts: HashSet<String> = HashSet::new();
    let mut retries = 0usize;

    loop {
        download_archive_from_url(&current_url, dest_path, pb)?;
        pb.finish_with_message("Download complete");

        let Some(next_alt) = validate_downloaded_archive(dest_path, filename, &current_url)? else {
            return Ok(());
        };

        retries += 1;
        if retries > MAX_MIRROR_RETRIES {
            bail!(
                "Exceeded mirror retry limit ({}) while fetching {}",
                MAX_MIRROR_RETRIES,
                candidate_url
            );
        }
        if !seen_alts.insert(next_alt.clone()) {
            bail!("Mirror retry loop detected for URL: {}", next_alt);
        }

        crate::log_info!("Retrying download from mirror: {}", next_alt);
        fs::remove_file(dest_path).ok();
        pb.set_position(0);
        pb.set_length(0);
        current_url = next_alt;
    }
}

fn download_archive_from_url(url: &str, dest_path: &Path, pb: &ProgressBar) -> Result<()> {
    let parsed_url = Url::parse(url).with_context(|| format!("Invalid URL: {}", url))?;
    if parsed_url.scheme() == "ftp" {
        return download_ftp_archive(&parsed_url, dest_path, pb);
    }
    if !scheme_uses_http_transport(parsed_url.scheme()) {
        bail!(
            "Unsupported URL scheme for source fetch: {}",
            parsed_url.scheme()
        );
    }
    download_http_archive(url, dest_path, pb)
}

fn download_ftp_archive(parsed_url: &Url, dest_path: &Path, pb: &ProgressBar) -> Result<()> {
    let host = parsed_url.host_str().context("FTP URL missing host")?;
    let port = parsed_url.port_or_known_default().unwrap_or(21);
    let addr = format!("{}:{}", host, port);
    let mut ftp_stream = suppaftp::FtpStream::connect(addr.as_str())
        .with_context(|| format!("Failed to connect to FTP host: {}", addr))?;
    let user = if parsed_url.username().is_empty() {
        "anonymous"
    } else {
        parsed_url.username()
    };
    let pass = parsed_url.password().unwrap_or("anonymous@");
    ftp_stream
        .login(user, pass)
        .with_context(|| format!("FTP login failed for {}", host))?;

    let path = parsed_url.path();
    let candidates = [path.to_string(), path.trim_start_matches('/').to_string()];
    let mut retrieved = false;
    for candidate in candidates.iter().filter(|path| !path.is_empty()) {
        match ftp_stream.retr(
            candidate,
            |reader: &mut dyn Read| -> std::result::Result<(), suppaftp::FtpError> {
                let mut file =
                    File::create(dest_path).map_err(suppaftp::FtpError::ConnectionError)?;
                let mut buffer = [0u8; 8192];
                let mut downloaded = 0u64;
                loop {
                    let bytes_read = reader
                        .read(&mut buffer)
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    if bytes_read == 0 {
                        break;
                    }
                    file.write_all(&buffer[..bytes_read])
                        .map_err(suppaftp::FtpError::ConnectionError)?;
                    downloaded += bytes_read as u64;
                    pb.set_position(downloaded);
                }
                Ok(())
            },
        ) {
            Ok(_) => {
                retrieved = true;
                break;
            }
            Err(_) => continue,
        }
    }
    ftp_stream.quit().ok();
    if !retrieved {
        bail!("FTP error fetching {}", parsed_url);
    }
    Ok(())
}

fn download_http_archive(url: &str, dest_path: &Path, pb: &ProgressBar) -> Result<()> {
    let ua = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    let mut last_err = None;

    for attempt in 1..=HTTP_FETCH_RETRY_LIMIT {
        match download_http_archive_once(url, dest_path, pb, &ua) {
            Ok(()) => return Ok(()),
            Err(err) if attempt < HTTP_FETCH_RETRY_LIMIT && is_transient_http_error(&err) => {
                crate::log_info!(
                    "Fetch attempt {} for {} failed with a transient network error; retrying",
                    attempt,
                    url
                );
                fs::remove_file(dest_path).ok();
                pb.set_position(0);
                pb.set_length(0);
                last_err = Some(err);
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Failed to fetch: {}", url)))
}

fn download_http_archive_once(
    url: &str,
    dest_path: &Path,
    pb: &ProgressBar,
    ua: &str,
) -> Result<()> {
    let client = super::build_blocking_client(ua, Some(HTTP_FETCH_TIMEOUT))
        .with_context(|| "Failed to build HTTP client")?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("Failed to fetch: {}", url))?;
    let status = response.status();
    if !status.is_success() {
        let mut preview_bytes = Vec::new();
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
    pb.set_length(total_size);

    let mut file = File::create(dest_path)
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

    Ok(())
}

fn is_transient_http_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|inner| inner.is_timeout() || inner.is_connect() || inner.is_request())
    })
}

/// Validate downloaded file's magic header to make sure it is the expected
/// archive format (avoids saving HTML pages or other unexpected content).
fn validate_downloaded_archive(
    path: &std::path::Path,
    filename: &str,
    orig_url: &str,
) -> Result<Option<String>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf)?;
    let head = &buf[..n.min(4096)];

    // Detect obvious HTML error pages (case-insensitive)
    if is_html_content(head) {
        if url_contains_sourceforge_host(orig_url) {
            let body = std::fs::read(path)
                .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
                .unwrap_or_else(|_| String::from_utf8_lossy(head).to_string());
            if let Some(alt) = sourceforge_alt_url_from_html(&body, orig_url) {
                return Ok(Some(alt));
            }
        }

        anyhow::bail!(
            "Downloaded file '{}' looks like HTML (not an archive). Preview: {}",
            filename,
            html_preview(head)
        );
    }

    // Validate by extension (best-effort)
    let lower = filename.to_ascii_lowercase();
    let is_ok = classify_archive_magic(head, &lower, path);

    if !is_ok {
        anyhow::bail!(
            "Downloaded file '{}' does not match expected archive magic; preview: {}",
            filename,
            html_preview(head)
        );
    }

    Ok(None)
}

fn sourceforge_alt_url_from_html(body: &str, orig_url: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    for href in extract_hrefs(body, &lower) {
        if href.contains("downloads.sourceforge.net")
            && let Some(url) = sourceforge_candidate_from_href(&href)
        {
            return Some(url);
        }
    }
    for href in extract_hrefs(body, &lower) {
        if href.contains("/download")
            && let Some(url) = sourceforge_candidate_from_href(&href)
        {
            return Some(url);
        }
    }
    sourceforge_download_fallback(orig_url)
}

fn extract_hrefs(body: &str, lower: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("href=") {
        let start = i + rel + 5;
        if start >= body.len() {
            break;
        }
        let quote = body.as_bytes()[start] as char;
        if quote != '"' && quote != '\'' {
            i = start + 1;
            continue;
        }
        let val_start = start + 1;
        if val_start > body.len() {
            break;
        }
        if let Some(end_rel) = body[val_start..].find(quote) {
            out.push(body[val_start..val_start + end_rel].to_string());
            i = val_start + end_rel + 1;
        } else {
            break;
        }
    }
    out
}

fn normalize_downloads_sf_href(href: &str) -> String {
    let href = href.replace("&amp;", "&");
    if href.starts_with("//") {
        format!("https:{}", href)
    } else if href.starts_with('/') {
        format!("https://downloads.sourceforge.net{}", href)
    } else {
        href
    }
}

fn normalize_sf_href(href: &str) -> String {
    let href = href.replace("&amp;", "&");
    if href.starts_with("//") {
        format!("https:{}", href)
    } else if href.starts_with('/') {
        format!("https://sourceforge.net{}", href)
    } else {
        href
    }
}

fn sourceforge_candidate_from_href(href: &str) -> Option<String> {
    let normalized = if href.contains("downloads.sourceforge.net") {
        normalize_downloads_sf_href(href)
    } else {
        normalize_sf_href(href)
    };
    let parsed = Url::parse(&normalized).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();

    if is_sourceforge_host(&host) || is_downloads_sourceforge_host(&host) {
        return Some(normalized);
    }

    // Some HTML pages use social share links that embed a real SourceForge URL
    // in a query parameter (e.g. x.com/share?url=...).
    if is_social_share_host(&host) {
        for (k, v) in parsed.query_pairs() {
            if (k.eq_ignore_ascii_case("url") || k.eq_ignore_ascii_case("u"))
                && let Ok(inner) = Url::parse(v.as_ref())
                && let Some(inner_host) = inner.host_str()
            {
                let inner_host = inner_host.to_ascii_lowercase();
                if is_sourceforge_host(&inner_host) || is_downloads_sourceforge_host(&inner_host) {
                    return Some(inner.to_string());
                }
            }
        }
    }

    None
}

fn sourceforge_download_fallback(orig_url: &str) -> Option<String> {
    let parsed = Url::parse(orig_url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();

    if is_sourceforge_host(&host) || is_downloads_sourceforge_host(&host) {
        return Some(format!("{}/download", orig_url.trim_end_matches('/')));
    }

    if is_social_share_host(&host) {
        for (k, v) in parsed.query_pairs() {
            if (k.eq_ignore_ascii_case("url") || k.eq_ignore_ascii_case("u"))
                && let Ok(inner) = Url::parse(v.as_ref())
                && let Some(inner_host) = inner.host_str()
            {
                let inner_host = inner_host.to_ascii_lowercase();
                if is_sourceforge_host(&inner_host) || is_downloads_sourceforge_host(&inner_host) {
                    return Some(format!("{}/download", inner.as_str().trim_end_matches('/')));
                }
            }
        }
    }

    None
}

fn is_sourceforge_host(host: &str) -> bool {
    host == "sourceforge.net" || host.ends_with(".sourceforge.net")
}

fn is_downloads_sourceforge_host(host: &str) -> bool {
    host == "downloads.sourceforge.net" || host.ends_with(".downloads.sourceforge.net")
}

fn is_social_share_host(host: &str) -> bool {
    matches!(
        host,
        "x.com"
            | "www.x.com"
            | "twitter.com"
            | "www.twitter.com"
            | "facebook.com"
            | "www.facebook.com"
            | "linkedin.com"
            | "www.linkedin.com"
            | "reddit.com"
            | "www.reddit.com"
            | "t.me"
            | "telegram.me"
    )
}

fn url_contains_sourceforge_host(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .map(|h| is_sourceforge_host(&h) || is_downloads_sourceforge_host(&h))
        .unwrap_or_else(|| {
            url.contains("://sourceforge.net/")
                || url.contains("://downloads.sourceforge.net/")
                || url.contains(".sourceforge.net/")
        })
}

fn html_preview(head: &[u8]) -> String {
    String::from_utf8_lossy(&head[..head.len().min(1024)])
        .trim()
        .to_string()
}

fn is_html_content(head: &[u8]) -> bool {
    let head_str = String::from_utf8_lossy(head).to_ascii_lowercase();
    head_str.starts_with("<!doctype html")
        || head_str.starts_with("<html")
        || head_str.contains("<html")
}

fn classify_archive_magic(head: &[u8], lower_filename: &str, path: &Path) -> bool {
    if lower_filename.ends_with(".tar.xz")
        || lower_filename.ends_with(".txz")
        || lower_filename.ends_with(".xz")
    {
        head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00])
    } else if lower_filename.ends_with(".tar.gz")
        || lower_filename.ends_with(".tgz")
        || lower_filename.ends_with(".gz")
    {
        head.starts_with(&[0x1F, 0x8B])
    } else if lower_filename.ends_with(".tar.zst")
        || lower_filename.ends_with(".tzst")
        || lower_filename.ends_with(".zst")
    {
        head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD])
    } else if lower_filename.ends_with(".zip") {
        head.starts_with(b"PK\x03\x04")
    } else if lower_filename.ends_with(".tar.bz2") || lower_filename.ends_with(".tbz2") {
        head.starts_with(&[0x42, 0x5A, 0x68])
    } else if lower_filename.ends_with(".tar") {
        if let Ok(mut f2) = std::fs::File::open(path) {
            let mut hdr = [0u8; 262];
            if f2.read_exact(&mut hdr).is_ok() {
                &hdr[257..262] == b"ustar"
            } else {
                true
            }
        } else {
            true
        }
    } else if lower_filename.ends_with(".deb") {
        head.starts_with(b"!<arch>")
    } else {
        // rpm/unknown extensions: keep permissive as before.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_uses_http_transport_only_for_http_and_https() {
        assert!(scheme_uses_http_transport("http"));
        assert!(scheme_uses_http_transport("https"));
        assert!(!scheme_uses_http_transport("ftp"));
        assert!(!scheme_uses_http_transport("file"));
    }

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
        assert_eq!(
            derive_filename_from_url("ftp://example.com/foo-1.2.3.tar.gz"),
            "foo-1.2.3.tar.gz"
        );
    }

    #[test]
    fn filename_from_non_url_string() {
        let name = derive_filename_from_url("not-a-url-at-all");
        assert!(name.starts_with("source-") && name.ends_with(".download"));
    }

    #[test]
    fn musl_snapshot_candidates_prefer_known_gentoo_mirror() {
        let candidates = archive_fetch_candidates(MUSL_1_2_6_SNAPSHOT_URL);
        assert_eq!(
            candidates,
            vec![
                MUSL_1_2_6_GENTOO_MIRROR_URL.to_string(),
                MUSL_1_2_6_SNAPSHOT_URL.to_string(),
                "https://musl.libc.org/releases/musl-1.2.6.tar.gz".to_string(),
            ]
        );
    }

    #[test]
    fn musl_release_candidates_prefer_known_gentoo_mirror() {
        let candidates = archive_fetch_candidates(MUSL_1_2_6_RELEASE_URL);
        assert_eq!(
            candidates,
            vec![
                MUSL_1_2_6_GENTOO_MIRROR_URL.to_string(),
                MUSL_1_2_6_RELEASE_URL.to_string(),
            ]
        );
    }

    #[test]
    fn musl_release_mirror_only_matches_snapshot_urls() {
        assert_eq!(
            musl_release_mirror_url(
                "https://git.musl-libc.org/cgit/musl/snapshot/musl-1.2.5.tar.gz"
            ),
            Some("https://musl.libc.org/releases/musl-1.2.5.tar.gz".to_string())
        );
        assert_eq!(
            musl_release_mirror_url("https://musl.libc.org/releases/musl-1.2.5.tar.gz"),
            None
        );
    }

    #[test]
    fn sourceforge_html_no_link_falls_back_to_download_suffix() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "<!doctype html><html><body>No direct link</body></html>"
        )
        .unwrap();
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
            Some(
                "https://downloads.sourceforge.net/project/zsh/zsh/5.9/zsh-5.9.tar.xz?download"
                    .to_string()
            )
        );
    }

    #[test]
    fn sourceforge_html_large_page_extracts_downloads_link_beyond_4k() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "<!doctype html><html><body>{}<a href='//downloads.sourceforge.net/project/tcl/tcl8.6.17-src.tar.gz?download'>download</a></body></html>",
            "x".repeat(9000)
        )
        .unwrap();
        let alt = validate_downloaded_archive(
            tmp.path(),
            "tcl8.6.17-src.tar.gz",
            "https://sourceforge.net/projects/tcl/files/tcl8.6.17-src.tar.gz",
        )
        .unwrap();
        assert_eq!(
            alt,
            Some(
                "https://downloads.sourceforge.net/project/tcl/tcl8.6.17-src.tar.gz?download"
                    .to_string()
            )
        );
    }

    #[test]
    fn sourceforge_html_ignores_social_share_links_and_unwraps_url_param() {
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            "<!doctype html><a href='https://x.com/share?url=https://sourceforge.net/projects/tcl/files/Tcl/8.6.17/tcl8.6.17-src.tar.gz/download&amp;text=share'>share</a>"
        )
        .unwrap();
        let alt = validate_downloaded_archive(
            tmp.path(),
            "tcl8.6.17-src.tar.gz",
            "https://sourceforge.net/projects/tcl/files/Tcl/8.6.17/tcl8.6.17-src.tar.gz",
        )
        .unwrap();
        assert_eq!(
            alt,
            Some(
                "https://sourceforge.net/projects/tcl/files/Tcl/8.6.17/tcl8.6.17-src.tar.gz/download"
                    .to_string()
            )
        );
    }

    #[test]
    fn sourceforge_share_url_fallback_uses_embedded_sourceforge_url() {
        let fallback = sourceforge_download_fallback(
            "https://x.com/share?url=https://sourceforge.net/projects/tcl/files/Tcl/8.6.17/tcl8.6.17-src.tar.gz",
        );
        assert_eq!(
            fallback,
            Some(
                "https://sourceforge.net/projects/tcl/files/Tcl/8.6.17/tcl8.6.17-src.tar.gz/download"
                    .to_string()
            )
        );
    }

    #[test]
    fn verify_checksum_accepts_sha1_md5_sha512_b2sum_and_default_sha256() {
        use sha1::Sha1;
        use sha2::Digest;
        use sha2::Sha256;
        use sha2::Sha512;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"abc").unwrap();

        // compute expected values using the same libraries
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

        // unprefixed should default to sha256
        assert!(verify_checksum(tmp.path(), &sha256_hex).unwrap());
        // explicit prefixes
        assert!(verify_checksum(tmp.path(), &format!("sha256:{}", sha256_hex)).unwrap());
        assert!(verify_checksum(tmp.path(), &format!("sha512:{}", sha512_hex)).unwrap());
        assert!(verify_checksum(tmp.path(), &format!("sha1:{}", sha1_hex)).unwrap());
        assert!(verify_checksum(tmp.path(), &format!("md5:{}", md5_hex)).unwrap());
        assert!(verify_checksum(tmp.path(), &format!("b2:{}", b2_hex)).unwrap());
        assert!(verify_checksum(tmp.path(), &format!("b2sum:{}", b2_hex)).unwrap());
        // empty algorithm before colon -> assume sha256
        assert!(verify_checksum(tmp.path(), &format!(":{}", sha256_hex)).unwrap());
        // negative: wrong value fails
        assert!(!verify_checksum(tmp.path(), "md5:deadbeef").unwrap());
    }
}
