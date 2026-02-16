//! Package index for fast lookups
//!
//! Caches package name -> spec path and provides -> spec path mappings.

use crate::package::PackageSpec;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Cached package index for O(1) lookups
#[derive(Debug, Default)]
pub struct PackageIndex {
    /// Package name -> spec path
    by_name: HashMap<String, PathBuf>,
    /// Provided name -> spec paths (can be multiple)
    by_provides: HashMap<String, Vec<PathBuf>>,
}

impl PackageIndex {
    /// Build index by scanning packages/*/*.toml
    pub fn build() -> Self {
        let mut index = Self::default();
        let packages_dir = PathBuf::from("packages");

        if !packages_dir.exists() {
            return index;
        }

        // Scan all package directories
        if let Ok(entries) = fs::read_dir(&packages_dir) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let dir = entry.path();

                // Find all .toml files in this directory
                if let Ok(files) = fs::read_dir(&dir) {
                    for file in files.flatten() {
                        let path = file.path();
                        if path.extension().map(|e| e == "toml").unwrap_or(false) {
                            // Try to parse the spec
                            if let Ok(spec) = PackageSpec::from_file(&path) {
                                // Index by name
                                index
                                    .by_name
                                    .insert(spec.package.name.clone(), path.clone());

                                // Index by provides
                                for provided in &spec.alternatives.provides {
                                    index
                                        .by_provides
                                        .entry(provided.clone())
                                        .or_default()
                                        .push(path.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        println!(
            "Indexed {} packages ({} provides)",
            index.by_name.len(),
            index.by_provides.len()
        );

        index
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
                eprintln!(
                    "Warning: Multiple packages provide '{}': {:?}",
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
}
