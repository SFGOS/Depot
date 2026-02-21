//! Staging phase - file collection and cleanup

use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

fn is_skipped_install_path(rel_path: &str) -> bool {
    let p = rel_path.trim_start_matches('/');
    p == ".metadata.toml"
        || p == ".files.yaml"
        || p == "scripts"
        || p.starts_with("scripts/")
        || p == "usr/share/info/dir"
        || p.starts_with("usr/share/info/dir.")
}

fn normalize_relative_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        anyhow::bail!("keep paths must not be empty");
    }

    let p = Path::new(trimmed);
    if p.is_absolute() {
        anyhow::bail!("keep paths must be relative: {}", trimmed);
    }

    let mut normalized = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(seg) => normalized.push(seg),
            Component::CurDir => {}
            _ => {
                anyhow::bail!(
                    "keep paths must not contain traversal or root components: {}",
                    trimmed
                );
            }
        }
    }

    let s = normalized
        .to_str()
        .context("keep paths must be valid UTF-8")?
        .to_string();

    if s.is_empty() {
        anyhow::bail!("keep paths must not resolve to empty paths: {}", trimmed);
    }

    Ok(s)
}

/// Process staged files - remove .la files, strip binaries, etc.
pub fn process(destdir: &Path, _spec: &PackageSpec) -> Result<()> {
    println!("Processing staged files...");

    let mut removed_count = 0;

    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();

        // Remove libtool .la files
        if path.extension().map(|e| e == "la").unwrap_or(false) {
            println!("  Removing: {}", path.display());
            fs::remove_file(path)?;
            removed_count += 1;
        }
    }

    if removed_count > 0 {
        println!("Removed {} .la file(s)", removed_count);
    }

    Ok(())
}

/// Copy license files into the staged tree.
///
/// Copies common license file patterns from the source directory root into:
/// `destdir/documentation/licenses/<pkgname>/...`
pub fn add_licenses(src_dir: &Path, destdir: &Path, pkgname: &str) -> Result<usize> {
    let mut copied = 0usize;
    let dst_base = destdir.join("usr/share/licenses").join(pkgname);

    // Common patterns requested: LICENSE*, COPYING*
    // (plus a couple of very common variants)
    let is_license_name = |name: &str| {
        let upper = name.to_ascii_uppercase();
        upper.starts_with("LICENSE")
            || upper.starts_with("COPYING")
            || upper.starts_with("LICENCE")
            || upper.starts_with("NOTICE")
    };

    let entries = match fs::read_dir(src_dir) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_license_name(name) {
            continue;
        }

        fs::create_dir_all(&dst_base)
            .with_context(|| format!("Failed to create license dir: {}", dst_base.display()))?;

        let dst = dst_base.join(name);
        fs::copy(&path, &dst)
            .with_context(|| format!("Failed to copy license {}", path.display()))?;
        copied += 1;
    }

    if copied > 0 {
        println!(
            "Copied {} license file(s) to {}/",
            copied,
            dst_base.display()
        );
    }

    Ok(copied)
}

#[derive(Debug)]
pub struct FsTransaction {
    rootfs: PathBuf,
    tx_dir: PathBuf,
    backed_up: Vec<String>,
    created: Vec<String>,
    removed: Vec<String>,
}

impl FsTransaction {
    fn backup_path(&self, rel: &str) -> PathBuf {
        self.tx_dir.join("backup").join(rel)
    }

    fn removed_backup_path(&self, rel: &str) -> PathBuf {
        self.tx_dir.join("removed").join(rel)
    }

