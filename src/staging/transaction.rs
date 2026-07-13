use super::*;

#[derive(Debug)]
pub struct FsTransaction {
    rootfs: PathBuf,
    pub(super) tx_dir: PathBuf,
    backed_up: Vec<String>,
    created: Vec<String>,
    relocated: Vec<String>,
    removed: Vec<String>,
}

pub(super) fn is_directory_empty(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("Failed to read directory {}", path.display()))?;
    Ok(entries.next().transpose()?.is_none())
}

pub(super) fn backup_existing_path(src: &Path, backup_path: &Path, rel: &str) -> Result<()> {
    let metadata = src
        .symlink_metadata()
        .with_context(|| format!("Failed to inspect existing path {}", rel))?;

    if let Some(parent) = backup_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create backup dir {}", parent.display()))?;
    }

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(src)
            .with_context(|| format!("Failed to read existing symlink target {}", rel))?;
        std::os::unix::fs::symlink(&target, backup_path)
            .with_context(|| format!("Failed to backup symlink {}", rel))?;
    } else if metadata.file_type().is_dir() {
        fs::create_dir_all(backup_path)
            .with_context(|| format!("Failed to backup directory {}", rel))?;
        apply_unix_mode(backup_path, &metadata)?;
    } else {
        fs::copy(src, backup_path).with_context(|| format!("Failed to backup file {}", rel))?;
    }

    Ok(())
}

pub(super) fn move_directory_contents(src_dir: &Path, dst_dir: &Path) -> Result<()> {
    fs::create_dir_all(dst_dir)
        .with_context(|| format!("Failed to create directory {}", dst_dir.display()))?;

    for entry in
        fs::read_dir(src_dir).with_context(|| format!("Failed to read {}", src_dir.display()))?
    {
        let entry = entry?;
        move_tree_preserving_layout(&entry.path(), &dst_dir.join(entry.file_name()))?;
    }

    fs::remove_dir(src_dir).with_context(|| format!("Failed to remove {}", src_dir.display()))?;
    Ok(())
}

pub(super) fn copy_tree_preserving_layout_no_overwrite(
    src_root: &Path,
    dst_root: &Path,
    logical_root: &str,
    created: &mut Vec<String>,
) -> Result<()> {
    for entry in WalkDir::new(src_root).follow_links(false) {
        let entry = entry
            .with_context(|| format!("Failed to walk relocation tree {}", src_root.display()))?;
        let src_path = entry.path();
        let rel = src_path
            .strip_prefix(src_root)
            .with_context(|| format!("Failed to strip relocation root {}", src_root.display()))?;
        if rel.as_os_str().is_empty() {
            continue;
        }

        let dst_path = dst_root.join(rel);
        let metadata = src_path
            .symlink_metadata()
            .with_context(|| format!("Failed to inspect {}", src_path.display()))?;
        let file_type = metadata.file_type();

        if file_type.is_dir() {
            match dst_path.symlink_metadata() {
                Ok(dst_meta) => {
                    if !dst_meta.file_type().is_dir() {
                        anyhow::bail!(
                            "Failed to replay relocated directory into {}: destination exists and is not a directory",
                            dst_path.display()
                        );
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir_all(&dst_path).with_context(|| {
                        format!("Failed to create directory {}", dst_path.display())
                    })?;
                    apply_unix_mode(&dst_path, &metadata)?;
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("Failed to inspect {}", dst_path.display()));
                }
            }
            continue;
        }

        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        if let Ok(dst_metadata) = dst_path.symlink_metadata() {
            if duplicate_staged_path_is_equivalent(src_path, &metadata, &dst_path, &dst_metadata)? {
                continue;
            }
            anyhow::bail!(
                "Failed to replay relocated path into {}: destination already exists",
                dst_path.display()
            );
        }

        if file_type.is_symlink() {
            let target = fs::read_link(src_path)
                .with_context(|| format!("Failed to read symlink {}", src_path.display()))?;
            std::os::unix::fs::symlink(&target, &dst_path).with_context(|| {
                format!(
                    "Failed to create relocated symlink {} -> {}",
                    dst_path.display(),
                    target.display()
                )
            })?;
        } else {
            fs::copy(src_path, &dst_path).with_context(|| {
                format!(
                    "Failed to copy relocated path {} to {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
            apply_unix_mode(&dst_path, &metadata)?;
        }

        let logical = Path::new(logical_root).join(rel);
        let logical = logical
            .to_str()
            .context("Relocated install paths must be valid UTF-8")?
            .to_string();
        created.push(logical);
    }

    Ok(())
}

pub(super) fn symlink_target_path_inside_rootfs(
    rootfs: &Path,
    link_rel: &str,
    target: &Path,
) -> Result<Option<PathBuf>> {
    let mut normalized = PathBuf::new();
    if target.is_absolute() {
        for component in target.components() {
            match component {
                Component::RootDir => {}
                Component::CurDir => {}
                Component::Normal(segment) => normalized.push(segment),
                Component::ParentDir | Component::Prefix(_) => return Ok(None),
            }
        }
    } else {
        if let Some(parent) = Path::new(link_rel).parent() {
            normalized.push(parent);
        }
        for component in target.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(segment) => normalized.push(segment),
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Ok(None);
                    }
                }
                Component::RootDir | Component::Prefix(_) => return Ok(None),
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Ok(None);
    }

    Ok(Some(rootfs.join(normalized)))
}

pub(super) fn can_relocate_directory_for_symlink_swap(
    rootfs: &Path,
    link_rel: &str,
    symlink_path: &Path,
) -> Result<bool> {
    let target = fs::read_link(symlink_path)
        .with_context(|| format!("Failed to read symlink {}", symlink_path.display()))?;
    let Some(target_path) = symlink_target_path_inside_rootfs(rootfs, link_rel, &target)? else {
        return Ok(false);
    };

    match target_path.symlink_metadata() {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err)
            .with_context(|| format!("Failed to inspect symlink target {}", target_path.display())),
    }
}

