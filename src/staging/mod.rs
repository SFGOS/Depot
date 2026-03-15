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
        if dst.symlink_metadata().is_ok() {
            anyhow::bail!(
                "Failed to move {} into {}: destination already exists",
                src.display(),
                dst.display()
            );
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

#[derive(Debug)]
pub struct FsTransaction {
    rootfs: PathBuf,
    tx_dir: PathBuf,
    backed_up: Vec<String>,
    created: Vec<String>,
    relocated: Vec<String>,
    removed: Vec<String>,
}

fn is_directory_empty(path: &Path) -> Result<bool> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("Failed to read directory {}", path.display()))?;
    Ok(entries.next().transpose()?.is_none())
}

fn backup_existing_path(src: &Path, backup_path: &Path, rel: &str) -> Result<()> {
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

fn move_directory_contents(src_dir: &Path, dst_dir: &Path) -> Result<()> {
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

fn copy_tree_preserving_layout_no_overwrite(
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

        if dst_path.symlink_metadata().is_ok() {
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

fn remove_path_in_place(path: &Path, rel: &str) -> Result<()> {
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

fn backup_and_remove_obsolete_path(
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

fn remove_obsolete_children_for_dir(
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
    let remove_set: HashSet<&str> = remove_paths.iter().map(String::as_str).collect();

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
        relocated: Vec::new(),
        removed: Vec::new(),
    };
    let mut staged_paths = HashSet::new();

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
                    if !remove_set.contains(install_rel_path.as_str()) {
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
                    if !remove_set.contains(install_rel_path.as_str()) {
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
                let target = fs::read_link(src_path)?;
                std::os::unix::fs::symlink(target, &dest_path)
                    .with_context(|| format!("Failed to create symlink: {}", install_rel_path))?;
                tx.replay_relocated_dir_if_present(&install_rel_path)?;
            } else {
                fs::copy(src_path, &dest_path)
                    .with_context(|| format!("Failed to install: {}", install_rel_path))?;
                apply_unix_mode(&dest_path, &metadata)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};
    use std::io::Read;

    fn mk_spec_for_stage_processing() -> PackageSpec {
        let flags = BuildFlags {
            no_strip: true,
            no_compress_man: true,
            ..BuildFlags::default()
        };
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
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
    fn process_splits_docs_into_docs_output() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/share/doc/foo")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/share/gtk-doc/html/foo")).unwrap();
        std::fs::create_dir_all(destdir.join("opt/foo-docs")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join("usr/share/doc/foo/README"), "doc").unwrap();
        std::fs::write(destdir.join("usr/share/gtk-doc/html/foo/index.html"), "gtk").unwrap();
        std::fs::write(destdir.join("opt/foo-docs/guide.txt"), "guide").unwrap();
        std::fs::write(destdir.join("usr/bin/foo"), "bin").unwrap();

        let mut spec = mk_spec_for_stage_processing();
        spec.build.flags.split_docs = true;
        spec.build.flags.doc_dirs = vec!["/opt/foo-docs".to_string()];

        process(&destdir, &spec).unwrap();

        let docs_destdir = output_staging_dir(&destdir, "foo-docs");
        assert!(docs_destdir.join("usr/share/doc/foo/README").exists());
        assert!(
            docs_destdir
                .join("usr/share/gtk-doc/html/foo/index.html")
                .exists()
        );
        assert!(docs_destdir.join("opt/foo-docs/guide.txt").exists());
        assert!(destdir.join("usr/bin/foo").exists());
        assert!(!destdir.join("usr/share/doc/foo/README").exists());
        assert!(
            !destdir
                .join("usr/share/gtk-doc/html/foo/index.html")
                .exists()
        );
        assert!(!destdir.join("opt/foo-docs/guide.txt").exists());
    }

    #[test]
    fn process_splits_docs_for_additional_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let destdir = tmp.path().join("dest");
        let dev_destdir = output_staging_dir(&destdir, "foo-dev");
        std::fs::create_dir_all(dev_destdir.join("usr/share/doc/foo-dev")).unwrap();
        std::fs::create_dir_all(dev_destdir.join("usr/include")).unwrap();
        std::fs::write(dev_destdir.join("usr/share/doc/foo-dev/README"), "doc").unwrap();
        std::fs::write(dev_destdir.join("usr/include/foo.h"), "header").unwrap();

        let mut spec = mk_spec_for_stage_processing();
        spec.packages.push(PackageInfo {
            name: "foo-dev".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "dev".into(),
            homepage: "h".into(),
            abi_breaking: false,
            license: vec!["MIT".into()],
        });
        spec.build.flags.split_docs = true;

        process(&destdir, &spec).unwrap();

        let docs_destdir = output_staging_dir(&destdir, "foo-dev-docs");
        assert!(docs_destdir.join("usr/share/doc/foo-dev/README").exists());
        assert!(dev_destdir.join("usr/include/foo.h").exists());
        assert!(!dev_destdir.join("usr/share/doc/foo-dev/README").exists());
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
    fn compress_manpages_zstd_detects_split_output_payload_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let page = output_staging_dir(&dest, "clang").join("usr/share/man/man1/clang.1");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        std::fs::write(&page, b"clang manpage\n").unwrap();

        let count = compress_manpages_zstd(&dest).unwrap();
        assert_eq!(count, 1);
        assert!(!page.exists());

        let compressed = page.with_extension("1.zst");
        assert!(compressed.exists());
        let encoded = std::fs::read(&compressed).unwrap();
        let decoded = zstd::stream::decode_all(std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "clang manpage\n");
    }

    #[test]
    fn stage_split_package_licenses_symlinks_matching_outputs_and_copies_distinct_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();
        std::fs::write(src_dir.join("LICENSE"), "license text").unwrap();
        add_licenses(&src_dir, &destdir, "foo").unwrap();

        let mut spec = mk_spec_for_stage_processing();
        spec.packages.push(PackageInfo {
            name: "foo-dev".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "dev".into(),
            homepage: "h".into(),
            abi_breaking: false,
            license: vec!["MIT".into()],
        });
        spec.packages.push(PackageInfo {
            name: "foo-extras".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "extras".into(),
            homepage: "h".into(),
            abi_breaking: false,
            license: vec!["Apache-2.0".into()],
        });

        let dev_dest = output_staging_dir(&destdir, "foo-dev").join("usr/bin");
        let extras_dest = output_staging_dir(&destdir, "foo-extras").join("usr/bin");
        std::fs::create_dir_all(&dev_dest).unwrap();
        std::fs::create_dir_all(&extras_dest).unwrap();
        std::fs::write(dev_dest.join("foo-dev"), "bin").unwrap();
        std::fs::write(extras_dest.join("foo-extras"), "bin").unwrap();

        stage_split_package_licenses(&src_dir, &destdir, &spec).unwrap();

        let dev_license =
            output_staging_dir(&destdir, "foo-dev").join("usr/share/licenses/foo-dev");
        let dev_meta = std::fs::symlink_metadata(&dev_license).unwrap();
        assert!(dev_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&dev_license).unwrap(),
            PathBuf::from("foo")
        );

        let extras_license =
            output_staging_dir(&destdir, "foo-extras").join("usr/share/licenses/foo-extras");
        let extras_meta = std::fs::symlink_metadata(&extras_license).unwrap();
        assert!(extras_meta.is_dir());
        let mut text = String::new();
        std::fs::File::open(extras_license.join("LICENSE"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "license text");
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
    #[cfg(unix)]
    fn install_atomic_replaces_existing_symlink_without_touching_target_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();
        std::fs::write(rootfs.join("usr/bin/existing"), "keep-me").unwrap();
        std::os::unix::fs::symlink("usr/bin", rootfs.join("bin")).unwrap();
        std::os::unix::fs::symlink("usr/bin", destdir.join("bin")).unwrap();

        let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

        assert_eq!(
            std::fs::read_link(rootfs.join("bin")).unwrap(),
            PathBuf::from("usr/bin")
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/existing")).unwrap(),
            "keep-me"
        );

        tx.rollback().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn install_atomic_rejects_replacing_directory_with_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
        std::fs::create_dir_all(destdir.join("usr")).unwrap();
        std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

        let err = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap_err();
        assert!(
            err.to_string()
                .contains("Refusing to replace existing directory with packaged file/symlink")
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_atomic_replaces_obsolete_directory_with_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::create_dir_all(destdir.join("usr")).unwrap();
        std::fs::write(rootfs.join("usr/sbin/legacy"), "old").unwrap();
        std::fs::write(destdir.join("usr/bin/legacy"), "new").unwrap();
        std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

        let remove_paths = vec!["usr/sbin/legacy".to_string(), "usr/sbin".to_string()];
        let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

        let sbin_meta = rootfs.join("usr/sbin").symlink_metadata().unwrap();
        assert!(sbin_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(rootfs.join("usr/sbin")).unwrap(),
            PathBuf::from("bin")
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/legacy")).unwrap(),
            "new"
        );

        tx.rollback().unwrap();
        let restored = rootfs.join("usr/sbin").symlink_metadata().unwrap();
        assert!(restored.file_type().is_dir());
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/sbin/legacy")).unwrap(),
            "old"
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_atomic_preserves_non_obsolete_directory_contents_when_replacing_with_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::create_dir_all(destdir.join("usr")).unwrap();
        std::fs::write(rootfs.join("usr/sbin/keep"), "keep-me").unwrap();
        std::fs::write(rootfs.join("usr/sbin/legacy"), "old").unwrap();
        std::fs::write(destdir.join("usr/bin/legacy"), "new").unwrap();
        std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

        let remove_paths = vec!["usr/sbin/legacy".to_string(), "usr/sbin".to_string()];
        let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

        let sbin_meta = rootfs.join("usr/sbin").symlink_metadata().unwrap();
        assert!(sbin_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/keep")).unwrap(),
            "keep-me"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/sbin/keep")).unwrap(),
            "keep-me"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/legacy")).unwrap(),
            "new"
        );

        tx.rollback().unwrap();
        let restored = rootfs.join("usr/sbin").symlink_metadata().unwrap();
        assert!(restored.file_type().is_dir());
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/sbin/keep")).unwrap(),
            "keep-me"
        );
        assert!(!rootfs.join("usr/bin/keep").exists());
    }

    #[test]
    #[cfg(unix)]
    fn install_atomic_rejects_symlink_swap_when_relocated_contents_conflict_with_target() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let destdir = tmp.path().join("dest");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::fs::create_dir_all(destdir.join("usr")).unwrap();
        std::fs::write(rootfs.join("usr/sbin/keep"), "keep-me").unwrap();
        std::fs::write(rootfs.join("usr/bin/keep"), "target-conflict").unwrap();
        std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

        let remove_paths = vec!["usr/sbin".to_string()];
        let err = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to replay relocated path into")
        );

        let restored = rootfs.join("usr/sbin").symlink_metadata().unwrap();
        assert!(restored.file_type().is_dir());
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/sbin/keep")).unwrap(),
            "keep-me"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/keep")).unwrap(),
            "target-conflict"
        );
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
