//! Staging phase - file collection and cleanup

use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub const INTERNAL_DEPOT_DIR: &str = ".depot";
pub const INTERNAL_OUTPUTS_DIR: &str = ".depot/outputs";

fn is_info_dir_index_path(rel_path: &str) -> bool {
    matches!(rel_path, "usr/info/dir" | "usr/share/info/dir")
        || rel_path.starts_with("usr/info/dir.")
        || rel_path.starts_with("usr/share/info/dir.")
}

fn is_purged_install_basename(rel_path: &str) -> bool {
    Path::new(rel_path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".packlist" || name.ends_with(".pod"))
}

fn is_skipped_install_path(rel_path: &str) -> bool {
    let p = rel_path.trim_start_matches('/');
    p == ".metadata.toml"
        || p == ".files.yaml"
        || p == INTERNAL_DEPOT_DIR
        || p.strip_prefix(INTERNAL_DEPOT_DIR)
            .is_some_and(|rest| rest.starts_with('/'))
        || p == "scripts"
        || p.starts_with("scripts/")
        || is_info_dir_index_path(p)
        || is_purged_install_basename(p)
}

/// Return the internal split-output staging root inside a package `destdir`.
pub fn output_staging_root(destdir: &Path) -> PathBuf {
    destdir.join(INTERNAL_OUTPUTS_DIR)
}

/// Return the staging directory for an additional output package.
pub fn output_staging_dir(destdir: &Path, pkg_name: &str) -> PathBuf {
    output_staging_root(destdir).join(pkg_name)
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

#[derive(Debug, Clone)]
enum KeepMatcher {
    Exact(String),
    Pattern(String),
}

impl KeepMatcher {
    fn from_spec(raw: &str) -> Result<Self> {
        let normalized = normalize_relative_path(raw)?;
        if normalized.contains('*') || normalized.contains('?') {
            Ok(Self::Pattern(normalized))
        } else {
            Ok(Self::Exact(normalized))
        }
    }

    fn matches(&self, rel_path: &str) -> bool {
        match self {
            Self::Exact(p) => p == rel_path,
            Self::Pattern(p) => glob_match_path(p, rel_path),
        }
    }
}

fn glob_match_path(pattern: &str, path: &str) -> bool {
    let p_parts: Vec<&str> = pattern.split('/').collect();
    let s_parts: Vec<&str> = path.split('/').collect();
    glob_match_path_parts(&p_parts, &s_parts)
}

fn glob_match_path_parts(pattern_parts: &[&str], path_parts: &[&str]) -> bool {
    if pattern_parts.is_empty() {
        return path_parts.is_empty();
    }

    if pattern_parts[0] == "**" {
        let mut next = 1usize;
        while next < pattern_parts.len() && pattern_parts[next] == "**" {
            next += 1;
        }
        let rest = &pattern_parts[next..];
        if rest.is_empty() {
            return true;
        }
        for skip in 0..=path_parts.len() {
            if glob_match_path_parts(rest, &path_parts[skip..]) {
                return true;
            }
        }
        return false;
    }

    if path_parts.is_empty() {
        return false;
    }

    glob_match_segment(pattern_parts[0], path_parts[0])
        && glob_match_path_parts(&pattern_parts[1..], &path_parts[1..])
}

fn glob_match_segment(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_match_ti = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
            continue;
        }

        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            star_match_ti = ti;
            continue;
        }

        if let Some(star_pi) = star {
            pi = star_pi + 1;
            star_match_ti += 1;
            ti = star_match_ti;
            continue;
        }

        return false;
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

fn has_known_compressed_suffix(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".zst")
        || lower.ends_with(".gz")
        || lower.ends_with(".xz")
        || lower.ends_with(".bz2")
        || lower.ends_with(".lzma")
        || lower.ends_with(".z")
}

fn is_manpage_rel_path(rel_path: &str) -> bool {
    let rel = rel_path.trim_start_matches('/');
    rel.starts_with("usr/share/man/") && !has_known_compressed_suffix(rel)
}

fn append_os_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(suffix);
    PathBuf::from(s)
}

fn is_elf_file(path: &Path) -> Result<bool> {
    let mut file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut magic = [0u8; 4];
    let n = file
        .read(&mut magic)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(n == 4 && magic == [0x7F, b'E', b'L', b'F'])
}