pub(super) fn remove_path_in_place(path: &Path, rel: &str) -> Result<()> {
    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("Failed to inspect existing path {}", rel))?;

    if metadata.file_type().is_dir() {
        fs::remove_dir(path)
            .with_context(|| format!("Failed to remove obsolete directory {}", rel))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("Failed to remove obsolete file/symlink {}", rel))?;
    }

    Ok(())
}

pub(super) fn backup_and_remove_obsolete_path(
    tx: &mut FsTransaction,
    rootfs: &Path,
    rel: &str,
    require_empty_dir: bool,
) -> Result<bool> {
    let dest_path = rootfs.join(rel);
    let metadata = match dest_path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to inspect obsolete path before removal: {}", rel)
            });
        }
    };

    if metadata.file_type().is_dir() {
        let empty = is_directory_empty(&dest_path)?;
        if !empty {
            if require_empty_dir {
                anyhow::bail!(
                    "Refusing to replace existing non-empty directory with packaged file/symlink: {}",
                    rel
                );
            }
            return Ok(false);
        }
    }

    let backup_path = tx.removed_backup_path(rel);
    backup_existing_path(&dest_path, &backup_path, rel)?;
    remove_path_in_place(&dest_path, rel)?;
    tx.removed.push(rel.to_string());
    Ok(true)
}

pub(super) fn remove_obsolete_children_for_dir(
    tx: &mut FsTransaction,
    rootfs: &Path,
    dir_rel: &str,
    remove_paths: &[String],
) -> Result<()> {
    let prefix = format!("{dir_rel}/");
    let mut nested_paths: Vec<&str> = remove_paths
        .iter()
        .filter_map(|path| path.strip_prefix(&prefix).map(|_| path.as_str()))
        .collect();
    nested_paths.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));

    for rel in nested_paths {
        let _ = backup_and_remove_obsolete_path(tx, rootfs, rel, false)?;
    }

    Ok(())
}

impl FsTransaction {
    fn backup_path(&self, rel: &str) -> PathBuf {
        self.tx_dir.join("backup").join(rel)
    }

    fn removed_backup_path(&self, rel: &str) -> PathBuf {
        self.tx_dir.join("removed").join(rel)
    }

    fn relocated_path(&self, rel: &str) -> PathBuf {
        self.tx_dir.join("relocated").join(rel)
    }

    fn relocate_directory_for_symlink_swap(&mut self, rel: &str) -> Result<()> {
        let src = self.rootfs.join(rel);
        let relocated = self.relocated_path(rel);
        move_directory_contents(&src, &relocated)?;
        self.relocated.push(rel.to_string());
        Ok(())
    }

    fn replay_relocated_dir_if_present(&mut self, rel: &str) -> Result<()> {
        let relocated = self.relocated_path(rel);
        match relocated.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => {
                anyhow::bail!(
                    "Relocation staging path is not a directory: {}",
                    relocated.display()
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to inspect {}", relocated.display()));
            }
        }

        copy_tree_preserving_layout_no_overwrite(
            &relocated,
            &self.rootfs.join(rel),
            rel,
            &mut self.created,
        )
    }

