//! Minisign-based detached signature helpers.

use anyhow::{Context, Result};
use minisign::{PublicKey, SecretKeyBox, SignatureBox};
use std::fs;
use std::path::{Path, PathBuf};

const PUBLIC_KEYS_DIR_REL: &str = "usr/share/depot/keys/public";
const SIGN_KEYS_DIR_REL: &str = "usr/share/depot/keys/sign";

#[derive(Debug, Clone, Default)]
pub struct KeyLocations {
    pub public_key: Option<PathBuf>,
    pub signing_key: Option<PathBuf>,
}

fn is_zst_file(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().ends_with(".zst"))
        .unwrap_or(false)
}

fn candidate_roots(rootfs: &Path, host_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    out.push(rootfs.to_path_buf());
    if rootfs != host_root {
        out.push(host_root.to_path_buf());
    }
    out
}

fn key_dir_candidates(rootfs: &Path, host_root: &Path, rel: &str) -> Vec<PathBuf> {
    candidate_roots(rootfs, host_root)
        .into_iter()
        .map(|root| root.join(rel))
        .collect()
}

fn is_public_key_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("pub"))
}

fn pick_key_file(dir: &Path, is_public: bool) -> Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    if !dir.is_dir() {
        anyhow::bail!("Key path is not a directory: {}", dir.display());
    }

    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("Failed to read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    if files.is_empty() {
        return Ok(None);
    }
    files.sort();

    let preferred = if is_public {
        ["depot.pub", "depot.minisign.pub", "minisign.pub"]
    } else {
        ["depot.key", "depot.minisign.key", "minisign.key"]
    };
    for name in preferred {
        if let Some(found) = files
            .iter()
            .find(|p| p.file_name().is_some_and(|n| n == name))
            .cloned()
        {
            return Ok(Some(found));
        }
    }

    let ext = if is_public { "pub" } else { "key" };
    let ext_matches: Vec<PathBuf> = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .cloned()
        .collect();
    if ext_matches.len() == 1 {
        return Ok(Some(ext_matches[0].clone()));
    }
    if files.len() == 1 {
        return Ok(Some(files[0].clone()));
    }

    anyhow::bail!(
        "Ambiguous {} key directory {} (multiple key files found)",
        if is_public { "public" } else { "signing" },
        dir.display()
    );
}

