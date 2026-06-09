//! Filesystem copy helpers that preserve Unix link topology.

use anyhow::{Context, Result};
use filetime::FileTime;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

type HardlinkKey = (u64, u64);

fn hardlink_key(metadata: &fs::Metadata) -> Option<HardlinkKey> {
    (metadata.nlink() > 1).then_some((metadata.dev(), metadata.ino()))
}

/// Tracks already-copied hardlink groups while copying a set of files.
#[derive(Debug, Default)]
pub(crate) struct HardlinkCopyTracker {
    copied: HashMap<HardlinkKey, PathBuf>,
}

impl HardlinkCopyTracker {
    /// Create an empty tracker for one logical copy operation.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Copy a regular file while preserving hardlinks seen by this tracker.
    pub(crate) fn copy_file(&mut self, src: &Path, dst: &Path) -> Result<()> {
        let metadata = src
            .symlink_metadata()
            .with_context(|| format!("Failed to inspect {}", src.display()))?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("Expected regular file for copy: {}", src.display());
        }

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create dir: {}", parent.display()))?;
        }

        if let Some(first_dst) = hardlink_key(&metadata).and_then(|key| self.copied.get(&key)) {
            fs::hard_link(first_dst, dst).with_context(|| {
                format!(
                    "Failed to create hardlink {} -> {}",
                    dst.display(),
                    first_dst.display()
                )
            })?;
            return Ok(());
        }

        fs::copy(src, dst)
            .with_context(|| format!("Failed to copy {} to {}", src.display(), dst.display()))?;
        preserve_file_metadata(dst, &metadata)?;

        if let Some(key) = hardlink_key(&metadata) {
            self.copied.insert(key, dst.to_path_buf());
        }

        Ok(())
    }
}

/// Copy a tree without following symlinks, preserving symlinks and hardlinks.
pub(crate) fn copy_tree_preserving_links(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create destination dir: {}", dst.display()))?;

    let mut hardlinks = HardlinkCopyTracker::new();
    for entry in WalkDir::new(src).follow_links(false) {
        crate::interrupts::check()?;
        let entry = entry.with_context(|| format!("Failed to walk {}", src.display()))?;
        let rel = entry
            .path()
            .strip_prefix(src)
            .with_context(|| format!("Failed to strip prefix: {}", src.display()))?;
        let target = dst.join(rel);

        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("Failed to create dir: {}", target.display()))?;
            let metadata = entry
                .path()
                .symlink_metadata()
                .with_context(|| format!("Failed to inspect {}", entry.path().display()))?;
            preserve_directory_metadata(&target, &metadata)?;
            continue;
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create dir: {}", parent.display()))?;
        }

        if entry.file_type().is_symlink() {
            let link_target = fs::read_link(entry.path())
                .with_context(|| format!("Failed to read symlink: {}", entry.path().display()))?;
            std::os::unix::fs::symlink(&link_target, &target).with_context(|| {
                format!(
                    "Failed to create symlink {} -> {}",
                    target.display(),
                    link_target.display()
                )
            })?;
        } else {
            hardlinks.copy_file(entry.path(), &target)?;
        }
    }

    Ok(())
}

fn preserve_file_metadata(dst: &Path, metadata: &fs::Metadata) -> Result<()> {
    fs::set_permissions(dst, metadata.permissions())
        .with_context(|| format!("Failed to set permissions for {}", dst.display()))?;
    let atime = FileTime::from_last_access_time(metadata);
    let mtime = FileTime::from_last_modification_time(metadata);
    filetime::set_file_times(dst, atime, mtime)
        .with_context(|| format!("Failed to preserve file timestamps for {}", dst.display()))?;
    Ok(())
}

fn preserve_directory_metadata(dst: &Path, metadata: &fs::Metadata) -> Result<()> {
    fs::set_permissions(dst, metadata.permissions())
        .with_context(|| format!("Failed to set permissions for {}", dst.display()))?;
    let atime = FileTime::from_last_access_time(metadata);
    let mtime = FileTime::from_last_modification_time(metadata);
    filetime::set_file_times(dst, atime, mtime).with_context(|| {
        format!(
            "Failed to preserve directory timestamps for {}",
            dst.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_tree_preserves_hardlinks_and_symlinks() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");
        fs::create_dir_all(src.join("usr/bin"))?;
        fs::write(src.join("usr/bin/uutils"), "multicall")?;
        fs::hard_link(src.join("usr/bin/uutils"), src.join("usr/bin/ls"))?;
        std::os::unix::fs::symlink("uutils", src.join("usr/bin/cat"))?;

        copy_tree_preserving_links(&src, &dst)?;

        let uutils = dst.join("usr/bin/uutils").metadata()?;
        let ls = dst.join("usr/bin/ls").metadata()?;
        assert_eq!(uutils.ino(), ls.ino());
        assert_eq!(uutils.nlink(), 2);
        assert_eq!(
            fs::read_link(dst.join("usr/bin/cat"))?,
            PathBuf::from("uutils")
        );
        Ok(())
    }
}
