//! Source fetching and extraction

mod extractor;
mod fetcher;
mod git;
pub mod hooks;

use crate::package::PackageSpec;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use url::Url;
use walkdir::WalkDir;

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
        let mut local_entries: Vec<String> = Vec::new();
        if let Some(file) = manual.file.as_ref() {
            if !file.trim().is_empty() {
                local_entries.push(file.clone());
            }
        }
        local_entries.extend(
            manual
                .files
                .iter()
                .filter(|s| !s.trim().is_empty())
                .cloned(),
        );

        let mut url_entries: Vec<String> = Vec::new();
        if let Some(url) = manual.url.as_ref() {
            if !url.trim().is_empty() {
                url_entries.push(url.clone());
            }
        }
        url_entries.extend(manual.urls.iter().filter(|s| !s.trim().is_empty()).cloned());

        if !local_entries.is_empty() {
            for raw_file in local_entries {
                let file = spec.expand_vars(&raw_file);
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
                let expanded_url = spec.expand_vars(&raw_url);
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
    let dest_name = if let Some(dest) = manual.dest.as_ref() {
        spec.expand_vars(dest)
    } else {
        default_dest.to_string()
    };
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

/// Verify a file against an `expected` checksum string.
///
/// Formats accepted: `sha256:HEX`, `sha512:HEX`, `md5:HEX`, or plain `HEX` (assumed sha256).
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
            let actual = format!("{:x}", hasher.finalize());
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
            let actual = format!("{:x}", hasher.finalize());
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
            let actual = format!("{:x}", digest);
            Ok(actual == hex)
        }
        _ => bail!("Unsupported checksum algorithm: {}", alg),
    }
}

/// Build a blocking reqwest HTTP client using the platform-default TLS backend.
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

    // Heuristic: if the URL contains '#', treat it as a git URL with a revision.
    // (except when it clearly looks like an archive URL)
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
        )?;
        hooks::post_extract(spec, source, &checkout_dir, cache_dir)?;
        return Ok(checkout_dir);
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
    use super::*;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, ManualSource, PackageInfo,
        PackageSpec, Source,
    };

    fn mk_spec_with_manuals(spec_dir: PathBuf, manuals: Vec<ManualSource>) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
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
                version: "3.11.3".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
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
    fn verify_file_hash_accepts_multiple_algorithms() {
        use sha2::{Digest, Sha256, Sha512};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"abc").unwrap();

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

        assert!(verify_file_hash(tmp.path(), &sha256_hex).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("sha256:{}", sha256_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("sha512:{}", sha512_hex)).unwrap());
        assert!(verify_file_hash(tmp.path(), &format!("md5:{}", md5_hex)).unwrap());
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
}
