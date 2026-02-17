//! SQLite-based package database

pub mod repo;

use crate::package::PackageSpec;
use crate::staging;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::fs;
use std::path::Path;

/// Initialize database and register a package
pub fn register_package(db_path: &Path, spec: &PackageSpec, destdir: &Path) -> Result<()> {
    // Create parent directory (auto-create db dir if missing)
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut conn = Connection::open(db_path)?;
    init_db(&conn)?;

    let tx = conn.transaction()?;

    // Generate manifest with files and directories
    let manifest = staging::generate_manifest_with_dirs(destdir)?;

    // Insert/update package without changing its primary key (UPSERT keeps the existing row).
    tx.execute(
        "INSERT INTO packages (name, version, revision, description, homepage, license)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name) DO UPDATE SET
            version=excluded.version,
            revision=excluded.revision,
            description=excluded.description,
            homepage=excluded.homepage,
            license=excluded.license",
        params![
            spec.package.name,
            spec.package.version,
            spec.package.revision,
            spec.package.description,
            spec.package.homepage,
            spec.package.license,
        ],
    )?;

    let pkg_id: i64 = tx.query_row(
        "SELECT id FROM packages WHERE name = ?1",
        params![spec.package.name],
        |row| row.get(0),
    )?;

    // Replace provides + file + directory lists for this package.
    tx.execute(
        "DELETE FROM provides WHERE package_id = ?1",
        params![pkg_id],
    )?;
    tx.execute("DELETE FROM files WHERE package_id = ?1", params![pkg_id])?;
    tx.execute(
        "DELETE FROM directories WHERE package_id = ?1",
        params![pkg_id],
    )?;

    // Insert provides
    for provides in &spec.alternatives.provides {
        tx.execute(
            "INSERT OR IGNORE INTO provides (package_id, provides_name) VALUES (?1, ?2)",
            params![pkg_id, provides],
        )?;
    }

    // Detect ownership conflicts and separate into auto-removable vs fatal.
    let mut fatal_conflicts: Vec<(String, String)> = Vec::new();
    let mut auto_conflicts: Vec<(String, String)> = Vec::new();

    for file in &manifest.files {
        let owner_res: rusqlite::Result<String> = tx.query_row(
            "SELECT p.name FROM files f JOIN packages p ON f.package_id = p.id WHERE f.path = ?1",
            params![file],
            |row| row.get(0),
        );
        if let Ok(owner) = owner_res {
            if owner != spec.package.name {
                if is_auto_removable_path(file) {
                    auto_conflicts.push((file.clone(), owner));
                } else {
                    fatal_conflicts.push((file.clone(), owner));
                }
            }
        }
    }

    if !fatal_conflicts.is_empty() {
        let mut msg = String::from("File ownership conflict detected:\n");
        for (f, owner) in &fatal_conflicts {
            msg.push_str(&format!("  {} -> owned by {}\n", f, owner));
        }
        anyhow::bail!("{}", msg);
    }

    // For auto-removable conflicts, remove previous DB ownership entries and
    // attempt to delete the on-disk file if we can infer the rootfs.
    if !auto_conflicts.is_empty() {
        let rootfs_opt = detect_rootfs_from_db_path(db_path);
        for (f, owner) in &auto_conflicts {
            // Remove DB row(s) marking the previous owner for this path
            tx.execute(
                "DELETE FROM files WHERE path = ?1 AND package_id = (SELECT id FROM packages WHERE name = ?2)",
                params![f, owner],
            )?;

            if let Some(rootfs) = &rootfs_opt {
                let disk_path = rootfs.join(f);
                if disk_path.exists() {
                    let _ = std::fs::remove_file(&disk_path);
                    println!(
                        "Auto-removed conflicting path: {} (was owned by {})",
                        f, owner
                    );
                }
            } else {
                println!(
                    "Auto-cleared DB ownership for path: {} (previously owned by {})",
                    f, owner
                );
            }
        }
    }

    // Insert files (no fatal conflicts remain)
    for file in &manifest.files {
        tx.execute(
            "INSERT INTO files (package_id, path) VALUES (?1, ?2)",
            params![pkg_id, file],
        )?;
    }

    // Insert directories (can be shared by multiple packages)
    for dir in &manifest.directories {
        tx.execute(
            "INSERT INTO directories (package_id, path) VALUES (?1, ?2)",
            params![pkg_id, dir],
        )?;
    }

    tx.commit()?;

    println!(
        "Registered {} files and {} directories in database",
        manifest.files.len(),
        manifest.directories.len()
    );
    Ok(())
}

