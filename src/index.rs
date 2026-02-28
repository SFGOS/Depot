//! Package index for fast lookups
//!
//! Caches package name -> spec path and provides -> spec path mappings.

use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// Filename for the source-repo index written at a repo root.
pub const SOURCE_REPO_INDEX_FILENAME: &str = "depot-index.tsv";
const SOURCE_REPO_INDEX_HEADER: &str = "depot-source-index-v1";
const SOURCE_REPO_INDEX_KIND_PACKAGE: &str = "P";
const SOURCE_REPO_INDEX_KIND_PROVIDES: &str = "V";

/// Statistics for generating a source repository index file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRepoIndexStats {
    /// Absolute path to the written index file.
    pub index_path: PathBuf,
    /// Number of `.toml` files discovered under the selected scan roots.
    pub toml_files_scanned: usize,
    /// Number of discovered TOML files that parsed as valid package specs.
    pub specs_indexed: usize,
    /// Number of package-name rows written to the index.
    pub package_rows: usize,
    /// Number of provides rows written to the index.
    pub provides_rows: usize,
    /// Number of discovered TOML files ignored because they were not package specs.
    pub ignored_toml_files: usize,
}

/// Cached package index for O(1) lookups
#[derive(Debug, Default)]
pub struct PackageIndex {
    /// Package name -> spec path
    by_name: HashMap<String, PathBuf>,
    /// Provided name -> spec paths (can be multiple)
    by_provides: HashMap<String, Vec<PathBuf>>,
}

/// Source package search result from `PackageIndex`.
#[derive(Debug, Clone)]
pub struct SourceSearchHit {
    pub name: String,
    pub path: PathBuf,
    pub provides: Vec<String>,
}

/// Return the source index file path for `repo_root`.
pub fn source_repo_index_path(repo_root: &Path) -> PathBuf {
    repo_root.join(SOURCE_REPO_INDEX_FILENAME)
}

/// Create/update the source-repo package index at `repo_root`.
///
/// The file format is line-based TSV with deterministic ordering:
/// - Header: `depot-source-index-v1`
/// - Package name row: `P<TAB><name><TAB><relative-spec-path>`
/// - Provides row: `V<TAB><feature><TAB><relative-spec-path>`
pub fn create_source_repo_index(
    repo_root: &Path,
    subdirs: &[String],
) -> Result<SourceRepoIndexStats> {
    let repo_root = repo_root
        .canonicalize()
        .with_context(|| format!("Failed to resolve repo root {}", repo_root.display()))?;
    if !repo_root.is_dir() {
        anyhow::bail!("Repo root is not a directory: {}", repo_root.display());
    }

    let scan_roots = resolve_scan_roots(&repo_root, subdirs)?;
    let mut spec_rows: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut toml_files_scanned = 0usize;
    let mut ignored_toml_files = 0usize;

    for spec_path in scan_toml_files(&scan_roots)? {
        toml_files_scanned += 1;
        let spec = match PackageSpec::from_file(&spec_path) {
            Ok(spec) => spec,
            Err(_) => {
                ignored_toml_files += 1;
                continue;
            }
        };

        let rel = spec_path.strip_prefix(&repo_root).with_context(|| {
            format!(
                "Failed to compute relative path for {} from {}",
                spec_path.display(),
                repo_root.display()
            )
        })?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        spec_rows.push((spec.package.name.clone(), rel, spec.alternatives.provides));
    }

    let mut rows: Vec<(String, String, String)> = Vec::new();
    for (name, rel, provides) in &spec_rows {
        rows.push((
            SOURCE_REPO_INDEX_KIND_PACKAGE.to_string(),
            name.clone(),
            rel.clone(),
        ));
        for provided in provides {
            rows.push((
                SOURCE_REPO_INDEX_KIND_PROVIDES.to_string(),
                provided.clone(),
                rel.clone(),
            ));
        }
    }
    rows.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut out = String::new();
    out.push_str(SOURCE_REPO_INDEX_HEADER);
    out.push('\n');
    for (kind, name, rel) in &rows {
        if name.contains('\n') || name.contains('\r') || name.contains('\t') {
            anyhow::bail!(
                "Index field contains unsupported control character: {}",
                name
            );
        }
        if rel.contains('\n') || rel.contains('\r') || rel.contains('\t') {
            anyhow::bail!("Index path contains unsupported control character: {}", rel);
        }
        out.push_str(kind);
        out.push('\t');
        out.push_str(name);
        out.push('\t');
        out.push_str(rel);
        out.push('\n');
    }

    let index_path = source_repo_index_path(&repo_root);
    let tmp_path = index_path.with_extension("tsv.tmp");
    fs::write(&tmp_path, out).with_context(|| format!("Failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &index_path).with_context(|| {
        format!(
            "Failed to replace index {} from {}",
            index_path.display(),
            tmp_path.display()
        )
    })?;

    let package_rows = rows
        .iter()
        .filter(|(kind, _, _)| kind == SOURCE_REPO_INDEX_KIND_PACKAGE)
        .count();
    let provides_rows = rows
        .iter()
        .filter(|(kind, _, _)| kind == SOURCE_REPO_INDEX_KIND_PROVIDES)
        .count();

    Ok(SourceRepoIndexStats {
        index_path,
        toml_files_scanned,
        specs_indexed: spec_rows.len(),
        package_rows,
        provides_rows,
        ignored_toml_files,
    })
}