    /// Roll back file operations performed by `install_atomic`.
    pub fn rollback(&self) -> Result<()> {
        // Restore removed files
        for rel in &self.removed {
            let src = self.removed_backup_path(rel);
            let dst = self.rootfs.join(rel);
            if src.symlink_metadata().is_ok() {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                // Best-effort remove if something exists now.
                let _ = fs::remove_file(&dst);
                match fs::rename(&src, &dst) {
                    Ok(()) => {}
                    Err(_) => {
                        fs::copy(&src, &dst)?;
                        fs::remove_file(&src)?;
                    }
                }
            }
        }

        // Restore overwritten files
        for rel in &self.backed_up {
            let src = self.backup_path(rel);
            let dst = self.rootfs.join(rel);
            if src.symlink_metadata().is_ok() {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                let _ = fs::remove_file(&dst);
                match fs::rename(&src, &dst) {
                    Ok(()) => {}
                    Err(_) => {
                        fs::copy(&src, &dst)?;
                        fs::remove_file(&src)?;
                    }
                }
            }
        }

        // Remove files that were newly created
        for rel in &self.created {
            let dst = self.rootfs.join(rel);
            let _ = fs::remove_file(dst);
        }

        Ok(())
    }

    /// Commit the transaction (delete backup directory).
    pub fn commit(self) -> Result<()> {
        if self.tx_dir.exists() {
            fs::remove_dir_all(&self.tx_dir)?;
        }
        Ok(())
    }
}