fn auto_strip_elf_files(destdir: &Path) -> Result<usize> {
    let mut stripped = 0usize;
    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_elf_file(path)? {
            continue;
        }

        let status = Command::new("strip")
            .arg("--strip-debug")
            .arg(path)
            .status()
            .with_context(|| {
                format!(
                    "Failed to execute strip for {} (disable with build.flags.no_strip = true)",
                    path.display()
                )
            })?;
        if !status.success() {
            anyhow::bail!(
                "strip failed for {} with status {} (disable with build.flags.no_strip = true)",
                path.display(),
                status
            );
        }
        stripped += 1;
    }
    Ok(stripped)
}

fn auto_delete_static_archives(destdir: &Path) -> Result<usize> {
    let mut removed = 0usize;
    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "a") {
            continue;
        }

        fs::remove_file(path).with_context(|| {
            format!(
                "Failed to remove static library {} (disable with build.flags.no_delete_static = true)",
                path.display()
            )
        })?;
        removed += 1;
    }
    Ok(removed)
}

fn compress_manpages_zstd(destdir: &Path) -> Result<usize> {
    crate::log_info!("Compressing man pages with zstd...");
    let mut man_files = Vec::new();
    let mut man_symlinks = Vec::new();

    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path().to_path_buf();
        let rel = path
            .strip_prefix(destdir)
            .context("Failed to strip destdir prefix during manpage compression")?
            .to_string_lossy()
            .to_string();
        if !is_manpage_rel_path(&rel) {
            continue;
        }
        if entry.file_type().is_file() {
            man_files.push(path);
        } else if entry.file_type().is_symlink() {
            man_symlinks.push(path);
        }
    }

    let mut compressed = 0usize;
    for path in &man_files {
        let metadata = fs::metadata(path)
            .with_context(|| format!("Failed to inspect man page {}", path.display()))?;
        let out_path = append_os_suffix(path, ".zst");
        let tmp_path = append_os_suffix(&out_path, ".tmp");

        if tmp_path.exists() {
            let _ = fs::remove_file(&tmp_path);
        }

        let mut input = fs::File::open(path)
            .with_context(|| format!("Failed to open man page {}", path.display()))?;
        let out_file = fs::File::create(&tmp_path)
            .with_context(|| format!("Failed to create {}", tmp_path.display()))?;

        let mut encoder = zstd::stream::write::Encoder::new(out_file, 19)
            .with_context(|| format!("Failed to start zstd encoder for {}", path.display()))?;
        io::copy(&mut input, &mut encoder)
            .with_context(|| format!("Failed to compress {}", path.display()))?;
        let mut out_file = encoder
            .finish()
            .with_context(|| format!("Failed to finish zstd compression for {}", path.display()))?;
        out_file
            .flush()
            .with_context(|| format!("Failed to flush {}", tmp_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&tmp_path)?.permissions();
            perms.set_mode(metadata.permissions().mode() & 0o777);
            fs::set_permissions(&tmp_path, perms)?;
        }

        fs::rename(&tmp_path, &out_path).with_context(|| {
            format!(
                "Failed to finalize compressed man page {} -> {}",
                tmp_path.display(),
                out_path.display()
            )
        })?;
        fs::remove_file(path)
            .with_context(|| format!("Failed to remove original man page {}", path.display()))?;
        compressed += 1;
    }

    let mut fixed_symlinks = 0usize;
    for link_path in man_symlinks {
        let target = fs::read_link(&link_path)
            .with_context(|| format!("Failed to read manpage symlink {}", link_path.display()))?;
        let target_s = target.to_string_lossy();
        if has_known_compressed_suffix(&target_s) {
            continue;
        }

        let new_link_path = append_os_suffix(&link_path, ".zst");
        let new_target = append_os_suffix(&target, ".zst");

        if new_link_path.symlink_metadata().is_ok() {
            fs::remove_file(&new_link_path).with_context(|| {
                format!(
                    "Failed to remove existing compressed manpage symlink {}",
                    new_link_path.display()
                )
            })?;
        }

        std::os::unix::fs::symlink(&new_target, &new_link_path).with_context(|| {
            format!(
                "Failed to create compressed manpage symlink {} -> {}",
                new_link_path.display(),
                new_target.display()
            )
        })?;
        fs::remove_file(&link_path).with_context(|| {
            format!(
                "Failed to remove original manpage symlink {}",
                link_path.display()
            )
        })?;
        fixed_symlinks += 1;
    }

    if fixed_symlinks > 0 {
        crate::log_info!(
            "Updated {} man page symlink(s) for .zst targets",
            fixed_symlinks
        );
    }

    Ok(compressed)
}