impl PackageIndex {
    /// Build index by scanning packages/*/*.toml and configured repo dir.
    ///
    /// Use `build_with_repo_dir` to provide an explicit repo dir.
    /// Build index scanning the local `packages/` directory and an optional
    /// system repo dir (e.g., /usr/src/depot). If `repo_dir` is None, the
    /// default `/usr/src/depot` is used.
    pub fn build_with_repo_dir(repo_dir: Option<PathBuf>) -> Self {
        let mut index = Self::default();
        let packages_dir = PathBuf::from("packages");

        index.scan_spec_tree(&packages_dir);

        let sys_dir = repo_dir.unwrap_or_else(|| PathBuf::from("/usr/src/depot"));
        if sys_dir.exists() {
            index.scan_repo_store(&sys_dir);
        }

        crate::log_info!(
            "Indexed {} packages ({} provides)",
            index.by_name.len(),
            index.by_provides.len()
        );

        index
    }

    fn scan_repo_store(&mut self, sys_dir: &Path) {
        let direct_index = source_repo_index_path(sys_dir);
        if direct_index.exists() || sys_dir.join(".git").is_dir() {
            self.scan_repo_root(sys_dir);
            return;
        }

        let mut child_dirs = Vec::new();
        if let Ok(entries) = fs::read_dir(sys_dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                if ft.is_dir() {
                    child_dirs.push(entry.path());
                }
            }
        }

        if child_dirs.is_empty() {
            self.scan_spec_tree(sys_dir);
            return;
        }

        child_dirs.sort();
        for child in child_dirs {
            self.scan_repo_root(&child);
        }
    }

    fn scan_repo_root(&mut self, repo_root: &Path) {
        let index_path = source_repo_index_path(repo_root);
        if index_path.exists() {
            match self.load_repo_index(repo_root, &index_path) {
                Ok(_) => return,
                Err(err) => {
                    crate::log_warn!(
                        "Failed to read source index {} (falling back to TOML scan): {}",
                        index_path.display(),
                        err
                    );
                }
            }
        }
        self.scan_spec_tree(repo_root);
    }

    fn load_repo_index(&mut self, repo_root: &Path, index_path: &Path) -> Result<()> {
        let content = fs::read_to_string(index_path)
            .with_context(|| format!("Failed to read {}", index_path.display()))?;
        let mut lines = content.lines();
        let header = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing source index header"))?;
        if header.trim() != SOURCE_REPO_INDEX_HEADER {
            anyhow::bail!(
                "Unsupported source index header '{}' in {}",
                header,
                index_path.display()
            );
        }

        for (idx, raw) in lines.enumerate() {
            let line_no = idx + 2;
            let line = raw.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut parts = line.splitn(3, '\t');
            let kind = parts.next().unwrap_or_default();
            let name = parts.next().ok_or_else(|| {
                anyhow::anyhow!("Malformed line {} in {}", line_no, index_path.display())
            })?;
            let rel = parts.next().ok_or_else(|| {
                anyhow::anyhow!("Malformed line {} in {}", line_no, index_path.display())
            })?;
            if name.is_empty() || rel.is_empty() {
                anyhow::bail!("Malformed line {} in {}", line_no, index_path.display());
            }
            let path = repo_root.join(rel);
            match kind {
                SOURCE_REPO_INDEX_KIND_PACKAGE => {
                    self.by_name.insert(name.to_string(), path);
                }
                SOURCE_REPO_INDEX_KIND_PROVIDES => {
                    self.by_provides
                        .entry(name.to_string())
                        .or_default()
                        .push(path);
                }
                _ => {
                    anyhow::bail!(
                        "Unknown source index row type '{}' on line {} in {}",
                        kind,
                        line_no,
                        index_path.display()
                    );
                }
            }
        }

        Ok(())
    }

    fn scan_spec_tree(&mut self, root: &Path) {
        if !root.exists() {
            return;
        }

        let mut paths = Vec::new();
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                paths.push(path.to_path_buf());
            }
        }
        paths.sort();

        for path in paths {
            if let Ok(spec) = PackageSpec::from_file(&path) {
                self.add_spec(&spec, path);
            }
        }
    }

    fn add_spec(&mut self, spec: &PackageSpec, path: PathBuf) {
        self.by_name.insert(spec.package.name.clone(), path.clone());
        for provided in &spec.alternatives.provides {
            self.by_provides
                .entry(provided.clone())
                .or_default()
                .push(path.clone());
        }
    }

    /// Find a spec by package name or provides
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        // First try by name
        if let Some(path) = self.by_name.get(name) {
            return Some(path.clone());
        }

        // Then try by provides
        if let Some(paths) = self.by_provides.get(name) {
            if paths.len() > 1 {
                crate::log_warn!(
                    "Multiple packages provide '{}': {:?}",
                    name,
                    paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                );
            }
            return paths.first().cloned();
        }

        None
    }

    /// Return all source specs that provide the requested feature/package name.
    pub fn find_providers(&self, name: &str) -> Vec<PathBuf> {
        self.by_provides.get(name).cloned().unwrap_or_default()
    }

    /// Search indexed specs by package name or provided feature.
    pub fn search(&self, query: &str) -> Vec<SourceSearchHit> {
        let q = query.to_ascii_lowercase();
        let mut provides_by_path: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for (provided, paths) in &self.by_provides {
            for path in paths {
                provides_by_path
                    .entry(path.clone())
                    .or_default()
                    .push(provided.clone());
            }
        }

        let mut hits = Vec::new();
        for (name, path) in &self.by_name {
            let provides = provides_by_path.remove(path).unwrap_or_default();
            let name_match = name.to_ascii_lowercase().contains(&q);
            let provides_match = provides.iter().any(|p| p.to_ascii_lowercase().contains(&q));
            if name_match || provides_match {
                hits.push(SourceSearchHit {
                    name: name.clone(),
                    path: path.clone(),
                    provides,
                });
            }
        }

        hits.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        hits
    }
}