    fn restore_relocated_dir(&self, rel: &str) -> Result<()> {
        let relocated = self.relocated_path(rel);
        match relocated.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => {
                anyhow::bail!(
                    "Relocation staging path is not a directory: {}",
                    relocated.display()
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to inspect {}", relocated.display()));
            }
        }

        let dst = self.rootfs.join(rel);
        match dst.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => {
                anyhow::bail!(
                    "Failed to restore relocated directory contents into {}: destination is not a directory",
                    dst.display()
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&dst)
                    .with_context(|| format!("Failed to create directory {}", dst.display()))?;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to inspect {}", dst.display()));
            }
        }

        move_directory_contents(&relocated, &dst)?;
        cleanup_empty_parent_dirs(&self.tx_dir, &relocated)?;
        Ok(())
    }

    fn restore_backup_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        let metadata = src
            .symlink_metadata()
            .with_context(|| format!("Failed to inspect backup entry {}", src.display()))?;

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create restore dir {}", parent.display()))?;
        }

        if metadata.file_type().is_dir() {
            match dst.symlink_metadata() {
                Ok(dst_meta) if dst_meta.file_type().is_dir() => {}
                Ok(dst_meta) if dst_meta.file_type().is_symlink() => {
                    fs::remove_file(dst)
                        .with_context(|| format!("Failed to remove {}", dst.display()))?;
                }
                Ok(_) => {
                    fs::remove_file(dst)
                        .with_context(|| format!("Failed to remove {}", dst.display()))?;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("Failed to inspect {}", dst.display()));
                }
            }
            fs::create_dir_all(dst)
                .with_context(|| format!("Failed to restore directory {}", dst.display()))?;
            apply_unix_mode(dst, &metadata)?;
            return Ok(());
        }

        let _ = fs::remove_file(dst);
        match fs::rename(src, dst) {
            Ok(()) => Ok(()),
            Err(_) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(src)
                    .with_context(|| format!("Failed to read backup symlink {}", src.display()))?;
                std::os::unix::fs::symlink(&target, dst)
                    .with_context(|| format!("Failed to restore symlink {}", dst.display()))
            }
            Err(_) => {
                fs::copy(src, dst).with_context(|| {
                    format!(
                        "Failed to restore file {} from {}",
                        dst.display(),
                        src.display()
                    )
                })?;
                Ok(())
            }
        }
    }

    /// Roll back file operations performed by `install_atomic`.
    pub fn rollback(&self) -> Result<()> {
        // Remove files that were newly created
        for rel in &self.created {
            let dst = self.rootfs.join(rel);
            let _ = fs::remove_file(dst);
        }

        // Restore overwritten paths first so relocated and removed children have their parent layout.
        for rel in &self.backed_up {
            let src = self.backup_path(rel);
            let dst = self.rootfs.join(rel);
            if src.symlink_metadata().is_ok() {
                self.restore_backup_entry(&src, &dst)?;
            }
        }

        for rel in &self.relocated {
            self.restore_relocated_dir(rel)?;
        }

        // Restore removed files/directories.
        for rel in &self.removed {
            let src = self.removed_backup_path(rel);
            let dst = self.rootfs.join(rel);
            if src.symlink_metadata().is_ok() {
                self.restore_backup_entry(&src, &dst)?;
            }
        }

        Ok(())
    }

    /// Commit the transaction (delete backup directory).
    pub fn commit(self) -> Result<()> {
        let tx_base_dir = self.tx_dir.parent().map(Path::to_path_buf);
        if self.tx_dir.exists() {
            fs::remove_dir_all(&self.tx_dir)?;
        }
        if let Some(tx_base_dir) = tx_base_dir {
            match fs::remove_dir(&tx_base_dir) {
                Ok(()) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) => {}
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("Failed to remove tx dir {}", tx_base_dir.display())
                    });
                }
            }
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
    let tx_base_dir = if rootfs != Path::new("/") && tx_base_dir.starts_with(rootfs) {
        rootfs.join(".depot-tx")
    } else {
        tx_base_dir.to_path_buf()
    };
    let keep_rules: Vec<KeepMatcher> = keep_paths
        .iter()
        .map(|p| KeepMatcher::from_spec(p))
        .collect::<Result<Vec<_>>>()?;
    let keep_set: HashSet<String> = keep_rules
        .iter()
        .filter_map(|m| match m {
            KeepMatcher::Exact(p) => Some(p.clone()),
            KeepMatcher::Pattern(_) => None,
        })
        .collect();
    let remove_set: HashSet<&str> = remove_paths.iter().map(String::as_str).collect();

    fs::create_dir_all(&tx_base_dir)
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
        relocated: Vec::new(),
        removed: Vec::new(),
    };
    let mut staged_paths = HashSet::new();
    let mut installed_hardlinks: HashMap<HardlinkKey, String> = HashMap::new();

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
            let rel_path_str = rel_path.to_string_lossy().to_string();
            if is_skipped_install_path(&rel_path_str) {
                continue;
            }

            let dest_path = rootfs.join(rel_path);
            if !dest_path.exists() {
                fs::create_dir_all(&dest_path)?;
                apply_unix_mode(&dest_path, &src_path.symlink_metadata()?)?;
            }
            staged_paths.insert(rel_path_str);
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

            let keep_match = if keep_set.contains(&rel_path) {
                true
            } else {
                keep_rules.iter().any(|m| m.matches(&rel_path))
            };
            let keep_as_depotnew = keep_match && rootfs.join(&rel_path).exists();
            let install_rel_path = if keep_as_depotnew {
                format!("{}.depotnew", rel_path)
            } else {
                rel_path.clone()
            };

            let dest_path = rootfs.join(&install_rel_path);

            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }

            if let Ok(dest_meta) = dest_path.symlink_metadata() {
                let backup_path = tx.backup_path(&install_rel_path);
                if dest_meta.file_type().is_dir() {
                    let can_relocate = file_type.is_symlink()
                        && can_relocate_directory_for_symlink_swap(
                            rootfs,
                            &install_rel_path,
                            src_path,
                        )?;
                    if !remove_set.contains(install_rel_path.as_str()) && !can_relocate {
                        anyhow::bail!(
                            "Refusing to replace existing directory with packaged file/symlink: {}",
                            install_rel_path
                        );
                    }
                    backup_existing_path(&dest_path, &backup_path, &install_rel_path)?;
                    tx.backed_up.push(install_rel_path.clone());
                    remove_obsolete_children_for_dir(
                        &mut tx,
                        rootfs,
                        &install_rel_path,
                        remove_paths,
                    )?;
                    if file_type.is_symlink() {
                        tx.relocate_directory_for_symlink_swap(&install_rel_path)?;
                    }
                } else {
                    backup_existing_path(&dest_path, &backup_path, &install_rel_path)?;
                    tx.backed_up.push(install_rel_path.clone());
                }
            } else {
                tx.created.push(install_rel_path.clone());
            }

            // Install new file/symlink
            // Remove destination if it exists (we backed it up) so we can overwrite
            if let Ok(dest_meta) = dest_path.symlink_metadata() {
                if dest_meta.file_type().is_dir() {
                    let relocated_for_symlink_swap =
                        tx.relocated.iter().any(|rel| rel == &install_rel_path);
                    if !remove_set.contains(install_rel_path.as_str())
                        && !relocated_for_symlink_swap
                    {
                        anyhow::bail!(
                            "Refusing to replace existing directory with packaged file/symlink: {}",
                            install_rel_path
                        );
                    }
                    if !is_directory_empty(&dest_path)? {
                        anyhow::bail!(
                            "Refusing to replace existing non-empty directory with packaged file/symlink: {}",
                            install_rel_path
                        );
                    }
                    fs::remove_dir(&dest_path)?;
                } else {
                    fs::remove_file(&dest_path)?;
                }
            }

            if file_type.is_symlink() {
                let target = fs::read_link(src_path)
                    .with_context(|| format!("Failed to read staged symlink {}", rel_path))?;
                std::os::unix::fs::symlink(target, &dest_path)
                    .with_context(|| format!("Failed to create symlink: {}", install_rel_path))?;
                tx.replay_relocated_dir_if_present(&install_rel_path)?;
            } else {
                let hardlink_key = if keep_as_depotnew {
                    None
                } else {
                    hardlink_key(&metadata)
                };
                if let Some(first_rel_path) =
                    hardlink_key.and_then(|key| installed_hardlinks.get(&key))
                {
                    let first_path = rootfs.join(first_rel_path);
                    fs::hard_link(&first_path, &dest_path).with_context(|| {
                        format!(
                            "Failed to install hardlink: {} -> {}",
                            install_rel_path, first_rel_path
                        )
                    })?;
                } else {
                    fs::copy(src_path, &dest_path)
                        .with_context(|| format!("Failed to install: {}", install_rel_path))?;
                    apply_unix_mode(&dest_path, &metadata)?;
                    if let Some(key) = hardlink_key {
                        installed_hardlinks.insert(key, install_rel_path.clone());
                    }
                }
            }
            staged_paths.insert(install_rel_path);
        }

        // Remove obsolete files/directories left behind by the previous version.
        for rel in remove_paths {
            if staged_paths.contains(rel) || tx.removed.iter().any(|removed| removed == rel) {
                continue;
            }
            let _ = backup_and_remove_obsolete_path(&mut tx, rootfs, rel, false)?;
        }

        Ok(())
    })();

    if let Err(e) = result {
        let _ = tx.rollback();
        return Err(e);
    }

    Ok(tx)
}