/// Return the list of files owned by an installed package.
pub fn get_package_files(db_path: &Path, name: &str) -> Result<Vec<String>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(db_path)?;

    // If the package is not present in the DB (fresh install), treat that as
    // "no previously installed files" rather than an error.
    let pkg_id_res: rusqlite::Result<i64> = conn.query_row(
        "SELECT id FROM packages WHERE name = ?1",
        params![name],
        |row| row.get(0),
    );

    let pkg_id = match pkg_id_res {
        Ok(id) => id,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut stmt = conn.prepare("SELECT path FROM files WHERE package_id = ?1")?;
    let files: Vec<String> = stmt
        .query_map(params![pkg_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(files)
}

/// Remove a package from the database and filesystem
pub fn remove_package(db_path: &Path, name: &str, rootfs: &Path) -> Result<()> {
    if !db_path.exists() {
        anyhow::bail!("Package database not found");
    }

    let conn = Connection::open(db_path)?;

    // Get package ID
    let pkg_id: i64 = conn
        .query_row(
            "SELECT id FROM packages WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .context(format!("Package '{}' not found", name))?;

    // Get file list
    let mut stmt = conn.prepare("SELECT path FROM files WHERE package_id = ?1")?;
    let files: Vec<String> = stmt
        .query_map(params![pkg_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    // Get directory list (sorted deepest-first for proper removal order)
    let mut stmt = conn.prepare("SELECT path FROM directories WHERE package_id = ?1")?;
    let mut directories: Vec<String> = stmt
        .query_map(params![pkg_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    directories.sort_by_key(|b| std::cmp::Reverse(b.matches('/').count()));

    // Remove files
    let mut removal_errors: Vec<String> = Vec::new();
    for file in &files {
        let path = rootfs.join(file);
        match fs::remove_file(&path) {
            Ok(()) => {
                println!("  Removed file: {}", file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone, keep going.
            }
            Err(e) => {
                removal_errors.push(format!("{}: {}", file, e));
            }
        }
    }

    // Remove directories (only if empty and not owned by another package)
    let mut dirs_removed = 0;
    for dir in &directories {
        let path = rootfs.join(dir);

        // Skip if outside rootfs
        if !path.starts_with(rootfs) {
            continue;
        }

        // Check if directory is owned by another package
        let other_owners: i64 = conn.query_row(
            "SELECT COUNT(*) FROM directories WHERE path = ?1 AND package_id != ?2",
            params![dir, pkg_id],
            |row| row.get(0),
        )?;

        if other_owners > 0 {
            println!("  Keeping directory (owned by other package): {}", dir);
            continue;
        }

        // Try to remove (will fail if not empty, which is fine)
        match fs::remove_dir(&path) {
            Ok(()) => {
                println!("  Removed directory: {}", dir);
                dirs_removed += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone
            }
            Err(e) if e.raw_os_error() == Some(39) || e.raw_os_error() == Some(66) => {
                // ENOTEMPTY (39 on Linux, 66 on macOS) - directory not empty
                println!("  Keeping directory (not empty): {}", dir);
            }
            Err(_) => {
                // Other errors (permission, etc.) - just skip silently
            }
        }
    }

    // Remove from database
    conn.execute("DELETE FROM files WHERE package_id = ?1", params![pkg_id])?;
    conn.execute(
        "DELETE FROM directories WHERE package_id = ?1",
        params![pkg_id],
    )?;
    conn.execute(
        "DELETE FROM provides WHERE package_id = ?1",
        params![pkg_id],
    )?;
    conn.execute("DELETE FROM packages WHERE id = ?1", params![pkg_id])?;

    println!(
        "Removed {} files and {} directories",
        files.len(),
        dirs_removed
    );

    if !removal_errors.is_empty() {
        eprintln!("Warning: failed to remove some paths:");
        for err in removal_errors {
            eprintln!("  {}", err);
        }
    }
    Ok(())
}

/// Show information about an installed package
pub fn show_package_info(db_path: &Path, name: &str) -> Result<()> {
    if !db_path.exists() {
        anyhow::bail!("Package database not found");
    }

    let conn = Connection::open(db_path)?;

    let (version, revision, description, homepage, license): (String, u32, String, String, String) = conn
        .query_row(
            "SELECT version, revision, description, homepage, license FROM packages WHERE name = ?1",
            params![name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .context(format!("Package '{}' not found", name))?;

    println!("Package: {} v{}-{}", name, version, revision);
    println!("Description: {}", description);
    println!("Homepage: {}", homepage);
    println!("License: {}", license);

    // Count files
    let file_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files f JOIN packages p ON f.package_id = p.id WHERE p.name = ?1",
        params![name],
        |row| row.get(0),
    )?;

    println!("Files: {}", file_count);

    Ok(())
}

/// List all installed packages
pub fn list_packages(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        println!("No packages installed.");
        return Ok(());
    }

    let conn = Connection::open(db_path)?;

    let mut stmt = conn.prepare("SELECT name, version FROM packages ORDER BY name")?;
    let packages = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    println!("{:<30} VERSION", "PACKAGE");
    println!("{}", "-".repeat(50));

    for pkg in packages {
        let (name, version) = pkg?;
        println!("{:<30} {}", name, version);
    }

    Ok(())
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS packages (
            id INTEGER PRIMARY KEY,
            name TEXT UNIQUE NOT NULL,
            version TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            description TEXT,
            homepage TEXT,
            license TEXT
        );

        CREATE TABLE IF NOT EXISTS provides (
            id INTEGER PRIMARY KEY,
            package_id INTEGER NOT NULL,
            provides_name TEXT NOT NULL,
            FOREIGN KEY (package_id) REFERENCES packages(id),
            UNIQUE(package_id, provides_name)
        );

        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            package_id INTEGER NOT NULL,
            path TEXT NOT NULL,
            FOREIGN KEY (package_id) REFERENCES packages(id),
            UNIQUE(path)
        );

        CREATE TABLE IF NOT EXISTS directories (
            id INTEGER PRIMARY KEY,
            package_id INTEGER NOT NULL,
            path TEXT NOT NULL,
            FOREIGN KEY (package_id) REFERENCES packages(id)
        );

        CREATE INDEX IF NOT EXISTS idx_files_package ON files(package_id);
        CREATE INDEX IF NOT EXISTS idx_provides_name ON provides(provides_name);
        CREATE INDEX IF NOT EXISTS idx_directories_package ON directories(package_id);
        CREATE INDEX IF NOT EXISTS idx_directories_path ON directories(path);
        ",
    )?;
    Ok(())
}

/// Decide whether a conflicting path is safe to auto-remove ownership for.
/// This covers shared index files (e.g. `usr/share/info/dir`) and common
/// language-shared trees such as Perl site/vendor/lib directories.
fn is_auto_removable_path(path: &str) -> bool {
    let p = path.trim_start_matches('/');

    // Exact info index file (and compressed variants)
    if p == "usr/share/info/dir" || p.starts_with("usr/share/info/dir.") {
        return true;
    }

    // Perl shared trees (common multi-package locations)
    if p.starts_with("usr/lib/perl")
        || p.starts_with("usr/share/perl")
        || p.starts_with("usr/lib/perl5")
        || p.starts_with("usr/share/perl5")
    {
        return true;
    }

    false
}

/// Attempt to infer the rootfs path from the database path. If the DB path
/// follows the standard layout `<rootfs>/var/lib/depot/packages.db`, return
/// the `<rootfs>` portion. Otherwise return None.
fn detect_rootfs_from_db_path(db_path: &Path) -> Option<std::path::PathBuf> {
    // Look for the suffix `var/lib/depot/packages.db` and return the ancestor 4 levels up
    if db_path.ends_with(std::path::Path::new("var/lib/depot/packages.db")) {
        return db_path.ancestors().nth(4).map(|p| p.to_path_buf());
    }
    None
}

/// Calculate which files need to be removed during an upgrade.
/// Returns paths that exist in the old version but NOT in the new version.
pub fn calculate_upgrade_paths(
    db_path: &Path,
    name: &str,
    new_files: &[String],
) -> Result<Vec<String>> {
    let old_files = get_package_files(db_path, name)?;
    let mut new_set = std::collections::HashSet::new();
    for f in new_files {
        new_set.insert(f);
    }

    let remove_paths: Vec<String> = old_files
        .into_iter()
        .filter(|p| !new_set.contains(p))
        .collect();

    Ok(remove_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
    };
    use std::path::PathBuf;

    fn mk_spec(name: &str, version: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: name.into(),
                version: version.into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: "MIT".into(),
            },
            packages: Vec::new(),
            alternatives: Alternatives {
                provides: vec![format!("{}-virtual", name)],
                replaces: Vec::new(),
            },
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn register_package_updates_in_place_and_replaces_file_list() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");

        let spec_v1 = mk_spec("foo", "1.0");
        let dest1 = tmp.path().join("dest1");
        std::fs::create_dir_all(dest1.join("usr/bin")).unwrap();
        std::fs::write(dest1.join("usr/bin/foo"), "v1").unwrap();

        register_package(&db_path, &spec_v1, &dest1).unwrap();

        // Capture package id
        let conn = Connection::open(&db_path).unwrap();
        let id1: i64 = conn
            .query_row(
                "SELECT id FROM packages WHERE name = ?1",
                params!["foo"],
                |r| r.get(0),
            )
            .unwrap();

        // Update with different file set
        let spec_v2 = mk_spec("foo", "2.0");
        let dest2 = tmp.path().join("dest2");
        std::fs::create_dir_all(dest2.join("usr/bin")).unwrap();
        std::fs::write(dest2.join("usr/bin/foo"), "v2").unwrap();
        std::fs::write(dest2.join("usr/bin/new_only"), "x").unwrap();

        register_package(&db_path, &spec_v2, &dest2).unwrap();

        let id2: i64 = conn
            .query_row(
                "SELECT id FROM packages WHERE name = ?1",
                params!["foo"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(id1, id2);

        let files = get_package_files(&db_path, "foo").unwrap();
        assert!(files.contains(&"usr/bin/foo".to_string()));
        assert!(files.contains(&"usr/bin/new_only".to_string()));

        let version = get_package_version(&db_path, "foo").unwrap();
        assert_eq!(version.as_deref(), Some("2.0"));
    }

    #[test]
    fn register_package_detects_conflicting_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");

        // Install package 'alpha' owning usr/bin/shared
        let spec_a = mk_spec("alpha", "1.0");
        let dest_a = tmp.path().join("dest_a");
        std::fs::create_dir_all(dest_a.join("usr/bin")).unwrap();
        std::fs::write(dest_a.join("usr/bin/shared"), "a").unwrap();
        register_package(&db_path, &spec_a, &dest_a).unwrap();

        // Try to install package 'beta' that also includes the same path -> should fail
        let spec_b = mk_spec("beta", "1.0");
        let dest_b = tmp.path().join("dest_b");
        std::fs::create_dir_all(dest_b.join("usr/bin")).unwrap();
        std::fs::write(dest_b.join("usr/bin/shared"), "b").unwrap();

        let res = register_package(&db_path, &spec_b, &dest_b);
        assert!(res.is_err());
        let err = format!("{}", res.err().unwrap());
        assert!(err.contains("File ownership conflict detected"));
        assert!(err.contains("usr/bin/shared"));
        assert!(err.contains("alpha"));
    }

    #[test]
    fn register_package_auto_clears_safe_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");

        // Install package 'alpha' owning usr/share/info/dir
        let spec_a = mk_spec("alpha", "1.0");
        let dest_a = tmp.path().join("dest_a");
        std::fs::create_dir_all(dest_a.join("usr/share/info")).unwrap();
        std::fs::write(dest_a.join("usr/share/info/dir"), "index").unwrap();
        register_package(&db_path, &spec_a, &dest_a).unwrap();

        // Now install package 'beta' that also provides usr/share/info/dir -> should auto-clear
        let spec_b = mk_spec("beta", "1.0");
        let dest_b = tmp.path().join("dest_b");
        std::fs::create_dir_all(dest_b.join("usr/share/info")).unwrap();
        std::fs::write(dest_b.join("usr/share/info/dir"), "index2").unwrap();

        // This should succeed and transfer ownership of the 'dir' path to beta
        register_package(&db_path, &spec_b, &dest_b).unwrap();

        // Verify DB: alpha should no longer own the path, beta should
        let files_a = get_package_files(&db_path, "alpha").unwrap();
        assert!(!files_a.contains(&"usr/share/info/dir".to_string()));
        let files_b = get_package_files(&db_path, "beta").unwrap();
        assert!(files_b.contains(&"usr/share/info/dir".to_string()));
    }

    #[test]
    fn get_package_files_missing_package_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");

        // Create an empty database file with schema but no packages
        let conn = Connection::open(&db_path).unwrap();
        init_db(&conn).unwrap();
        drop(conn);

        // Querying files for a package that doesn't exist should return an empty list
        let files = get_package_files(&db_path, "nonexistent").unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn remove_package_tolerates_missing_files_and_cleans_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let rootfs = tmp.path().join("root");
        std::fs::create_dir_all(&rootfs).unwrap();

        let spec = mk_spec("foo", "1.0");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
        std::fs::write(dest.join("usr/bin/foo"), "bin").unwrap();
        register_package(&db_path, &spec, &dest).unwrap();

        // Create the installed file in rootfs (one real)
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::fs::write(rootfs.join("usr/bin/foo"), "bin").unwrap();

        // Inject an extra missing file into DB to ensure we tolerate it.
        let conn = Connection::open(&db_path).unwrap();
        let pkg_id: i64 = conn
            .query_row(
                "SELECT id FROM packages WHERE name = ?1",
                params!["foo"],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO files (package_id, path) VALUES (?1, ?2)",
            params![pkg_id, "usr/bin/does_not_exist"],
        )
        .unwrap();

        remove_package(&db_path, "foo", &rootfs).unwrap();
        assert!(get_package_version(&db_path, "foo").unwrap().is_none());
    }

    #[test]
    fn test_package_upgrade_removes_orphaned_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let rootfs = tmp.path().join("root");
        let tx_base = tmp.path().join("tx");
        std::fs::create_dir_all(&rootfs).unwrap();

        // 1. Install v1: usr/bin/foo, usr/bin/shared_dir/old_file
        let spec_v1 = mk_spec("foo", "1.0");
        let dest1 = tmp.path().join("dest1");
        std::fs::create_dir_all(dest1.join("usr/bin/shared_dir")).unwrap();
        std::fs::write(dest1.join("usr/bin/foo"), "v1").unwrap();
        std::fs::write(dest1.join("usr/bin/shared_dir/old_file"), "old").unwrap();

        register_package(&db_path, &spec_v1, &dest1).unwrap();
        let _ = crate::staging::install_atomic(&dest1, &rootfs, &tx_base, &[]).unwrap();

        assert!(rootfs.join("usr/bin/foo").exists());
        assert!(rootfs.join("usr/bin/shared_dir/old_file").exists());

        // 2. Prepare v2: usr/bin/foo (updated), usr/bin/new_file
        // (shared_dir/old_file is removed from spec)
        let spec_v2 = mk_spec("foo", "2.0");
        let dest2 = tmp.path().join("dest2");
        std::fs::create_dir_all(dest2.join("usr/bin")).unwrap();
        std::fs::write(dest2.join("usr/bin/foo"), "v2").unwrap();
        std::fs::write(dest2.join("usr/bin/new_file"), "new").unwrap();

        let manifest2 = crate::staging::generate_manifest_with_dirs(&dest2).unwrap();
        let remove_paths = calculate_upgrade_paths(&db_path, "foo", &manifest2.files).unwrap();

        assert_eq!(
            remove_paths,
            vec!["usr/bin/shared_dir/old_file".to_string()]
        );

        let tx = crate::staging::install_atomic(&dest2, &rootfs, &tx_base, &remove_paths).unwrap();
        register_package(&db_path, &spec_v2, &dest2).unwrap();
        tx.commit().unwrap();

        // 3. Verify filesystem
        assert_eq!(
            std::fs::read_to_string(rootfs.join("usr/bin/foo")).unwrap(),
            "v2"
        );
        assert!(rootfs.join("usr/bin/new_file").exists());
        assert!(!rootfs.join("usr/bin/shared_dir/old_file").exists());

        // Check DB
        let files = get_package_files(&db_path, "foo").unwrap();
        assert!(files.contains(&"usr/bin/foo".to_string()));
        assert!(files.contains(&"usr/bin/new_file".to_string()));
        assert!(!files.contains(&"usr/bin/shared_dir/old_file".to_string()));
    }
}

/// Get set of all installed package names
pub fn get_installed_packages(db_path: &Path) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;

    let conn = Connection::open(db_path)?;
    let mut stmt = conn.prepare("SELECT name FROM packages")?;
    let names: HashSet<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

/// Get set of all provided package names (alternatives)
pub fn get_all_provides(db_path: &Path) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;

    let conn = Connection::open(db_path)?;
    let mut stmt = conn.prepare("SELECT provides_name FROM provides")?;
    let names: HashSet<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

/// Get version of a specific installed package
pub fn get_package_version(db_path: &Path, name: &str) -> Result<Option<String>> {
    let conn = Connection::open(db_path)?;
    let version: Option<String> = conn
        .query_row(
            "SELECT version FROM packages WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .ok();
    Ok(version)
}
