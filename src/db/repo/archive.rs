use super::*;

pub(super) fn verify_binary_package_record_checksums(
    path: &Path,
    rec: &BinaryRepoPackageRecord,
) -> Result<()> {
    let expected = expected_binary_package_sha512(path, rec)?;

    let mut file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut hasher = Sha512::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    verify_binary_package_sha512_digest(
        path,
        rec,
        &expected,
        &crate::hex::encode_lower(hasher.finalize()),
    )
}

pub(super) fn expected_binary_package_sha512(
    path: &Path,
    rec: &BinaryRepoPackageRecord,
) -> Result<String> {
    let expected = rec.sha512.trim().to_ascii_lowercase();
    if expected.is_empty() {
        anyhow::bail!(
            "Missing SHA-512 checksum for {} from repo '{}'",
            path.display(),
            rec.repo_name
        );
    }
    Ok(expected)
}

pub(super) fn verify_binary_package_sha512_digest(
    path: &Path,
    rec: &BinaryRepoPackageRecord,
    expected: &str,
    actual: &str,
) -> Result<()> {
    if actual != expected {
        anyhow::bail!(
            "SHA-512 mismatch for {} from repo '{}'",
            path.display(),
            rec.repo_name
        );
    }
    Ok(())
}

pub(super) struct Sha512Reader<R> {
    inner: R,
    hasher: Sha512,
}

impl<R> Sha512Reader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha512::new(),
        }
    }

    fn finalize_hex(self) -> String {
        crate::hex::encode_lower(self.hasher.finalize())
    }
}

impl<R: Read> Read for Sha512Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            self.hasher.update(&buf[..read]);
        }
        Ok(read)
    }
}

impl<R: Seek> Seek for Sha512Reader<R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        if position != SeekFrom::Start(0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "checksum reader only supports rewinding to the start",
            ));
        }
        let position = self.inner.seek(position)?;
        self.hasher = Sha512::new();
        Ok(position)
    }
}

pub(super) fn download_binary_package_archive(
    client: &reqwest::blocking::Client,
    pkg_url: &str,
    tmp_path: &Path,
    progress_cb: &mut Option<&mut dyn FnMut(u64, Option<u64>)>,
) -> Result<()> {
    match copy_file_url_to_path(pkg_url, tmp_path)? {
        FileUrlCopyOutcome::Copied => {
            if let Some(cb) = progress_cb.as_mut() {
                let total = fs::metadata(tmp_path).map(|m| m.len()).unwrap_or(0);
                cb(total, Some(total));
            }
        }
        FileUrlCopyOutcome::Missing => {
            anyhow::bail!("Failed to fetch {}: local file not found", pkg_url);
        }
        FileUrlCopyOutcome::NotFileUrl => {
            let mut resp = client
                .get(pkg_url)
                .send()
                .with_context(|| format!("Failed to fetch {}", pkg_url))?;
            if !resp.status().is_success() {
                anyhow::bail!("Failed to fetch {}: HTTP {}", pkg_url, resp.status());
            }

            let total = resp.content_length();
            if let Some(cb) = progress_cb.as_mut() {
                cb(0, total);
            }

            let mut out = fs::File::create(tmp_path)
                .with_context(|| format!("Failed to create {}", tmp_path.display()))?;
            let mut downloaded = 0u64;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = resp
                    .read(&mut buf)
                    .with_context(|| format!("Failed to read {}", pkg_url))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .with_context(|| format!("Failed to save {}", tmp_path.display()))?;
                downloaded = downloaded.saturating_add(n as u64);
                if let Some(cb) = progress_cb.as_mut() {
                    cb(downloaded, total);
                }
            }
            out.flush()
                .with_context(|| format!("Failed to flush {}", tmp_path.display()))?;
        }
    }
    Ok(())
}

pub(super) fn fetch_binary_package_signature(
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

pub(super) fn verify_binary_package_signature_with_trusted_keys(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    pkg_path: &Path,
    sig_path: &Path,
    trusted_keys: &[crate::signing::TrustedPublicKey],
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

    let _verified_key = crate::signing::verify_zst_file_detached_with_trusted_keys(
        pkg_path,
        sig_path,
        trusted_keys,
    )
    .with_context(|| {
        format!(
            "Failed to verify detached package signature for {}",
            pkg_path.display()
        )
    })?;
    Ok(())
}

/// Ensure a binary package archive and detached signature are present in cache
/// without performing checksum/signature verification.
pub fn cache_binary_package_archive(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rec: &BinaryRepoPackageRecord,
    package_cache_dir: &Path,
) -> Result<BinaryRepoCachedArchive> {
    cache_binary_package_archive_with_progress(repo_name, repo, rec, package_cache_dir, None)
}

/// Ensure a binary package archive and detached signature are present in cache
/// without performing checksum/signature verification, optionally reporting
/// download progress.
pub fn cache_binary_package_archive_with_progress(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rec: &BinaryRepoPackageRecord,
    package_cache_dir: &Path,
    progress_cb: Option<&mut dyn FnMut(u64, Option<u64>)>,
) -> Result<BinaryRepoCachedArchive> {
    let client = binary_package_http_client()?;
    cache_binary_package_archive_with_client_and_progress(
        repo_name,
        repo,
        rec,
        package_cache_dir,
        &client,
        progress_cb,
    )
}

pub(crate) fn binary_package_http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .build()
        .context("Failed to build HTTP client for binary package fetch")
}

