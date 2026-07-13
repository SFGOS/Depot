use super::*;

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
