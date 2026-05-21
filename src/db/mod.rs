//! SQLite-based package database

pub mod repo;

use crate::package::PackageSpec;
use crate::staging;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

const DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS: &str = "DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS";

fn format_licenses(licenses: &[String]) -> String {
    licenses.join(", ")
}

fn verbose_remove_output() -> bool {
    std::env::var_os("DEPOT_VERBOSE_REMOVE").is_some()
}

fn should_ignore_sbase_conflicts() -> bool {
    std::env::var_os(DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS).is_some()
}

fn should_auto_clear_conflict(owner: &str, path: &str) -> bool {
    (owner == "sbase" && should_ignore_sbase_conflicts()) || is_auto_removable_path(path)
}

/// Installed package row from the local package database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackageRecord {
    /// Installed package name.
    pub name: String,
    /// Stable package stream name used for renamed updates.
    pub real_name: Option<String>,
    /// Installed package version.
    pub version: String,
    /// Installed package revision.
    pub revision: u32,
    /// Whether renamed updates should retain versioned shared libraries.
    pub abi_breaking: bool,
    /// Package completion timestamp if known.
    pub completed_at: Option<i64>,
}

impl InstalledPackageRecord {
    /// Return the stable package stream name, defaulting to the package name.
    pub fn effective_real_name(&self) -> &str {
        self.real_name.as_deref().unwrap_or(&self.name)
    }
}

/// Rename-aware replacement metadata applied while registering a new package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageReplacement {
    /// Installed package name being replaced.
    pub old_name: String,
    /// Subset of old-package files to keep installed and owned by the old package.
    pub retained_files: Vec<String>,
    /// Subset of old-package directories to keep owned by the old package.
    pub retained_directories: Vec<String>,
}

impl PackageReplacement {
    /// Return true when the replaced package remains installed after registration.
    pub fn retains_old_package(&self) -> bool {
        !self.retained_files.is_empty() || !self.retained_directories.is_empty()
    }
}

/// Initialize database and register a package
pub fn register_package(db_path: &Path, spec: &PackageSpec, destdir: &Path) -> Result<()> {
    register_package_with_replacement(db_path, spec, destdir, None)
}

