//! Staging phase - file collection and cleanup

use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

pub const INTERNAL_DEPOT_DIR: &str = ".depot";
pub const INTERNAL_OUTPUTS_DIR: &str = ".depot/outputs";

type HardlinkKey = (u64, u64);

fn hardlink_key(metadata: &fs::Metadata) -> Option<HardlinkKey> {
    (metadata.nlink() > 1).then_some((metadata.dev(), metadata.ino()))
}

fn is_info_dir_index_path(rel_path: &str) -> bool {
    matches!(
        rel_path,
        "usr/info/dir" | "usr/share/info/dir" | "system/documentation/info/dir"
    ) || rel_path.starts_with("usr/info/dir.")
        || rel_path.starts_with("usr/share/info/dir.")
        || rel_path.starts_with("system/documentation/info/dir.")
}

fn is_purged_install_basename(rel_path: &str) -> bool {
    Path::new(rel_path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".packlist" || name.ends_with(".pod"))
}

pub(crate) fn is_purged_payload_path(rel_path: &str) -> bool {
    let p = rel_path.trim_start_matches('/');
    is_info_dir_index_path(p) || is_purged_install_basename(p)
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
        || is_purged_payload_path(p)
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

fn logical_payload_rel_path(destdir: &Path, path: &Path) -> Result<Option<String>> {
    let rel = path
        .strip_prefix(destdir)
        .context("Failed to strip destdir prefix during path normalization")?;
    let output_root = Path::new(INTERNAL_OUTPUTS_DIR);
    if !rel.starts_with(output_root) {
        let rel = rel.to_string_lossy().to_string();
        if rel.is_empty() {
            return Ok(None);
        }
        return Ok(Some(rel));
    }

    let mut comps = rel.components();
    let _internal = comps.next();
    let _outputs = comps.next();
    let _package = comps.next();
    let logical = comps.as_path();
    if logical.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(logical.to_string_lossy().to_string()))
}

fn is_manpage_rel_path(rel_path: &str) -> bool {
    let rel = rel_path.trim_start_matches('/');
    rel.starts_with("usr/share/man/") && !has_known_compressed_suffix(rel)
}

fn normalize_doc_dir(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        anyhow::bail!("doc_dirs entries must not be empty");
    }

    let relative = trimmed.trim_start_matches('/');
    if relative.is_empty() {
        anyhow::bail!("doc_dirs entries must not resolve to the filesystem root");
    }

    let p = Path::new(relative);
    let mut normalized = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(seg) => normalized.push(seg),
            Component::CurDir => {}
            _ => {
                anyhow::bail!(
                    "doc_dirs entries must not contain traversal or root components: {}",
                    trimmed
                );
            }
        }
    }

    let normalized = normalized
        .to_str()
        .context("doc_dirs entries must be valid UTF-8")?
        .to_string();
    if normalized.is_empty() {
        anyhow::bail!(
            "doc_dirs entries must not resolve to an empty path: {}",
            trimmed
        );
    }
    Ok(normalized)
}

fn cleanup_empty_parent_dirs(root: &Path, start: &Path) -> Result<()> {
    let mut current = start.parent();
    while let Some(dir) = current {
        if dir == root {
            break;
        }

        match fs::remove_dir(dir) {
            Ok(()) => current = dir.parent(),
            Err(err) if err.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(err) if err.kind() == io::ErrorKind::NotFound => current = dir.parent(),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to prune empty dir {}", dir.display()));
            }
        }
    }
    Ok(())
}

