//! SQLite-based package database

pub mod repo;

use crate::package::PackageSpec;
use crate::staging;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
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

pub(crate) fn should_auto_clear_conflict(owner: &str, path: &str) -> bool {
    (owner == "sbase" && should_ignore_sbase_conflicts()) || is_auto_removable_path(path)
}

/// Return every installed file path and its owning package.
pub(crate) fn get_file_ownership(db_path: &Path) -> Result<BTreeMap<String, String>> {
    if !db_path.exists() {
        return Ok(BTreeMap::new());
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open package database at {}", db_path.display()))?;
    init_db(&conn).with_context(|| {
        format!(
            "Failed to initialize package database at {}",
            db_path.display()
        )
    })?;
    let mut stmt = conn
        .prepare(
            "SELECT f.path, p.name
             FROM files f
             JOIN packages p ON p.id = f.package_id
             ORDER BY f.path, p.name",
        )
        .context("Failed to prepare installed file ownership query")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("Failed to query installed file ownership")?;
    let mut ownership = BTreeMap::new();
    for row in rows {
        let (path, owner) = row.context("Failed to read installed file ownership row")?;
        ownership.insert(path, owner);
    }
    Ok(ownership)
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
    /// Concrete package names this package was built against for ABI-sensitive dependencies.
    pub built_against: Vec<String>,
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
        "INSERT INTO packages (name, real_name, version, revision, description, homepage, license, abi_breaking, built_against, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(name) DO UPDATE SET
            real_name=excluded.real_name,
            version=excluded.version,
            revision=excluded.revision,
            description=excluded.description,
            homepage=excluded.homepage,
            license=excluded.license,
            abi_breaking=excluded.abi_breaking,
            built_against=excluded.built_against,
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
            format_built_against(&spec.package.built_against),
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
            built_against TEXT NOT NULL DEFAULT '',
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
    ensure_packages_built_against_column(conn)?;
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

fn ensure_packages_built_against_column(conn: &Connection) -> Result<()> {
    let has_built_against: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'built_against'",
            [],
            |row| {
                let count: i64 = row.get(0)?;
                Ok(count > 0)
            },
        )
        .context("Failed to inspect installed package DB schema")?;
    if !has_built_against {
        conn.execute(
            "ALTER TABLE packages ADD COLUMN built_against TEXT NOT NULL DEFAULT ''",
            [],
        )
        .context("Failed to add built_against column to installed package DB")?;
    }
    Ok(())
}

fn format_built_against(packages: &[String]) -> String {
    packages.join("\n")
}

fn parse_built_against(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(String::from)
        .collect()
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

/// Get installed dependency names, including stable `real_name` aliases.
pub(crate) fn get_installed_dependency_names(
    db_path: &Path,
) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;

    if !db_path.exists() {
        return Ok(HashSet::new());
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT name FROM packages
         UNION
         SELECT real_name FROM packages WHERE real_name IS NOT NULL",
    )?;
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
        "SELECT name, real_name, version, revision, abi_breaking, built_against, completed_at
         FROM packages
         ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        let built_against: String = row.get(5)?;
        Ok(InstalledPackageRecord {
            name: row.get(0)?,
            real_name: row.get(1)?,
            version: row.get(2)?,
            revision: row.get::<_, i64>(3)? as u32,
            abi_breaking: row.get(4)?,
            built_against: parse_built_against(&built_against),
            completed_at: row.get(6)?,
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

/// Get an installed package version by package name or stable `real_name`.
pub(crate) fn get_dependency_version(db_path: &Path, name: &str) -> Result<Option<String>> {
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let version = conn
        .query_row(
            "SELECT version FROM packages
             WHERE name = ?1 OR real_name = ?1
             ORDER BY CASE WHEN name = ?1 THEN 0 ELSE 1 END, name
             LIMIT 1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(version)
}

/// Return the concrete installed ABI-breaking package satisfying a dependency name.
///
/// Dependency aliases are matched through the package name, stable `real_name`,
/// `provides`, and `replaces` metadata. The returned value is always the
/// concrete installed `packages.name` so packages built against renamed streams
/// can remember the ABI-carrying package they actually used.
pub(crate) fn get_abi_breaking_provider_for_dependency(
    db_path: &Path,
    name: &str,
) -> Result<Option<String>> {
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open(db_path)?;
    init_db(&conn)?;
    let provider = conn
        .query_row(
            "SELECT p.name
             FROM packages p
             WHERE p.abi_breaking = 1
               AND (
                    p.name = ?1
                 OR p.real_name = ?1
                 OR EXISTS (
                    SELECT 1 FROM provides pr
                    WHERE pr.package_id = p.id AND pr.provides_name = ?1
                 )
                 OR EXISTS (
                    SELECT 1 FROM replaces r
                    WHERE r.package_id = p.id AND r.replaces_name = ?1
                 )
               )
             ORDER BY
               CASE
                 WHEN p.name = ?1 THEN 0
                 WHEN p.real_name = ?1 THEN 1
                 ELSE 2
               END,
               p.name
             LIMIT 1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(provider)
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
mod tests;
