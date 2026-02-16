//! Binary package "build" system — used when package supplies a prebuilt binary installer

use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;
use walkdir::WalkDir;
use crate::package::PackageSpec;
use crate::cross::CrossConfig;

/// For binary packages we simply copy the extracted files into DESTDIR (preserving
/// directory structure). This is useful for .deb packages where extract step
/// already unpacked the data payload into the source directory.
pub fn build(
    _spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    _cross: Option<&CrossConfig>,
) -> Result<()> {
    println!("Binary install: copying files from {} to {} (pkg type={})", src_dir.display(), destdir.display(), _spec.build.flags.binary_type);
    fs::create_dir_all(destdir).with_context(|| format!("Failed to create destdir: {}", destdir.display()))?;

    for entry in WalkDir::new(src_dir) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src_dir).unwrap();
        let target = destdir.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_symlink() {
            let link_target = fs::read_link(entry.path())?;
            if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
            // overwrite existing links/files
            if target.exists() { let _ = fs::remove_file(&target); }
            unix_fs::symlink(link_target, &target)?;
        } else {
            if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
            fs::copy(entry.path(), &target)?;
        }
    }

    Ok(())
}