fn list_public_key_files_in_roots(rootfs: &Path, host_root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in key_dir_candidates(rootfs, host_root, PUBLIC_KEYS_DIR_REL) {
        if !dir.exists() {
            continue;
        }
        if !dir.is_dir() {
            anyhow::bail!("Key path is not a directory: {}", dir.display());
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .with_context(|| format!("Failed to read {}", dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_public_key_file(p))
            .collect();
        files.sort();
        out.extend(files);
    }
    Ok(out)
}

fn locate_keys_in_roots(rootfs: &Path, host_root: &Path) -> Result<KeyLocations> {
    let mut keys = KeyLocations::default();

    for dir in key_dir_candidates(rootfs, host_root, PUBLIC_KEYS_DIR_REL) {
        if let Some(path) = pick_key_file(&dir, true)? {
            keys.public_key = Some(path);
            break;
        }
    }
    for dir in key_dir_candidates(rootfs, host_root, SIGN_KEYS_DIR_REL) {
        if let Some(path) = pick_key_file(&dir, false)? {
            keys.signing_key = Some(path);
            break;
        }
    }
    Ok(keys)
}

/// Locate public/signing keys by checking both `rootfs` and the host `/`.
pub fn locate_keys(rootfs: &Path) -> Result<KeyLocations> {
    locate_keys_in_roots(rootfs, Path::new("/"))
}

/// Return the directory where trusted minisign public keys are stored under `rootfs`.
pub fn trusted_public_keys_dir(rootfs: &Path) -> PathBuf {
    rootfs.join(PUBLIC_KEYS_DIR_REL)
}

/// List all trusted minisign public keys found in `rootfs` and then the host `/`.
pub fn list_trusted_public_keys(rootfs: &Path) -> Result<Vec<PathBuf>> {
    list_public_key_files_in_roots(rootfs, Path::new("/"))
}

fn detached_sig_path(input: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sig", input.display()))
}

fn sign_detached_with_key_paths(input: &Path, sig_path: &Path, keys: &KeyLocations) -> Result<()> {
    if !input.exists() {
        anyhow::bail!("File not found: {}", input.display());
    }
    if !is_zst_file(input) {
        anyhow::bail!(
            "Signing command currently only supports .zst files: {}",
            input.display()
        );
    }

    let signing_key_path = keys.signing_key.as_ref().with_context(
        || "No minisign signing key found in /usr/share/depot/keys/sign (checked rootfs and host)",
    )?;

    let secret_key_text = fs::read_to_string(signing_key_path).with_context(|| {
        format!(
            "Failed to read minisign signing key: {}",
            signing_key_path.display()
        )
    })?;
    let secret_key_box = SecretKeyBox::from_string(&secret_key_text).with_context(|| {
        format!(
            "Invalid minisign signing key file: {}",
            signing_key_path.display()
        )
    })?;
    let secret_key = match secret_key_box.clone().into_unencrypted_secret_key() {
        Ok(sk) => sk,
        Err(_) => secret_key_box.into_secret_key(None).with_context(|| {
            format!(
                "Failed to load minisign signing key: {}",
                signing_key_path.display()
            )
        })?,
    };
    let public_key = if let Some(path) = &keys.public_key {
        Some(
            PublicKey::from_file(path).with_context(|| {
                format!("Failed to load minisign public key: {}", path.display())
            })?,
        )
    } else {
        crate::log_warn!(
            "No minisign public key found in /usr/share/depot/keys/public (checked rootfs and host)"
        );
        None
    };

    let file =
        fs::File::open(input).with_context(|| format!("Failed to open {}", input.display()))?;
    let sig = minisign::sign(
        public_key.as_ref(),
        &secret_key,
        file,
        None,
        Some(&format!("depot signature for {}", input.display())),
    )
    .with_context(|| format!("Failed to sign {}", input.display()))?;

    fs::write(sig_path, sig.to_bytes())
        .with_context(|| format!("Failed to write detached signature {}", sig_path.display()))?;
    Ok(())
}

fn verify_detached_with_key_paths(
    input: &Path,
    sig_path: &Path,
    keys: &KeyLocations,
) -> Result<()> {
    if !input.exists() {
        anyhow::bail!("File not found: {}", input.display());
    }
    if !sig_path.exists() {
        anyhow::bail!("Detached signature not found: {}", sig_path.display());
    }
    if !is_zst_file(input) {
        anyhow::bail!(
            "Verification currently only supports .zst files: {}",
            input.display()
        );
    }

    let public_key_path = keys.public_key.as_ref().with_context(
        || "No minisign public key found in /usr/share/depot/keys/public (checked rootfs and host)",
    )?;
    let public_key = PublicKey::from_file(public_key_path).with_context(|| {
        format!(
            "Failed to load minisign public key: {}",
            public_key_path.display()
        )
    })?;
    let sig = SignatureBox::from_file(sig_path)
        .with_context(|| format!("Failed to load detached signature: {}", sig_path.display()))?;
    let mut file =
        fs::File::open(input).with_context(|| format!("Failed to open {}", input.display()))?;
    minisign::verify(&public_key, &sig, &mut file, true, false, false).with_context(|| {
        format!(
            "Detached signature verification failed for {}",
            input.display()
        )
    })?;
    Ok(())
}

/// Verify a `.zst` file using a detached minisign signature and an explicit public key path.
pub fn verify_zst_file_detached_with_public_key(
    input: &Path,
    sig_path: &Path,
    public_key_path: &Path,
) -> Result<()> {
    let keys = KeyLocations {
        public_key: Some(public_key_path.to_path_buf()),
        signing_key: None,
    };
    verify_detached_with_key_paths(input, sig_path, &keys)
}

/// Sign a `.zst` file with a detached minisign signature written to `<file>.sig`.
pub fn sign_zst_file_detached(rootfs: &Path, input: &Path) -> Result<PathBuf> {
    let keys = locate_keys(rootfs)?;
    let sig_path = detached_sig_path(input);
    sign_detached_with_key_paths(input, &sig_path, &keys)?;
    Ok(sig_path)
}

/// Attempt to sign a `.zst` file using discovered keys; skip if no signing key exists.
pub fn auto_sign_zst_file_detached(rootfs: &Path, input: &Path) -> Result<Option<PathBuf>> {
    if !is_zst_file(input) {
        return Ok(None);
    }
    let keys = locate_keys(rootfs)?;
    if keys.signing_key.is_none() {
        crate::log_info!(
            "No minisign signing key found; skipping detached signature for {}",
            input.display()
        );
        return Ok(None);
    }
    let sig_path = detached_sig_path(input);
    sign_detached_with_key_paths(input, &sig_path, &keys)?;
    Ok(Some(sig_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use minisign::KeyPair;
    use std::io::Write;

    fn write_test_keys(root: &Path) -> Result<(PathBuf, PathBuf)> {
        let public_dir = root.join(PUBLIC_KEYS_DIR_REL);
        let sign_dir = root.join(SIGN_KEYS_DIR_REL);
        fs::create_dir_all(&public_dir)?;
        fs::create_dir_all(&sign_dir)?;

        let pair = KeyPair::generate_unencrypted_keypair().context("Failed to generate keypair")?;
        let pub_path = public_dir.join("depot.pub");
        let sign_path = sign_dir.join("depot.key");
        fs::write(&pub_path, pair.pk.to_box()?.to_bytes())?;
        fs::write(&sign_path, pair.sk.to_box(Some("test"))?.to_bytes())?;
        Ok((pub_path, sign_path))
    }

    #[test]
    fn locate_keys_checks_rootfs_before_host() -> Result<()> {
        let rootfs = tempfile::tempdir()?;
        let host = tempfile::tempdir()?;
        let (rootfs_pub, rootfs_sign) = write_test_keys(rootfs.path())?;
        let _ = write_test_keys(host.path())?;

        let keys = locate_keys_in_roots(rootfs.path(), host.path())?;
        assert_eq!(keys.public_key, Some(rootfs_pub));
        assert_eq!(keys.signing_key, Some(rootfs_sign));
        Ok(())
    }

    #[test]
    fn sign_zst_file_writes_detached_signature() -> Result<()> {
        let rootfs = tempfile::tempdir()?;
        let host = tempfile::tempdir()?;
        let (pub_path, sign_path) = write_test_keys(rootfs.path())?;

        let file = rootfs.path().join("artifact.tar.zst");
        let mut f = fs::File::create(&file)?;
        f.write_all(b"test payload")?;
        f.flush()?;

        let keys = KeyLocations {
            public_key: Some(pub_path.clone()),
            signing_key: Some(sign_path.clone()),
        };
        let sig_path = PathBuf::from(format!("{}.sig", file.display()));
        sign_detached_with_key_paths(&file, &sig_path, &keys)?;
        assert!(sig_path.exists());

        let pk = PublicKey::from_file(&pub_path)?;
        let sig_box = SignatureBox::from_file(&sig_path)?;
        let mut reader = fs::File::open(&file)?;
        minisign::verify(&pk, &sig_box, &mut reader, true, false, false)
            .context("signature verification should succeed")?;
        verify_detached_with_key_paths(&file, &sig_path, &keys)?;
        verify_zst_file_detached_with_public_key(&file, &sig_path, &pub_path)?;

        // Also make sure host/rootfs lookup path version works without touching /.
        let _ = locate_keys_in_roots(rootfs.path(), host.path())?;
        Ok(())
    }

    #[test]
    fn list_trusted_public_keys_finds_multiple_pub_files() -> Result<()> {
        let rootfs = tempfile::tempdir()?;
        let host = tempfile::tempdir()?;
        let public_dir = rootfs.path().join(PUBLIC_KEYS_DIR_REL);
        fs::create_dir_all(&public_dir)?;
        fs::write(public_dir.join("a.pub"), b"dummy")?;
        fs::write(public_dir.join("note.txt"), b"ignore")?;
        fs::write(public_dir.join("b.pub"), b"dummy")?;

        let found = list_public_key_files_in_roots(rootfs.path(), host.path())?;
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("a.pub"));
        assert!(found[1].ends_with("b.pub"));
        Ok(())
    }
}