fn move_tree_preserving_layout(src: &Path, dst: &Path) -> Result<()> {
    let metadata = src
        .symlink_metadata()
        .with_context(|| format!("Failed to inspect {}", src.display()))?;
    let file_type = metadata.file_type();

    if file_type.is_dir() {
        match dst.symlink_metadata() {
            Ok(dst_meta) => {
                if !dst_meta.file_type().is_dir() {
                    anyhow::bail!(
                        "Failed to move {} into {}: destination exists and is not a directory",
                        src.display(),
                        dst.display()
                    );
                }
                for entry in fs::read_dir(src)
                    .with_context(|| format!("Failed to read {}", src.display()))?
                {
                    let entry = entry?;
                    let child_src = entry.path();
                    let child_dst = dst.join(entry.file_name());
                    move_tree_preserving_layout(&child_src, &child_dst)?;
                }
                fs::remove_dir(src)
                    .with_context(|| format!("Failed to remove {}", src.display()))?;
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create {}", parent.display()))?;
                }
                fs::rename(src, dst).with_context(|| {
                    format!(
                        "Failed to move documentation tree {} -> {}",
                        src.display(),
                        dst.display()
                    )
                })?;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to inspect {}", dst.display()));
            }
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        match dst.symlink_metadata() {
            Ok(dst_metadata)
                if duplicate_staged_path_is_equivalent(src, &metadata, dst, &dst_metadata)? =>
            {
                fs::remove_file(src).with_context(|| {
                    format!("Failed to remove duplicate staged path {}", src.display())
                })?;
                return Ok(());
            }
            Ok(_) => {
                anyhow::bail!(
                    "Failed to move {} into {}: destination already exists",
                    src.display(),
                    dst.display()
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to inspect {}", dst.display()));
            }
        }
        fs::rename(src, dst).with_context(|| {
            format!(
                "Failed to move documentation path {} -> {}",
                src.display(),
                dst.display()
            )
        })?;
    }

    Ok(())
}

fn duplicate_staged_path_is_equivalent(
    src: &Path,
    src_metadata: &fs::Metadata,
    dst: &Path,
    dst_metadata: &fs::Metadata,
) -> Result<bool> {
    let src_type = src_metadata.file_type();
    let dst_type = dst_metadata.file_type();

    if src_type.is_symlink() || dst_type.is_symlink() {
        if !(src_type.is_symlink() && dst_type.is_symlink()) {
            return Ok(false);
        }
        let src_target = fs::read_link(src)
            .with_context(|| format!("Failed to read symlink {}", src.display()))?;
        let dst_target = fs::read_link(dst)
            .with_context(|| format!("Failed to read symlink {}", dst.display()))?;
        return Ok(src_target == dst_target);
    }

    if !src_metadata.is_file() || !dst_metadata.is_file() {
        return Ok(false);
    }
    if src_metadata.len() != dst_metadata.len() {
        return Ok(false);
    }

    files_have_same_contents(src, dst)
}

fn files_have_same_contents(left: &Path, right: &Path) -> Result<bool> {
    let mut left = fs::File::open(left)
        .with_context(|| format!("Failed to open staged file {}", left.display()))?;
    let mut right = fs::File::open(right)
        .with_context(|| format!("Failed to open staged file {}", right.display()))?;
    let mut left_buf = [0u8; 8192];
    let mut right_buf = [0u8; 8192];

    loop {
        let left_read = left.read(&mut left_buf)?;
        let right_read = right.read(&mut right_buf)?;
        if left_read != right_read {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
        if left_buf[..left_read] != right_buf[..right_read] {
            return Ok(false);
        }
    }
}

fn split_docs_for_output(
    output_destdir: &Path,
    docs_destdir: &Path,
    doc_dirs: &[String],
) -> Result<usize> {
    let mut moved = 0usize;

    for rel_dir in doc_dirs {
        let src = output_destdir.join(rel_dir);
        let metadata = match src.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to inspect {}", src.display()));
            }
        };

        if !metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            continue;
        }

        let dst = docs_destdir.join(rel_dir);
        move_tree_preserving_layout(&src, &dst)?;
        cleanup_empty_parent_dirs(output_destdir, &src)?;
        moved += 1;
    }

    Ok(moved)
}