/// Process staged files - remove .la files/static libs, strip binaries, etc.
pub fn process(destdir: &Path, spec: &PackageSpec) -> Result<()> {
    crate::log_info!("Processing staged files...");

    let mut removed_la_count = 0;

    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();

        // Remove libtool .la files
        if path.extension().map(|e| e == "la").unwrap_or(false) {
            crate::log_info!("  Removing: {}", path.display());
            fs::remove_file(path)?;
            removed_la_count += 1;
        }
    }

    if removed_la_count > 0 {
        crate::log_info!("Removed {} .la file(s)", removed_la_count);
    }

    if spec.build.flags.no_delete_static {
        crate::log_info!(
            "Skipping static library cleanup: disabled by build.flags.no_delete_static"
        );
    } else {
        let removed_static = auto_delete_static_archives(destdir)?;
        if removed_static > 0 {
            crate::log_info!("Removed {} static library archive(s)", removed_static);
        }
    }

    if spec.build.flags.no_strip {
        crate::log_info!("Skipping auto-strip: disabled by build.flags.no_strip");
    } else {
        let stripped = auto_strip_elf_files(destdir)?;
        if stripped > 0 {
            crate::log_info!("Stripped {} ELF file(s)", stripped);
        }
    }

    if spec.build.flags.no_compress_man {
        crate::log_info!("Skipping manpage compression: disabled by build.flags.no_compress_man");
    } else {
        let compressed = compress_manpages_zstd(destdir)?;
        if compressed > 0 {
            crate::log_info!("Compressed {} man page(s) with zstd -22", compressed);
        }
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
        crate::log_info!(
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
            let rel_path_str = rel_path.to_string_lossy().to_string();
            if is_skipped_install_path(&rel_path_str) {
                continue;
            }

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
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};

    fn mk_spec_for_stage_processing() -> PackageSpec {
        let flags = BuildFlags {
            no_strip: true,
            no_compress_man: true,
            ..BuildFlags::default()
        };
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
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Custom,
                flags,
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn process_removes_static_archives_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/lib")).unwrap();
        std::fs::write(destdir.join("usr/lib/libfoo.a"), "static").unwrap();
        std::fs::write(destdir.join("usr/lib/libfoo.la"), "libtool").unwrap();
        std::fs::write(destdir.join("usr/lib/libfoo.so"), "shared").unwrap();

        let spec = mk_spec_for_stage_processing();
        process(&destdir, &spec).unwrap();

        assert!(!destdir.join("usr/lib/libfoo.a").exists());
        assert!(!destdir.join("usr/lib/libfoo.la").exists());
        assert!(destdir.join("usr/lib/libfoo.so").exists());
    }

    #[test]
    fn process_preserves_static_archives_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/lib")).unwrap();
        std::fs::write(destdir.join("usr/lib/libfoo.a"), "static").unwrap();
        std::fs::write(destdir.join("usr/lib/libfoo.la"), "libtool").unwrap();

        let mut spec = mk_spec_for_stage_processing();
        spec.build.flags.no_delete_static = true;
        process(&destdir, &spec).unwrap();

        assert!(destdir.join("usr/lib/libfoo.a").exists());
        assert!(!destdir.join("usr/lib/libfoo.la").exists());
    }

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
    fn install_atomic_keep_wildcard_matches_directory_children() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("etc/pam.d")).unwrap();
        std::fs::create_dir_all(destdir.join("etc/pam.d")).unwrap();
        std::fs::create_dir_all(destdir.join("etc/pam.d/subdir")).unwrap();

        std::fs::write(rootfs.join("etc/pam.d/system-auth"), "existing-auth").unwrap();
        std::fs::write(destdir.join("etc/pam.d/system-auth"), "pkg-auth").unwrap();
        std::fs::write(destdir.join("etc/pam.d/other"), "pkg-other").unwrap();
        std::fs::write(destdir.join("etc/pam.d/subdir/nested"), "pkg-nested").unwrap();

        let keep = vec!["etc/pam.d/*".to_string()];
        let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &keep).unwrap();

        // Existing matched file is preserved and package version becomes .depotnew
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/pam.d/system-auth")).unwrap(),
            "existing-auth"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/pam.d/system-auth.depotnew")).unwrap(),
            "pkg-auth"
        );

        // New matched file installs normally because no existing file is present
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/pam.d/other")).unwrap(),
            "pkg-other"
        );

        // Single-segment * does not cross '/'
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/pam.d/subdir/nested")).unwrap(),
            "pkg-nested"
        );
        assert!(!rootfs.join("etc/pam.d/subdir/nested.depotnew").exists());

        tx.rollback().unwrap();
    }

    #[test]
    fn keep_glob_matches_question_mark_and_not_path_separator() {
        assert!(glob_match_path(
            "etc/pam.d/system-????",
            "etc/pam.d/system-auth"
        ));
        assert!(!glob_match_path("etc/pam.d/*", "etc/pam.d/subdir/file"));
        assert!(glob_match_path("etc/pam.d/*", "etc/pam.d/file"));
        assert!(glob_match_path("etc/pam.d/**", "etc/pam.d/subdir/file"));
        assert!(glob_match_path("etc/**/file", "etc/pam.d/subdir/file"));
        assert!(glob_match_path("etc/pam.d/**", "etc/pam.d"));
    }

    #[test]
    fn is_manpage_rel_path_detects_uncompressed_manpages() {
        assert!(is_manpage_rel_path("usr/share/man/man1/ls.1"));
        assert!(is_manpage_rel_path("/usr/share/man/man5/pam.d.5"));
        assert!(!is_manpage_rel_path("usr/share/man/man1/ls.1.zst"));
        assert!(!is_manpage_rel_path("usr/share/doc/readme"));
    }

    #[test]
    fn is_elf_file_detects_magic_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let elf = tmp.path().join("elf.bin");
        let text = tmp.path().join("text.txt");
        std::fs::write(&elf, [0x7F, b'E', b'L', b'F', 0x02, 0x01]).unwrap();
        std::fs::write(&text, b"#!/bin/sh\n").unwrap();

        assert!(is_elf_file(&elf).unwrap());
        assert!(!is_elf_file(&text).unwrap());
    }

    #[test]
    fn compress_manpages_zstd_rewrites_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let man1 = dest.join("usr/share/man/man1");
        std::fs::create_dir_all(&man1).unwrap();

        let page = man1.join("foo.1");
        std::fs::write(&page, b"foo manpage\n").unwrap();
        std::os::unix::fs::symlink("foo.1", man1.join("bar.1")).unwrap();

        let count = compress_manpages_zstd(&dest).unwrap();
        assert_eq!(count, 1);
        assert!(!man1.join("foo.1").exists());
        assert!(man1.join("foo.1.zst").exists());
        assert!(!man1.join("bar.1").exists());

        let link_meta = std::fs::symlink_metadata(man1.join("bar.1.zst")).unwrap();
        assert!(link_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(man1.join("bar.1.zst")).unwrap(),
            PathBuf::from("foo.1.zst")
        );

        let file = std::fs::File::open(man1.join("foo.1.zst")).unwrap();
        let mut decoder = zstd::stream::read::Decoder::new(file).unwrap();
        let mut out = String::new();
        use std::io::Read as _;
        decoder.read_to_string(&mut out).unwrap();
        assert_eq!(out, "foo manpage\n");
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
        std::fs::create_dir_all(destdir.join("usr/info")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/share/info")).unwrap();
        std::fs::write(destdir.join("usr/info/dir"), "legacy index").unwrap();
        std::fs::write(destdir.join("usr/info/dir.bz2"), "legacy index bz2").unwrap();
        std::fs::write(destdir.join("usr/share/info/dir"), "index").unwrap();
        std::fs::write(destdir.join("usr/share/info/dir.gz"), "index gz").unwrap();
        std::fs::write(destdir.join("usr/share/info/ok.info"), "ok").unwrap();

        let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        assert!(!rootfs.join("usr/info/dir").exists());
        assert!(!rootfs.join("usr/info/dir.bz2").exists());
        assert!(!rootfs.join("usr/share/info/dir").exists());
        assert!(!rootfs.join("usr/share/info/dir.gz").exists());
        assert!(rootfs.join("usr/share/info/ok.info").exists());
    }

    #[test]
    fn install_atomic_skips_packlists_and_pod_files() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/core_perl")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/share/doc/perl-error")).unwrap();
        std::fs::write(
            destdir.join("usr/lib/perl5/5.42/core_perl/perllocal.pod"),
            "perllocal",
        )
        .unwrap();
        std::fs::write(
            destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist"),
            "packlist",
        )
        .unwrap();
        std::fs::write(destdir.join("usr/share/doc/perl-error/Error.pod"), "pod").unwrap();
        std::fs::write(destdir.join("usr/share/doc/perl-error/README"), "readme").unwrap();

        let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        assert!(
            !rootfs
                .join("usr/lib/perl5/5.42/core_perl/perllocal.pod")
                .exists()
        );
        assert!(
            !rootfs
                .join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist")
                .exists()
        );
        assert!(!rootfs.join("usr/share/doc/perl-error/Error.pod").exists());
        assert!(rootfs.join("usr/share/doc/perl-error/README").exists());
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
    fn install_atomic_skips_internal_output_staging_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(destdir.join(".depot/outputs/clang/usr/bin")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join(".depot/outputs/clang/usr/bin/clang"), "clang").unwrap();
        std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

        let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        assert!(rootfs.join("usr/bin/ok").exists());
        assert!(!rootfs.join(".depot").exists());
    }

    #[test]
    fn generate_manifest_skips_info_dir_index() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/info")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/share/info")).unwrap();
        std::fs::write(destdir.join("usr/info/dir"), "legacy index").unwrap();
        std::fs::write(destdir.join("usr/info/dir.zst"), "legacy index zst").unwrap();
        std::fs::write(destdir.join("usr/share/info/dir"), "index").unwrap();
        std::fs::write(destdir.join("usr/share/info/dir.xz"), "index xz").unwrap();
        std::fs::write(destdir.join("usr/share/info/ok.info"), "ok").unwrap();

        let manifest = generate_manifest_with_dirs(&destdir).unwrap();

        assert!(!manifest.files.contains(&"usr/info/dir".to_string()));
        assert!(!manifest.files.contains(&"usr/info/dir.zst".to_string()));
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
    fn generate_manifest_skips_packlists_and_pod_files() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/core_perl")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/share/doc/perl-error")).unwrap();
        std::fs::write(
            destdir.join("usr/lib/perl5/5.42/core_perl/perllocal.pod"),
            "perllocal",
        )
        .unwrap();
        std::fs::write(
            destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist"),
            "packlist",
        )
        .unwrap();
        std::fs::write(destdir.join("usr/share/doc/perl-error/Error.pod"), "pod").unwrap();
        std::fs::write(destdir.join("usr/share/doc/perl-error/README"), "readme").unwrap();

        let manifest = generate_manifest_with_dirs(&destdir).unwrap();

        assert!(
            !manifest
                .files
                .contains(&"usr/lib/perl5/5.42/core_perl/perllocal.pod".to_string())
        );
        assert!(
            !manifest
                .files
                .contains(&"usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist".to_string())
        );
        assert!(
            !manifest
                .files
                .contains(&"usr/share/doc/perl-error/Error.pod".to_string())
        );
        assert!(
            manifest
                .files
                .contains(&"usr/share/doc/perl-error/README".to_string())
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

    #[test]
    fn generate_manifest_skips_internal_output_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::create_dir_all(destdir.join(".depot/outputs/clang/usr/bin")).unwrap();
        std::fs::write(destdir.join("usr/bin/llvm-config"), "ok").unwrap();
        std::fs::write(destdir.join(".depot/outputs/clang/usr/bin/clang"), "clang").unwrap();

        let manifest = generate_manifest_with_dirs(&destdir).unwrap();

        assert!(manifest.files.contains(&"usr/bin/llvm-config".to_string()));
        assert!(
            !manifest
                .files
                .contains(&".depot/outputs/clang/usr/bin/clang".to_string())
        );
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