/// Initialize database and register a package, optionally replacing a renamed predecessor.
pub fn register_package_with_replacement(
    db_path: &Path,
    spec: &PackageSpec,
    destdir: &Path,
    replacement: Option<&PackageReplacement>,
) -> Result<()> {
    // Create parent directory (auto-create db dir if missing)
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut conn = Connection::open(db_path)?;
    init_db(&conn)?;

    let tx = conn.transaction()?;
    let completed_at = package_completed_at(destdir)?;

    // Generate manifest with files and directories
    let manifest = staging::generate_manifest_with_dirs(destdir)?;

    if let Some(replacement) = replacement {
        if replacement.old_name == spec.package.name {
            anyhow::bail!(
                "Replacement package '{}' cannot match new package name",
                spec.package.name
            );
        }

        let old_pkg_id: i64 = tx
            .query_row(
                "SELECT id FROM packages WHERE name = ?1",
                params![replacement.old_name],
                |row| row.get(0),
            )
            .with_context(|| format!("Package '{}' not found", replacement.old_name))?;

        if replacement.retains_old_package() {
            tx.execute(
                "DELETE FROM files WHERE package_id = ?1",
                params![old_pkg_id],
            )?;
            tx.execute(
                "DELETE FROM directories WHERE package_id = ?1",
                params![old_pkg_id],
            )?;
            for file in &replacement.retained_files {
                tx.execute(
                    "INSERT INTO files (package_id, path) VALUES (?1, ?2)",
                    params![old_pkg_id, file],
                )?;
            }
            for directory in &replacement.retained_directories {
                tx.execute(
                    "INSERT INTO directories (package_id, path) VALUES (?1, ?2)",
                    params![old_pkg_id, directory],
                )?;
            }
        } else {
            delete_package_rows_tx(&tx, old_pkg_id)?;
        }
    }

    // Insert/update package without changing its primary key (UPSERT keeps the existing row).
    tx.execute(
        "INSERT INTO packages (name, real_name, version, revision, description, homepage, license, abi_breaking, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(name) DO UPDATE SET
            real_name=excluded.real_name,
            version=excluded.version,
            revision=excluded.revision,
            description=excluded.description,
            homepage=excluded.homepage,
            license=excluded.license,
            abi_breaking=excluded.abi_breaking,
            completed_at=excluded.completed_at",
        params![
            spec.package.name,
            spec.package.real_name,
            spec.package.version,
            spec.package.revision,
            spec.package.description,
            spec.package.homepage,
            format_licenses(&spec.package.license),
            spec.package.abi_breaking,
            completed_at,
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
    tx.execute(
        "DELETE FROM replaces WHERE package_id = ?1",
        params![pkg_id],
    )?;
    tx.execute("DELETE FROM files WHERE package_id = ?1", params![pkg_id])?;
    tx.execute(
        "DELETE FROM directories WHERE package_id = ?1",
        params![pkg_id],
    )?;
    tx.execute(
        "DELETE FROM package_groups WHERE package_id = ?1",
        params![pkg_id],
    )?;

    // Insert provides
    for provides in &spec.alternatives.provides {
        tx.execute(
            "INSERT OR IGNORE INTO provides (package_id, provides_name) VALUES (?1, ?2)",
            params![pkg_id, provides],
        )?;
    }

    for replaces in &spec.alternatives.replaces {
        tx.execute(
            "INSERT OR IGNORE INTO replaces (package_id, replaces_name) VALUES (?1, ?2)",
            params![pkg_id, replaces],
        )?;
    }
    for group in &spec.dependencies.groups {
        tx.execute(
            "INSERT OR IGNORE INTO package_groups (package_id, group_name) VALUES (?1, ?2)",
            params![pkg_id, group],
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
        if let Ok(owner) = owner_res
            && owner != spec.package.name
        {
            if should_auto_clear_conflict(&owner, file) {
                auto_conflicts.push((file.clone(), owner));
            } else {
                fatal_conflicts.push((file.clone(), owner));
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
                if destdir.join(f).symlink_metadata().is_ok() {
                    crate::log_info!(
                        "Auto-cleared DB ownership for path: {} (previously owned by {})",
                        f,
                        owner
                    );
                    continue;
                }

                let disk_path = rootfs.join(f);
                if disk_path.exists() {
                    let _ = std::fs::remove_file(&disk_path);
                    crate::log_info!(
                        "Auto-removed conflicting path: {} (was owned by {})",
                        f,
                        owner
                    );
                }
            } else {
                crate::log_info!(
                    "Auto-cleared DB ownership for path: {} (previously owned by {})",
                    f,
                    owner
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

    Ok(())
}

/// Return the list of files owned by an installed package.
pub fn get_package_files(db_path: &Path, name: &str) -> Result<Vec<String>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;

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

/// Return the list of directories owned by an installed package.
pub fn get_package_directories(db_path: &Path, name: &str) -> Result<Vec<String>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;

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

    let mut stmt = conn.prepare("SELECT path FROM directories WHERE package_id = ?1")?;
    let mut directories: Vec<String> = stmt
        .query_map(params![pkg_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    Ok(directories)
}

/// Remove a package from the database and filesystem
pub fn remove_package(db_path: &Path, name: &str, rootfs: &Path) -> Result<()> {
    if !db_path.exists() {
        anyhow::bail!("Package database not found");
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;

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
                if verbose_remove_output() {
                    crate::log_info!("  Removed file: {}", file);
                }
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
            continue;
        }

        // Try to remove (will fail if not empty, which is fine)
        match fs::remove_dir(&path) {
            Ok(()) => {
                if verbose_remove_output() {
                    crate::log_info!("  Removed directory: {}", dir);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone
            }
            Err(e) if e.raw_os_error() == Some(39) || e.raw_os_error() == Some(66) => {
                // ENOTEMPTY (39 on Linux, 66 on macOS) - directory not empty
                if verbose_remove_output() {
                    crate::log_info!("  Keeping directory (not empty): {}", dir);
                }
            }
            Err(_) => {
                // Other errors (permission, etc.) - just skip silently
            }
        }
    }

    // Remove from database
    delete_package_rows(&conn, pkg_id)?;

    if !removal_errors.is_empty() {
        crate::log_warn!("Failed to remove some paths:");
        for err in removal_errors {
            crate::log_warn!("  {}", err);
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
    init_db(&conn)?;

    let (version, revision, description, homepage, license): (String, u32, String, String, String) = conn
        .query_row(
            "SELECT version, revision, description, homepage, license FROM packages WHERE name = ?1",
            params![name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .context(format!("Package '{}' not found", name))?;

    crate::log_info!("Package: {} v{}-{}", name, version, revision);
    crate::log_info!("Description: {}", description);
    crate::log_info!("Homepage: {}", homepage);
    crate::log_info!("License: {}", license);

    let groups = get_package_groups(db_path, name)?;
    if !groups.is_empty() {
        crate::log_info!("Groups: {}", groups.join(", "));
    }

    // Count files
    let file_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files f JOIN packages p ON f.package_id = p.id WHERE p.name = ?1",
        params![name],
        |row| row.get(0),
    )?;

    crate::log_info!("Files: {}", file_count);

    Ok(())
}

/// List all installed packages
pub fn list_packages(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        crate::log_info!("No packages installed.");
        return Ok(());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;

    let mut stmt = conn.prepare(
        "SELECT
            p.name,
            p.version,
            GROUP_CONCAT(pg.group_name, ',')
         FROM packages p
         LEFT JOIN package_groups pg ON pg.package_id = p.id
         GROUP BY p.id
         ORDER BY p.name",
    )?;
    let packages = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;

    crate::log_info!("{:<30} VERSION", "PACKAGE");
    crate::log_info!("{}", "-".repeat(50));

    for pkg in packages {
        let (name, version, groups_csv) = pkg?;
        let groups = groups_csv
            .map(|csv| {
                let mut values: Vec<String> = csv
                    .split(',')
                    .filter(|value| !value.is_empty())
                    .map(String::from)
                    .collect();
                values.sort();
                values.dedup();
                values
            })
            .unwrap_or_default();
        if groups.is_empty() {
            crate::log_info!("{:<30} {}", name, version);
        } else {
            crate::log_info!("{:<30} {} [groups: {}]", name, version, groups.join(", "));
        }
    }

    Ok(())
}

fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS packages (
            id INTEGER PRIMARY KEY,
            name TEXT UNIQUE NOT NULL,
            real_name TEXT,
            version TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            description TEXT,
            homepage TEXT,
            license TEXT,
            abi_breaking INTEGER NOT NULL DEFAULT 0,
            completed_at INTEGER
        );

        CREATE TABLE IF NOT EXISTS provides (
            id INTEGER PRIMARY KEY,
            package_id INTEGER NOT NULL,
            provides_name TEXT NOT NULL,
            FOREIGN KEY (package_id) REFERENCES packages(id),
            UNIQUE(package_id, provides_name)
        );

        CREATE TABLE IF NOT EXISTS replaces (
            id INTEGER PRIMARY KEY,
            package_id INTEGER NOT NULL,
            replaces_name TEXT NOT NULL,
            FOREIGN KEY (package_id) REFERENCES packages(id),
            UNIQUE(package_id, replaces_name)
        );

        CREATE TABLE IF NOT EXISTS package_groups (
            id INTEGER PRIMARY KEY,
            package_id INTEGER NOT NULL,
            group_name TEXT NOT NULL,
            FOREIGN KEY (package_id) REFERENCES packages(id),
            UNIQUE(package_id, group_name)
        );

        CREATE TABLE IF NOT EXISTS installed_groups (
            group_name TEXT PRIMARY KEY NOT NULL
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
        CREATE INDEX IF NOT EXISTS idx_replaces_name ON replaces(replaces_name);
        CREATE INDEX IF NOT EXISTS idx_package_groups_name ON package_groups(group_name);
        CREATE INDEX IF NOT EXISTS idx_directories_package ON directories(package_id);
        CREATE INDEX IF NOT EXISTS idx_directories_path ON directories(path);
        ",
    )?;
    ensure_packages_completed_at_column(conn)?;
    ensure_packages_real_name_column(conn)?;
    ensure_packages_abi_breaking_column(conn)?;
    Ok(())
}

fn ensure_packages_completed_at_column(conn: &Connection) -> Result<()> {
    let has_completed_at: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'completed_at'",
            [],
            |row| {
                let count: i64 = row.get(0)?;
                Ok(count > 0)
            },
        )
        .context("Failed to inspect installed package DB schema")?;
    if !has_completed_at {
        conn.execute("ALTER TABLE packages ADD COLUMN completed_at INTEGER", [])
            .context("Failed to add completed_at column to installed package DB")?;
    }
    Ok(())
}

fn ensure_packages_real_name_column(conn: &Connection) -> Result<()> {
    let has_real_name: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'real_name'",
            [],
            |row| {
                let count: i64 = row.get(0)?;
                Ok(count > 0)
            },
        )
        .context("Failed to inspect installed package DB schema")?;
    if !has_real_name {
        conn.execute("ALTER TABLE packages ADD COLUMN real_name TEXT", [])
            .context("Failed to add real_name column to installed package DB")?;
    }
    Ok(())
}

fn ensure_packages_abi_breaking_column(conn: &Connection) -> Result<()> {
    let has_abi_breaking: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'abi_breaking'",
            [],
            |row| {
                let count: i64 = row.get(0)?;
                Ok(count > 0)
            },
        )
        .context("Failed to inspect installed package DB schema")?;
    if !has_abi_breaking {
        conn.execute(
            "ALTER TABLE packages ADD COLUMN abi_breaking INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .context("Failed to add abi_breaking column to installed package DB")?;
    }
    Ok(())
}

fn delete_package_rows(conn: &Connection, pkg_id: i64) -> Result<()> {
    conn.execute("DELETE FROM files WHERE package_id = ?1", params![pkg_id])?;
    conn.execute(
        "DELETE FROM directories WHERE package_id = ?1",
        params![pkg_id],
    )?;
    conn.execute(
        "DELETE FROM provides WHERE package_id = ?1",
        params![pkg_id],
    )?;
    conn.execute(
        "DELETE FROM replaces WHERE package_id = ?1",
        params![pkg_id],
    )?;
    conn.execute(
        "DELETE FROM package_groups WHERE package_id = ?1",
        params![pkg_id],
    )?;
    conn.execute("DELETE FROM packages WHERE id = ?1", params![pkg_id])?;
    Ok(())
}

fn delete_package_rows_tx(tx: &rusqlite::Transaction<'_>, pkg_id: i64) -> Result<()> {
    tx.execute("DELETE FROM files WHERE package_id = ?1", params![pkg_id])?;
    tx.execute(
        "DELETE FROM directories WHERE package_id = ?1",
        params![pkg_id],
    )?;
    tx.execute(
        "DELETE FROM provides WHERE package_id = ?1",
        params![pkg_id],
    )?;
    tx.execute(
        "DELETE FROM replaces WHERE package_id = ?1",
        params![pkg_id],
    )?;
    tx.execute(
        "DELETE FROM package_groups WHERE package_id = ?1",
        params![pkg_id],
    )?;
    tx.execute("DELETE FROM packages WHERE id = ?1", params![pkg_id])?;
    Ok(())
}

fn package_completed_at(destdir: &Path) -> Result<Option<i64>> {
    let metadata_path = destdir.join(".metadata.toml");
    if let Some(completed_at) =
        crate::metadata_time::read_completed_at_from_metadata_path(&metadata_path)?
    {
        return Ok(Some(completed_at));
    }
    latest_tree_mtime(destdir)
}

fn latest_tree_mtime(path: &Path) -> Result<Option<i64>> {
    let mut latest = None;

    for entry in WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let modified = entry
            .metadata()
            .with_context(|| format!("Failed to read metadata for {}", entry.path().display()))?
            .modified()
            .with_context(|| {
                format!(
                    "Failed to read modification time for {}",
                    entry.path().display()
                )
            })?;
        let modified = crate::metadata_time::system_time_to_unix(modified)?;
        latest = Some(latest.map_or(modified, |current: i64| current.max(modified)));
    }

    Ok(latest)
}

/// Decide whether a conflicting path is safe to auto-remove ownership for.
/// This covers common language-shared trees such as Perl site/vendor/lib
/// directories.
fn is_auto_removable_path(path: &str) -> bool {
    let p = path.trim_start_matches('/');

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
    new_manifest: &staging::Manifest,
) -> Result<Vec<String>> {
    let old_files = get_package_files(db_path, name)?;
    let old_directories = get_package_directories(db_path, name)?;
    let new_files: std::collections::HashSet<_> = new_manifest.files.iter().cloned().collect();
    let new_directories: std::collections::HashSet<_> =
        new_manifest.directories.iter().cloned().collect();

    let mut remove_paths: Vec<String> = old_files
        .into_iter()
        .filter(|p| !new_files.contains(p))
        .collect();
    remove_paths.extend(
        old_directories
            .into_iter()
            .filter(|p| !new_directories.contains(p)),
    );
    remove_paths.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));

    Ok(remove_paths)
}

/// Get set of all installed package names
pub fn get_installed_packages(db_path: &Path) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;

    if !db_path.exists() {
        return Ok(HashSet::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare("SELECT name FROM packages")?;
    let names: HashSet<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

/// List installed packages with version and revision metadata.
pub fn list_installed_package_records(db_path: &Path) -> Result<Vec<InstalledPackageRecord>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT name, real_name, version, revision, abi_breaking, completed_at
         FROM packages
         ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(InstalledPackageRecord {
            name: row.get(0)?,
            real_name: row.get(1)?,
            version: row.get(2)?,
            revision: row.get::<_, i64>(3)? as u32,
            abi_breaking: row.get(4)?,
            completed_at: row.get(5)?,
        })
    })?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Return the alternatives provided by an installed package.
pub fn get_package_provides(db_path: &Path, name: &str) -> Result<Vec<String>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT pr.provides_name
         FROM provides pr
         JOIN packages p ON p.id = pr.package_id
         WHERE p.name = ?1
         ORDER BY pr.provides_name",
    )?;
    let rows = stmt.query_map(params![name], |row| row.get(0))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Return the group memberships recorded for an installed package.
pub fn get_package_groups(db_path: &Path, name: &str) -> Result<Vec<String>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT pg.group_name
         FROM package_groups pg
         JOIN packages p ON p.id = pg.package_id
         WHERE p.name = ?1
         ORDER BY pg.group_name",
    )?;
    let rows = stmt.query_map(params![name], |row| row.get(0))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Return the installed package names that belong to an explicitly recorded group.
pub fn get_packages_in_installed_group(db_path: &Path, group: &str) -> Result<Vec<String>> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT p.name
         FROM package_groups pg
         JOIN packages p ON p.id = pg.package_id
         WHERE lower(pg.group_name) = lower(?1)
         ORDER BY p.name",
    )?;
    let rows = stmt.query_map(params![group], |row| row.get(0))?;
    Ok(rows.filter_map(|row| row.ok()).collect())
}

/// Return true when the named group was explicitly installed by the user.
pub fn is_installed_group(db_path: &Path, group: &str) -> Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let found = conn
        .query_row(
            "SELECT 1 FROM installed_groups WHERE lower(group_name) = lower(?1) LIMIT 1",
            params![group],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    Ok(found)
}

/// Record one or more explicit group installs.
pub fn record_installed_groups(db_path: &Path, groups: &[String]) -> Result<()> {
    if groups.is_empty() {
        return Ok(());
    }

    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let tx = conn.transaction()?;
    for group in groups {
        tx.execute(
            "INSERT OR IGNORE INTO installed_groups (group_name) VALUES (?1)",
            params![group],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Remove an explicit installed-group marker.
pub fn remove_installed_group(db_path: &Path, group: &str) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    conn.execute(
        "DELETE FROM installed_groups WHERE lower(group_name) = lower(?1)",
        params![group],
    )?;
    Ok(())
}

/// Get set of all provided package names (alternatives)
pub fn get_all_provides(db_path: &Path) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;

    if !db_path.exists() {
        return Ok(HashSet::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare("SELECT provides_name FROM provides")?;
    let names: HashSet<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

/// Get set of all replacement names satisfied by installed packages.
pub fn get_all_replaces(db_path: &Path) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;

    if !db_path.exists() {
        return Ok(HashSet::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare("SELECT replaces_name FROM replaces")?;
    let names: HashSet<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

/// Get version of a specific installed package
pub fn get_package_version(db_path: &Path, name: &str) -> Result<Option<String>> {
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let version: Option<String> = conn
        .query_row(
            "SELECT version FROM packages WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .ok();
    Ok(version)
}

/// Find the installed package that owns a filesystem path from the local DB.
pub fn owns_path(db_path: &Path, path: &Path) -> Result<Option<String>> {
    if !db_path.exists() {
        return Ok(None);
    }

    let normalized = path
        .to_string_lossy()
        .trim_start_matches('/')
        .trim_start_matches("./")
        .to_string();
    if normalized.is_empty() {
        return Ok(None);
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let owner = conn
        .query_row(
            "SELECT p.name
             FROM files f
             JOIN packages p ON p.id = f.package_id
             WHERE f.path = ?1
             LIMIT 1",
            params![normalized],
            |row| row.get(0),
        )
        .ok();
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
    };
    use crate::test_support::TestEnv;
    use std::path::PathBuf;

    fn mk_spec(name: &str, version: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: name.into(),
                real_name: None,
                version: version.into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives {
                provides: vec![format!("{}-virtual", name)],
                conflicts: Vec::new(),
                replaces: Vec::new(),
                lib32: None,
            },
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
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
    fn register_package_uses_metadata_completed_at_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let spec = mk_spec("foo", "1.0");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
        std::fs::write(dest.join("usr/bin/foo"), "bin").unwrap();
        std::fs::write(
            dest.join(".metadata.toml"),
            "completed_at = \"2026-03-10T12:34:56Z\"\n",
        )
        .unwrap();

        register_package(&db_path, &spec, &dest).unwrap();

        let records = list_installed_package_records(&db_path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].completed_at, Some(1_773_146_096));
    }

    #[test]
    fn register_package_falls_back_to_destdir_mtime_when_metadata_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let spec = mk_spec("foo", "1.0");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
        let file = dest.join("usr/bin/foo");
        std::fs::write(&file, "bin").unwrap();

        let ts = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&file, ts).unwrap();
        filetime::set_file_mtime(dest.join("usr"), ts).unwrap();
        filetime::set_file_mtime(dest.join("usr/bin"), ts).unwrap();
        filetime::set_file_mtime(&dest, ts).unwrap();

        register_package(&db_path, &spec, &dest).unwrap();

        let records = list_installed_package_records(&db_path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].completed_at, Some(1_700_000_000));
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

        // Install package 'alpha' owning a known shared Perl path
        let spec_a = mk_spec("alpha", "1.0");
        let dest_a = tmp.path().join("dest_a");
        std::fs::create_dir_all(dest_a.join("usr/share/perl5")).unwrap();
        std::fs::write(dest_a.join("usr/share/perl5/shared.pm"), "package A;").unwrap();
        register_package(&db_path, &spec_a, &dest_a).unwrap();

        // Now install package 'beta' that also provides the same shared path -> should auto-clear
        let spec_b = mk_spec("beta", "1.0");
        let dest_b = tmp.path().join("dest_b");
        std::fs::create_dir_all(dest_b.join("usr/share/perl5")).unwrap();
        std::fs::write(dest_b.join("usr/share/perl5/shared.pm"), "package B;").unwrap();

        // This should succeed and transfer ownership of the shared path to beta
        register_package(&db_path, &spec_b, &dest_b).unwrap();

        // Verify DB: alpha should no longer own the path, beta should
        let files_a = get_package_files(&db_path, "alpha").unwrap();
        assert!(!files_a.contains(&"usr/share/perl5/shared.pm".to_string()));
        let files_b = get_package_files(&db_path, "beta").unwrap();
        assert!(files_b.contains(&"usr/share/perl5/shared.pm".to_string()));
    }

    #[test]
    fn register_package_auto_clears_sbase_conflicts_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let db_path = crate::config::Config::for_rootfs(&rootfs).installed_db_path(&rootfs);
        std::fs::create_dir_all(rootfs.join("system/binaries")).unwrap();
        std::fs::write(rootfs.join("system/binaries/find"), "sbase find").unwrap();

        let spec_a = mk_spec("sbase", "1.0");
        let dest_a = tmp.path().join("dest_a");
        std::fs::create_dir_all(dest_a.join("system/binaries")).unwrap();
        std::fs::write(dest_a.join("system/binaries/find"), "sbase find").unwrap();
        register_package(&db_path, &spec_a, &dest_a).unwrap();

        let spec_b = mk_spec("bfs", "4.1");
        let dest_b = tmp.path().join("dest_b");
        std::fs::create_dir_all(dest_b.join("system/binaries")).unwrap();
        std::fs::write(dest_b.join("system/binaries/find"), "bfs find").unwrap();

        let mut env = TestEnv::new();
        env.set_var(DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS, "1");
        register_package(&db_path, &spec_b, &dest_b).unwrap();

        let files_sbase = get_package_files(&db_path, "sbase").unwrap();
        assert!(!files_sbase.contains(&"system/binaries/find".to_string()));
        let files_bfs = get_package_files(&db_path, "bfs").unwrap();
        assert!(files_bfs.contains(&"system/binaries/find".to_string()));
    }

    #[test]
    fn register_package_auto_clear_preserves_new_payload_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let db_path = crate::config::Config::for_rootfs(&rootfs).installed_db_path(&rootfs);
        std::fs::create_dir_all(rootfs.join("system/binaries")).unwrap();
        std::fs::write(rootfs.join("system/binaries/find"), "sbase find").unwrap();

        let spec_a = mk_spec("sbase", "1.0");
        let dest_a = tmp.path().join("dest_a");
        std::fs::create_dir_all(dest_a.join("system/binaries")).unwrap();
        std::fs::write(dest_a.join("system/binaries/find"), "sbase find").unwrap();
        register_package(&db_path, &spec_a, &dest_a).unwrap();

        std::fs::write(rootfs.join("system/binaries/find"), "bfs find").unwrap();
        let spec_b = mk_spec("bfs", "4.1");
        let dest_b = tmp.path().join("dest_b");
        std::fs::create_dir_all(dest_b.join("system/binaries")).unwrap();
        std::fs::write(dest_b.join("system/binaries/find"), "bfs find").unwrap();

        let mut env = TestEnv::new();
        env.set_var(DEPOT_BOOTSTRAP_IGNORE_SBASE_CONFLICTS, "1");
        register_package(&db_path, &spec_b, &dest_b).unwrap();

        assert_eq!(
            std::fs::read_to_string(rootfs.join("system/binaries/find")).unwrap(),
            "bfs find"
        );
        let files_sbase = get_package_files(&db_path, "sbase").unwrap();
        assert!(!files_sbase.contains(&"system/binaries/find".to_string()));
        let files_bfs = get_package_files(&db_path, "bfs").unwrap();
        assert!(files_bfs.contains(&"system/binaries/find".to_string()));
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
    fn get_package_version_missing_db_returns_none_without_creating_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");

        let version = get_package_version(&db_path, "nonexistent").unwrap();
        assert!(version.is_none());
        assert!(!db_path.exists());
    }

    #[test]
    fn calculate_upgrade_paths_handles_existing_db_file_without_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        std::fs::File::create(&db_path).unwrap();
        let manifest = staging::Manifest {
            files: vec!["usr/bin/foo".to_string()],
            directories: Vec::new(),
        };

        let remove_paths = calculate_upgrade_paths(&db_path, "nonexistent", &manifest).unwrap();
        assert!(remove_paths.is_empty());
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
        let _ = crate::staging::install_atomic(&dest1, &rootfs, &tx_base, &[], &[]).unwrap();

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
        let remove_paths = calculate_upgrade_paths(&db_path, "foo", &manifest2).unwrap();

        assert_eq!(
            remove_paths,
            vec![
                "usr/bin/shared_dir/old_file".to_string(),
                "usr/bin/shared_dir".to_string()
            ]
        );

        let tx =
            crate::staging::install_atomic(&dest2, &rootfs, &tx_base, &remove_paths, &[]).unwrap();
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

    #[test]
    fn register_package_persists_replacements() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
        std::fs::write(dest.join("usr/bin/vx"), "vx").unwrap();

        let mut spec = mk_spec("vx", "1.0");
        spec.alternatives.replaces = vec!["grep".into(), "patch".into()];

        register_package(&db_path, &spec, &dest).unwrap();

        let replaces = get_all_replaces(&db_path).unwrap();
        assert!(replaces.contains("grep"));
        assert!(replaces.contains("patch"));
    }

    #[test]
    fn register_package_persists_groups() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
        std::fs::write(dest.join("usr/bin/foo"), "foo").unwrap();

        let mut spec = mk_spec("foo", "1.0");
        spec.dependencies.groups = vec!["base".into(), "desktop".into()];

        register_package(&db_path, &spec, &dest).unwrap();

        assert_eq!(
            get_package_groups(&db_path, "foo").unwrap(),
            vec!["base".to_string(), "desktop".to_string()]
        );
    }

    #[test]
    fn installed_group_helpers_round_trip_membership() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(dest.join("usr/bin")).unwrap();
        std::fs::write(dest.join("usr/bin/foo"), "foo").unwrap();

        let mut spec = mk_spec("foo", "1.0");
        spec.dependencies.groups = vec!["base".into()];
        register_package(&db_path, &spec, &dest).unwrap();

        record_installed_groups(&db_path, &[String::from("base")]).unwrap();
        assert!(is_installed_group(&db_path, "base").unwrap());
        assert_eq!(
            get_packages_in_installed_group(&db_path, "base").unwrap(),
            vec!["foo".to_string()]
        );

        remove_installed_group(&db_path, "base").unwrap();
        assert!(!is_installed_group(&db_path, "base").unwrap());
    }
}