fn split_docs_outputs(destdir: &Path, spec: &PackageSpec) -> Result<usize> {
    if !spec.build.flags.split_docs {
        return Ok(0);
    }

    let mut doc_dirs = vec![
        normalize_doc_dir("/usr/share/doc")?,
        normalize_doc_dir("/usr/share/gtk-doc")?,
    ];
    for custom in &spec.build.flags.doc_dirs {
        let normalized = normalize_doc_dir(custom)?;
        if !doc_dirs.contains(&normalized) {
            doc_dirs.push(normalized);
        }
    }

    let mut moved = 0usize;
    for output in spec.outputs() {
        if output.name.ends_with("-docs") {
            continue;
        }

        let output_destdir = if output.name == spec.package.name {
            destdir.to_path_buf()
        } else {
            output_staging_dir(destdir, &output.name)
        };
        if !output_destdir.exists() {
            continue;
        }

        let docs_pkg = spec.docs_package_for_output(&output);
        let docs_destdir = output_staging_dir(destdir, &docs_pkg.name);
        moved += split_docs_for_output(&output_destdir, &docs_destdir, &doc_dirs)?;
    }

    Ok(moved)
}

fn append_os_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(unix)]
fn apply_unix_mode(dst: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o7777;
    fs::set_permissions(dst, fs::Permissions::from_mode(mode))
        .with_context(|| format!("Failed to set permissions on {}", dst.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn apply_unix_mode(_dst: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
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

fn auto_strip_elf_files(destdir: &Path, strip_command: &str) -> Result<usize> {
    let mut stripped = 0usize;
    let mut hardlink_keys_by_path: HashMap<PathBuf, HardlinkKey> = HashMap::new();
    let mut stripped_hardlinks: HashMap<HardlinkKey, PathBuf> = HashMap::new();
    let strip_command = strip_command.trim();
    let strip_command = if strip_command.is_empty() {
        "strip"
    } else {
        strip_command
    };

    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let metadata = path
            .symlink_metadata()
            .with_context(|| format!("Failed to inspect {}", path.display()))?;
        if let Some(key) = hardlink_key(&metadata) {
            hardlink_keys_by_path.insert(path.to_path_buf(), key);
        }
    }

    for entry in WalkDir::new(destdir).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let metadata = path
            .symlink_metadata()
            .with_context(|| format!("Failed to inspect {}", path.display()))?;
        let hardlink_key = hardlink_keys_by_path
            .get(path)
            .copied()
            .or_else(|| hardlink_key(&metadata));
        if !is_elf_file(path)? {
            continue;
        }
        if let Some(stripped_path) = hardlink_key.and_then(|key| stripped_hardlinks.get(&key)) {
            fs::remove_file(path).with_context(|| {
                format!(
                    "Failed to replace hardlinked ELF after stripping: {}",
                    path.display()
                )
            })?;
            fs::hard_link(stripped_path, path).with_context(|| {
                format!(
                    "Failed to restore stripped hardlink {} -> {}",
                    path.display(),
                    stripped_path.display()
                )
            })?;
            stripped += 1;
            continue;
        }

        let status = Command::new(strip_command)
            .arg("--strip-debug")
            .arg(path)
            .status()
            .with_context(|| {
                format!(
                    "Failed to execute {} for {} (disable with build.flags.no_strip = true)",
                    strip_command,
                    path.display()
                )
            })?;
        if !status.success() {
            anyhow::bail!(
                "{} failed for {} with status {} (disable with build.flags.no_strip = true)",
                strip_command,
                path.display(),
                status
            );
        }
        stripped += 1;
        if let Some(key) = hardlink_key {
            stripped_hardlinks.insert(key, path.to_path_buf());
        }
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
        let Some(rel) = logical_payload_rel_path(destdir, &path)? else {
            continue;
        };
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

fn normalized_licenses(licenses: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = licenses
        .iter()
        .map(|license| license.trim().to_string())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn licenses_match(left: &[String], right: &[String]) -> bool {
    normalized_licenses(left) == normalized_licenses(right)
}

fn supplemental_output_license_targets(
    spec: &PackageSpec,
    destdir: &Path,
) -> Vec<(crate::package::PackageInfo, PathBuf)> {
    let declared_names: HashSet<String> = spec
        .outputs()
        .into_iter()
        .map(|output| output.name)
        .collect();
    let mut targets = Vec::new();
    let mut seen = HashSet::new();

    for output in spec.outputs() {
        if output.name != spec.package.name {
            let out_destdir = output_staging_dir(destdir, &output.name);
            if out_destdir.exists() && seen.insert(output.name.clone()) {
                targets.push((output.clone(), out_destdir));
            }
        }

        if !spec.build.flags.split_docs || output.name.ends_with("-docs") {
            continue;
        }

        let docs_pkg = spec.docs_package_for_output(&output);
        if declared_names.contains(&docs_pkg.name) && !seen.contains(&docs_pkg.name) {
            let docs_destdir = output_staging_dir(destdir, &docs_pkg.name);
            if docs_destdir.exists() && seen.insert(docs_pkg.name.clone()) {
                targets.push((docs_pkg, docs_destdir));
            }
            continue;
        }

        let docs_destdir = output_staging_dir(destdir, &docs_pkg.name);
        if docs_destdir.exists() && seen.insert(docs_pkg.name.clone()) {
            targets.push((docs_pkg, docs_destdir));
        }
    }

    targets
}

/// Stage license payloads for split package outputs.
///
/// Outputs that share the primary package license reuse the primary license directory via symlink.
/// Outputs with distinct metadata licenses receive copied license files from `src_dir`.
pub fn stage_split_package_licenses(
    src_dir: &Path,
    destdir: &Path,
    spec: &PackageSpec,
) -> Result<()> {
    let primary_license_dir = destdir.join("usr/share/licenses").join(&spec.package.name);
    if !primary_license_dir.exists() {
        return Ok(());
    }

    for (output, output_destdir) in supplemental_output_license_targets(spec, destdir) {
        if licenses_match(&output.license, &spec.package.license) {
            symlink_package_license(&output_destdir, &output.name, &spec.package.name)?;
        } else {
            add_licenses(src_dir, &output_destdir, &output.name)?;
        }
    }

    Ok(())
}

/// Stage a package license directory as a symlink to another package's license directory.
pub fn symlink_package_license(destdir: &Path, pkgname: &str, target_pkgname: &str) -> Result<()> {
    let licenses_dir = destdir.join("usr/share/licenses");
    fs::create_dir_all(&licenses_dir)
        .with_context(|| format!("Failed to create {}", licenses_dir.display()))?;
    let link_path = licenses_dir.join(pkgname);

    match link_path.symlink_metadata() {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                && fs::read_link(&link_path)? == Path::new(target_pkgname)
            {
                return Ok(());
            }
            if metadata.file_type().is_dir() {
                fs::remove_dir_all(&link_path).with_context(|| {
                    format!(
                        "Failed to remove existing license dir {}",
                        link_path.display()
                    )
                })?;
            } else {
                fs::remove_file(&link_path).with_context(|| {
                    format!(
                        "Failed to remove existing license path {}",
                        link_path.display()
                    )
                })?;
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to inspect existing license path {}",
                    link_path.display()
                )
            });
        }
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target_pkgname, &link_path).with_context(|| {
            format!(
                "Failed to create package license symlink {} -> {}",
                link_path.display(),
                target_pkgname
            )
        })?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        anyhow::bail!(
            "Package license symlinks are only supported on unix hosts: {}",
            link_path.display()
        );
    }
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
        let stripped = auto_strip_elf_files(destdir, &spec.build.flags.strip)?;
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

    let moved_docs = split_docs_outputs(destdir, spec)?;
    if moved_docs > 0 {
        crate::log_info!(
            "Moved {} documentation tree(s) into docs outputs",
            moved_docs
        );
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

mod manifest;
mod transaction;

pub use manifest::*;
pub use transaction::*;

#[cfg(test)]
mod tests;