fn resolve_scan_roots(repo_root: &Path, subdirs: &[String]) -> Result<Vec<PathBuf>> {
    if subdirs.is_empty() {
        return Ok(vec![repo_root.to_path_buf()]);
    }

    let mut out = Vec::new();
    for subdir in subdirs {
        let trimmed = subdir.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Subdirectory entries cannot be empty");
        }
        let rel = Path::new(trimmed);
        if rel.is_absolute() || rel.components().any(|c| c == Component::ParentDir) {
            anyhow::bail!(
                "Subdirectory '{}' must be a relative path without '..'",
                trimmed
            );
        }
        let abs = repo_root.join(rel);
        if !abs.is_dir() {
            anyhow::bail!("Subdirectory not found: {}", abs.display());
        }
        out.push(abs);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn scan_toml_files(scan_roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for root in scan_roots {
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
        {
            let entry = entry.with_context(|| {
                format!("Failed walking repository tree under {}", root.display())
            })?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
                paths.push(path.to_path_buf());
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_meta_spec(path: &Path, name: &str, provides: &[&str]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let provides_line = if provides.is_empty() {
            String::new()
        } else {
            let quoted = provides
                .iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[alternatives]\nprovides = [{quoted}]\n\n")
        };
        let content = format!(
            "[package]\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"test\"\nhomepage = \"https://example.com\"\nlicense = \"MIT\"\n\n{provides_line}[build]\ntype = \"meta\"\n"
        );
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn create_source_repo_index_and_load_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("depot");
        let spec_path = repo_root.join("core/hello/hello.toml");
        write_meta_spec(&spec_path, "hello", &["sh"]);

        let stats = create_source_repo_index(&repo_root, &["core".to_string()]).unwrap();
        assert_eq!(stats.specs_indexed, 1);
        assert_eq!(stats.package_rows, 1);
        assert_eq!(stats.provides_rows, 1);
        assert!(stats.index_path.exists());

        let index = PackageIndex::build_with_repo_dir(Some(repo_root.clone()));
        let hit = index.find("hello").expect("package name should resolve");
        assert!(hit.ends_with(Path::new("core/hello/hello.toml")));
        let providers = index.find_providers("sh");
        assert_eq!(providers.len(), 1);
        assert!(providers[0].ends_with(Path::new("core/hello/hello.toml")));
    }

    #[test]
    fn build_with_repo_dir_falls_back_when_index_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_store = tmp.path().join("repo-store");
        let repo_root = repo_store.join("vertex");
        let spec_path = repo_root.join("core/base/base.toml");
        write_meta_spec(&spec_path, "base", &[]);

        let index = PackageIndex::build_with_repo_dir(Some(repo_store));
        let hit = index
            .find("base")
            .expect("package should be found by fallback TOML scanning");
        assert_eq!(hit, spec_path.canonicalize().unwrap());
    }

    #[test]
    fn malformed_index_file_falls_back_to_toml_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("depot");
        let spec_path = repo_root.join("core/curl/curl.toml");
        write_meta_spec(&spec_path, "curl", &["libcurl"]);

        std::fs::create_dir_all(&repo_root).unwrap();
        std::fs::write(source_repo_index_path(&repo_root), "invalid-header\n").unwrap();

        let index = PackageIndex::build_with_repo_dir(Some(repo_root));
        let hit = index
            .find("curl")
            .expect("fallback should still find package spec");
        assert_eq!(hit, spec_path.canonicalize().unwrap());
    }

    #[test]
    fn create_source_repo_index_rejects_parent_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = create_source_repo_index(tmp.path(), &["../core".to_string()])
            .expect_err("unsafe subdir should be rejected");
        assert!(
            err.to_string()
                .contains("must be a relative path without '..'")
        );
    }
}
