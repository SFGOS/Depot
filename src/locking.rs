//! Rootfs-scoped advisory locking helpers.

use crate::config::Config;
use anyhow::{Context, Result};
use fd_lock::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Return the rootfs-scoped lock file path.
pub(crate) fn lock_path(config: &Config) -> PathBuf {
    config.db_dir.join("lock")
}

/// Open the rootfs-scoped lock file as an fd-lock reader/writer.
pub(crate) fn open_lock(config: &Config) -> Result<RwLock<File>> {
    let path = lock_path(config);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create lock dir {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open lock file {}", path.display()))?;
    Ok(RwLock::new(file))
}

/// Acquire a shared/read lock without blocking.
pub(crate) fn try_read<'a>(
    lock: &'a RwLock<File>,
    path: &Path,
    command_name: &str,
) -> Result<RwLockReadGuard<'a, File>> {
    lock.try_read().map_err(|e| {
        if e.kind() == ErrorKind::WouldBlock {
            anyhow::anyhow!(
                "Depot is busy (lock held by another process). Command '{}' needs a shared lock on {}",
                command_name,
                path.display()
            )
        } else {
            anyhow::anyhow!(e).context(format!(
                "Failed to acquire shared lock for '{}' on {}",
                command_name,
                path.display()
            ))
        }
    })
}

/// Acquire an exclusive/write lock without blocking.
pub(crate) fn try_write<'a>(
    lock: &'a mut RwLock<File>,
    path: &Path,
    command_name: &str,
) -> Result<RwLockWriteGuard<'a, File>> {
    lock.try_write().map_err(|e| {
        if e.kind() == ErrorKind::WouldBlock {
            anyhow::anyhow!(
                "Depot is busy (lock held by another process). Command '{}' needs an exclusive lock on {}",
                command_name,
                path.display()
            )
        } else {
            anyhow::anyhow!(e).context(format!(
                "Failed to acquire exclusive lock for '{}' on {}",
                command_name,
                path.display()
            ))
        }
    })
}