pub(crate) fn cache_binary_package_archive_with_client_and_progress(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rec: &BinaryRepoPackageRecord,
    package_cache_dir: &Path,
    client: &reqwest::blocking::Client,
    mut progress_cb: Option<&mut dyn FnMut(u64, Option<u64>)>,
) -> Result<BinaryRepoCachedArchive> {
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
    let package_path = cache_dir.join(&rec.filename);
    let signature_path = cache_dir.join(format!("{}.sig", rec.filename));
    let tmp_path = cache_dir.join(format!("{}.tmp", rec.filename));
    let tmp_sig_path = cache_dir.join(format!("{}.sig.tmp", rec.filename));
    let pkg_url = join_repo_url(base_url, &rec.filename)?;
    let sig_url = join_repo_url(base_url, &format!("{}.sig", rec.filename))?;

    let package_downloaded = if !package_path.exists() {
        download_binary_package_archive(client, &pkg_url, &tmp_path, &mut progress_cb)?;
        fs::rename(&tmp_path, &package_path).with_context(|| {
            format!(
                "Failed to move {} to {}",
                tmp_path.display(),
                package_path.display()
            )
        })?;
        true
    } else {
        if let Some(cb) = progress_cb.as_mut() {
            let total = fs::metadata(&package_path)
                .with_context(|| format!("Failed to stat {}", package_path.display()))?
                .len();
            cb(total, Some(total));
        }
        false
    };

    if package_downloaded || !signature_path.exists() {
        let sig_downloaded =
            fetch_binary_package_signature(repo_name, repo, client, &sig_url, &tmp_sig_path)?;
        if sig_downloaded {
            fs::rename(&tmp_sig_path, &signature_path).with_context(|| {
                format!(
                    "Failed to move {} to {}",
                    tmp_sig_path.display(),
                    signature_path.display()
                )
            })?;
        } else {
            let _ = fs::remove_file(&signature_path);
        }
    }

    Ok(BinaryRepoCachedArchive {
        package_path,
        signature_path,
    })
}

/// Verify a cached/downloaded package archive against checksums from signed
/// repository metadata.
pub fn verify_binary_package_archive_checksums(
    archive_path: &Path,
    rec: &BinaryRepoPackageRecord,
) -> Result<()> {
    verify_binary_package_record_checksums(archive_path, rec)
}

pub(crate) fn verify_binary_package_archive_integrity_with_trusted_keys(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    record: &BinaryRepoPackageRecord,
    package_path: &Path,
    signature_path: &Path,
    trusted_keys: &[crate::signing::TrustedPublicKey],
) -> Result<()> {
    if signature_path.exists() && !trusted_keys.is_empty() {
        let expected = expected_binary_package_sha512(package_path, record)?;
        let file = fs::File::open(package_path)
            .with_context(|| format!("Failed to open {}", package_path.display()))?;
        let mut reader = Sha512Reader::new(file);
        let _verified_key = crate::signing::verify_reader_detached_with_trusted_keys(
            &mut reader,
            package_path,
            signature_path,
            trusted_keys,
        )
        .with_context(|| {
            format!(
                "Failed to verify detached package signature for {}",
                package_path.display()
            )
        })?;
        let actual = reader.finalize_hex();
        verify_binary_package_sha512_digest(package_path, record, &expected, &actual)?;
        return Ok(());
    }

    verify_binary_package_archive_checksums(package_path, record)?;
    verify_binary_package_signature_with_trusted_keys(
        repo_name,
        repo,
        package_path,
        signature_path,
        trusted_keys,
    )
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
    let cached = cache_binary_package_archive(repo_name, repo, rec, package_cache_dir)?;
    let trusted_keys = if cached.signature_path.exists() {
        crate::signing::load_trusted_public_keys(rootfs)?
    } else {
        Vec::new()
    };
    verify_binary_package_archive_integrity_with_trusted_keys(
        repo_name,
        repo,
        rec,
        &cached.package_path,
        &cached.signature_path,
        &trusted_keys,
    )
    .with_context(|| {
        format!(
            "Binary package failed integrity verification: {}",
            cached.package_path.display()
        )
    })?;
    Ok(cached.package_path)
}