/// Install staged files using a rollback-capable transaction.
///
/// This is used for both first-time installs and updates. For updates, pass a
/// list of relative paths to remove (old manifest minus new manifest).
pub fn install_atomic(
    destdir: &Path,
    rootfs: &Path,
    tx_base_dir: &Path,
    remove_paths: &[String],
    keep_paths: &[String],
) -> Result<FsTransaction> {
    let keep_set: HashSet<String> = keep_paths
        .iter()
        .map(|p| normalize_relative_path(p))
        .collect::<Result<HashSet<_>>>()?;

    fs::create_dir_all(tx_base_dir)
        .with_context(|| format!("Failed to create tx dir: {}", tx_base_dir.display()))?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let tx_dir = tx_base_dir.join(format!("tx-{}-{}", ts, pid));
    let backup_dir = tx_dir.join("backup");
    let removed_dir = tx_dir.join("removed");
    fs::create_dir_all(&backup_dir)?;
    fs::create_dir_all(&removed_dir)?;

    let mut tx = FsTransaction {
        rootfs: rootfs.to_path_buf(),
        tx_dir,
        backed_up: Vec::new(),
        created: Vec::new(),
        removed: Vec::new(),
    };

    let result: Result<()> = (|| {
        // First, create all directories from destdir (for packages with only directories)
        for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
            let src_path = entry.path();
            let file_type = entry.file_type();

            if !file_type.is_dir() || src_path == destdir {
                continue;
            }

            let rel_path = src_path
                .strip_prefix(destdir)
                .context("Failed to strip destdir prefix")?;

            let dest_path = rootfs.join(rel_path);
            if !dest_path.exists() {
                fs::create_dir_all(&dest_path)?;
            }
        }

        // Copy in new files.
        for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
            let src_path = entry.path();
            let metadata = src_path
                .symlink_metadata()
                .context("Failed to get metadata")?;
            let file_type = metadata.file_type();

            // We want to install files AND symlinks (to anything)
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }

            let rel_path = src_path
                .strip_prefix(destdir)
                .context("Failed to strip destdir prefix")?
                .to_string_lossy()
                .to_string();

            if is_skipped_install_path(&rel_path) {
                continue;
            }

            let keep_as_depotnew = keep_set.contains(&rel_path) && rootfs.join(&rel_path).exists();
            let install_rel_path = if keep_as_depotnew {
                format!("{}.depotnew", rel_path)
            } else {
                rel_path.clone()
            };

            let dest_path = rootfs.join(&install_rel_path);

            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }

            if dest_path.symlink_metadata().is_ok() {
                // lexists checks existence without following symlinks
                // Backup existing
                let backup_path = tx.backup_path(&install_rel_path);
                if let Some(parent) = backup_path.parent() {
                    fs::create_dir_all(parent)?;
                }

                // Fallback: if symlink, read link and recreate at backup.
                let dest_meta = dest_path.symlink_metadata()?;
                if dest_meta.file_type().is_symlink() {
                    let target = fs::read_link(&dest_path)?;
                    std::os::unix::fs::symlink(&target, &backup_path)?;
                } else {
                    fs::copy(&dest_path, &backup_path)?;
                }

                tx.backed_up.push(install_rel_path.clone());
            } else {
                tx.created.push(install_rel_path.clone());
            }

            // Install new file/symlink
            // Remove destination if it exists (we backed it up) so we can overwrite
            if dest_path.symlink_metadata().is_ok() {
                if dest_path.is_dir() {
                    fs::remove_dir_all(&dest_path)?;
                } else {
                    fs::remove_file(&dest_path)?;
                }
            }

            if file_type.is_symlink() {
                let target = fs::read_link(src_path)?;
                std::os::unix::fs::symlink(target, &dest_path)
                    .with_context(|| format!("Failed to create symlink: {}", install_rel_path))?;
            } else {
                fs::copy(src_path, &dest_path)
                    .with_context(|| format!("Failed to install: {}", install_rel_path))?;
            }
        }

        // Remove obsolete files (update only)
        for rel in remove_paths {
            let rel = rel.as_str();
            let dest_path = rootfs.join(rel);
            let dest_meta = match dest_path.symlink_metadata() {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to inspect obsolete path before removal: {}", rel)
                    });
                }
            };

            if dest_meta.file_type().is_dir() {
                // Only obsolete files/symlinks are removed here.
                continue;
            }

            let backup_path = tx.removed_backup_path(rel);
            if let Some(parent) = backup_path.parent() {
                fs::create_dir_all(parent)?;
            }

            if dest_meta.file_type().is_symlink() {
                let target = fs::read_link(&dest_path)
                    .with_context(|| format!("Failed to read obsolete symlink target: {}", rel))?;
                std::os::unix::fs::symlink(&target, &backup_path)
                    .with_context(|| format!("Failed to backup removed symlink: {}", rel))?;
            } else {
                fs::copy(&dest_path, &backup_path)
                    .with_context(|| format!("Failed to backup removed file: {}", rel))?;
            }

            fs::remove_file(&dest_path)
                .with_context(|| format!("Failed to remove obsolete file/symlink: {}", rel))?;
            tx.removed.push(rel.to_string());
        }

        Ok(())
    })();

    if let Err(e) = result {
        let _ = tx.rollback();
        return Err(e);
    }

    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_licenses_copies_common_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();

        std::fs::write(src_dir.join("LICENSE"), "license text").unwrap();
        std::fs::write(src_dir.join("COPYING.md"), "copying text").unwrap();
        std::fs::write(src_dir.join("README"), "not a license").unwrap();

        let copied = add_licenses(&src_dir, &destdir, "foo").unwrap();
        assert_eq!(copied, 2);

        let lic_dir = destdir.join("usr/share/licenses/foo");
        assert!(lic_dir.join("LICENSE").exists());
        assert!(lic_dir.join("COPYING.md").exists());
        assert!(!lic_dir.join("README").exists());
    }

    #[test]
    fn install_atomic_update_and_rollback_restores_state() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();

        // Existing installed files
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::fs::write(rootfs.join("usr/bin/foo"), "old").unwrap();
        std::fs::write(rootfs.join("usr/bin/old_only"), "to_remove").unwrap();

        // New staged files
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join("usr/bin/foo"), "new").unwrap();
        std::fs::write(destdir.join("usr/bin/new_only"), "added").unwrap();

        let remove_paths = vec!["usr/bin/old_only".to_string()];
        let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

        // After install: updated + new present, obsolete removed
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/foo")).unwrap(),
            "new"
        );
        assert!(rootfs.join("usr/bin/new_only").exists());
        assert!(!rootfs.join("usr/bin/old_only").exists());

        // Roll back should restore old state
        tx.rollback().unwrap();
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/foo")).unwrap(),
            "old"
        );
        assert!(!rootfs.join("usr/bin/new_only").exists());
        assert!(rootfs.join("usr/bin/old_only").exists());
    }

    #[test]
    fn install_atomic_keep_existing_installs_depotnew() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::create_dir_all(destdir.join("etc")).unwrap();

        std::fs::write(rootfs.join("etc/locale.gen"), "existing").unwrap();
        std::fs::write(destdir.join("etc/locale.gen"), "from-package").unwrap();

        let keep = vec!["etc/locale.gen".to_string()];
        let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &keep).unwrap();

        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/locale.gen")).unwrap(),
            "existing"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/locale.gen.depotnew")).unwrap(),
            "from-package"
        );

        tx.rollback().unwrap();
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/locale.gen")).unwrap(),
            "existing"
        );
        assert!(!rootfs.join("etc/locale.gen.depotnew").exists());
    }

    #[test]
    fn install_atomic_rejects_unsafe_keep_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(destdir.join("etc")).unwrap();
        std::fs::write(destdir.join("etc/locale.gen"), "x").unwrap();

        let keep = vec!["../etc/shadow".to_string()];
        let err = install_atomic(&destdir, &rootfs, &tx_base, &[], &keep)
            .expect_err("expected keep path traversal to be rejected");
        assert!(
            err.to_string()
                .contains("keep paths must not contain traversal")
        );
    }

    #[test]
    fn install_atomic_removes_obsolete_symlink_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join("usr/bin/new"), "ok").unwrap();

        std::os::unix::fs::symlink("../lib/libold.so", rootfs.join("usr/lib/libold.so.link"))
            .unwrap();
        assert!(
            rootfs
                .join("usr/lib/libold.so.link")
                .symlink_metadata()
                .is_ok()
        );

        let remove_paths = vec!["usr/lib/libold.so.link".to_string()];
        let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

        assert!(
            rootfs
                .join("usr/lib/libold.so.link")
                .symlink_metadata()
                .is_err()
        );

        tx.rollback().unwrap();
        let restored = rootfs
            .join("usr/lib/libold.so.link")
            .symlink_metadata()
            .expect("symlink should be restored");
        assert!(restored.file_type().is_symlink());
    }

    #[test]
    fn install_atomic_commit_removes_tx_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join("usr/bin/foo"), "x").unwrap();

        let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();
        let tx_dir = tx.tx_dir.clone();
        assert!(tx_dir.exists());
        tx.commit().unwrap();
        assert!(!tx_dir.exists());
    }

    #[test]
    fn test_install_atomic_symlink_to_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        // Create a symlink bin -> usr/bin in destdir
        std::os::unix::fs::symlink("usr/bin", destdir.join("bin")).unwrap();

        let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        // Verify rootfs/bin is a symlink, not a directory
        let meta = rootfs
            .join("bin")
            .symlink_metadata()
            .expect("bin should exist");
        assert!(meta.file_type().is_symlink(), "bin should be a symlink");
        assert_eq!(
            std::fs::read_link(rootfs.join("bin")).unwrap(),
            std::path::PathBuf::from("usr/bin")
        );
    }

    #[test]
    fn test_walkdir_symlink_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::os::unix::fs::symlink("target", dir.join("link")).unwrap();

        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            if entry.path().ends_with("link") {
                let ft = entry.file_type();
                assert!(
                    !ft.is_dir(),
                    "walkdir should NOT report symlink to dir as a directory"
                );
                assert!(ft.is_symlink(), "walkdir SHOULD report it as a symlink");
            }
        }
    }

    #[test]
    fn install_atomic_skips_info_dir_index() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(destdir.join("usr/share/info")).unwrap();
        std::fs::write(destdir.join("usr/share/info/dir"), "index").unwrap();
        std::fs::write(destdir.join("usr/share/info/dir.gz"), "index gz").unwrap();
        std::fs::write(destdir.join("usr/share/info/ok.info"), "ok").unwrap();

        let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        assert!(!rootfs.join("usr/share/info/dir").exists());
        assert!(!rootfs.join("usr/share/info/dir.gz").exists());
        assert!(rootfs.join("usr/share/info/ok.info").exists());
    }

    #[test]
    fn install_atomic_skips_package_metadata_files() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join(".metadata.toml"), "name='foo'").unwrap();
        std::fs::write(destdir.join(".files.yaml"), "files: []").unwrap();
        std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

        let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        assert!(!rootfs.join(".metadata.toml").exists());
        assert!(!rootfs.join(".files.yaml").exists());
        assert!(rootfs.join("usr/bin/ok").exists());
    }

    #[test]
    fn install_atomic_skips_package_scripts_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(destdir.join("scripts")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join("scripts/pre_install"), "#!/bin/sh\necho pre\n").unwrap();
        std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

        let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        assert!(!rootfs.join("scripts/pre_install").exists());
        assert!(rootfs.join("usr/bin/ok").exists());
    }

    #[test]
    fn generate_manifest_skips_info_dir_index() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/share/info")).unwrap();
        std::fs::write(destdir.join("usr/share/info/dir"), "index").unwrap();
        std::fs::write(destdir.join("usr/share/info/dir.xz"), "index xz").unwrap();
        std::fs::write(destdir.join("usr/share/info/ok.info"), "ok").unwrap();

        let manifest = generate_manifest_with_dirs(&destdir).unwrap();

        assert!(!manifest.files.contains(&"usr/share/info/dir".to_string()));
        assert!(
            !manifest
                .files
                .contains(&"usr/share/info/dir.xz".to_string())
        );
        assert!(
            manifest
                .files
                .contains(&"usr/share/info/ok.info".to_string())
        );
    }

    #[test]
    fn generate_manifest_skips_package_metadata_files() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&destdir).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join(".metadata.toml"), "name='foo'").unwrap();
        std::fs::write(destdir.join(".files.yaml"), "files: []").unwrap();
        std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

        let manifest = generate_manifest_with_dirs(&destdir).unwrap();

        assert!(!manifest.files.contains(&".metadata.toml".to_string()));
        assert!(!manifest.files.contains(&".files.yaml".to_string()));
        assert!(manifest.files.contains(&"usr/bin/ok".to_string()));
    }

    #[test]
    fn generate_manifest_skips_package_scripts_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("scripts")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join("scripts/pre_install"), "echo pre").unwrap();
        std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

        let manifest = generate_manifest_with_dirs(&destdir).unwrap();

        assert!(!manifest.files.contains(&"scripts/pre_install".to_string()));
        assert!(manifest.files.contains(&"usr/bin/ok".to_string()));
    }
}

/// Manifest containing files and directories for a package
#[derive(Debug, Clone)]
pub struct Manifest {
    pub files: Vec<String>,
    pub directories: Vec<String>,
}

/// Generate manifest with both files and directories
pub fn generate_manifest_with_dirs(destdir: &Path) -> Result<Manifest> {
    let mut files = Vec::new();
    let mut directories = Vec::new();

    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        let rel_path = entry
            .path()
            .strip_prefix(destdir)?
            .to_string_lossy()
            .to_string();

        // Skip the root (empty path)
        if rel_path.is_empty() {
            continue;
        }

        if is_skipped_install_path(&rel_path) {
            continue;
        }

        let file_type = entry.file_type();

        // Check for symlink first
        if file_type.is_symlink() {
            // Track symlinks as files (they get removed the same way)
            files.push(rel_path);
        } else if file_type.is_file() {
            files.push(rel_path);
        } else if file_type.is_dir() {
            directories.push(rel_path);
        }
    }

    Ok(Manifest { files, directories })
}
