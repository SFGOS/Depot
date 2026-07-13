use super::*;

/// Search a cached binary repository SQLite DB by package name or provided feature.
pub fn search_cached_binary_repo_db(
    repo_name: &str,
    db_path: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoSearchHit>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let like = format!("%{}%", query.to_ascii_lowercase());
    let mut stmt = conn.prepare(
        "SELECT
            p.name,
            p.version,
            p.revision,
            p.description,
            p.filename,
            p.size,
            GROUP_CONCAT(DISTINCT pr_all.name)
         FROM packages p
         LEFT JOIN provides pr_all ON pr_all.package_id = p.id
         WHERE lower(p.name) LIKE ?1
            OR EXISTS (
                SELECT 1 FROM provides pr
                WHERE pr.package_id = p.id
                  AND lower(pr.name) LIKE ?1
            )
         GROUP BY p.id
         ORDER BY
            CASE
                WHEN lower(p.name) = lower(?2) THEN 0
                WHEN lower(p.name) LIKE lower(?3) THEN 1
                ELSE 2
            END,
            p.name ASC",
    )?;

    let starts = format!("{}%", query.to_ascii_lowercase());
    let rows = stmt.query_map(params![like, query, starts], |row| {
        let provides_csv: Option<String> = row.get(6)?;
        Ok(BinaryRepoSearchHit {
            repo_name: repo_name.to_string(),
            name: row.get(0)?,
            version: row.get(1)?,
            revision: row.get::<_, i64>(2)? as u32,
            description: row.get(3)?,
            filename: row.get(4)?,
            size: row.get::<_, i64>(5)? as u64,
            provides: provides_csv
                .map(|s| {
                    s.split(',')
                        .filter(|v| !v.is_empty())
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        })
    })?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Fetch and search a binary repo by name or provide.
pub fn search_binary_repo(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoSearchHit>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    search_cached_binary_repo_db(repo_name, &db_path, query)
}

/// Search a cached binary repo DB by file path substring.
pub fn search_cached_binary_repo_files(
    repo_name: &str,
    db_path: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let has_files_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_files_table {
        return Ok(Vec::new());
    }

    let like = format!("%{}%", query.to_ascii_lowercase());
    let mut stmt = conn.prepare(
        "SELECT p.name, p.version, p.revision, f.path, p.size
         FROM files f
         JOIN packages p ON p.id = f.package_id
         WHERE lower(f.path) LIKE ?1
         ORDER BY p.name ASC, f.path ASC",
    )?;
    let rows = stmt.query_map(params![like], |row| {
        Ok(BinaryRepoFileSearchHit {
            repo_name: repo_name.to_string(),
            package_name: row.get(0)?,
            version: row.get(1)?,
            revision: row.get::<_, i64>(2)? as u32,
            path: row.get(3)?,
            size: row.get::<_, i64>(4)? as u64,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// Fetch and search a binary repo by file path substring.
pub fn search_binary_repo_files(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    search_cached_binary_repo_files(repo_name, &db_path, query)
}

/// Find the package(s) that own a file path in a cached binary repo DB.
pub fn cached_binary_repo_owns_path(
    repo_name: &str,
    db_path: &Path,
    path: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let has_files_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_files_table {
        return Ok(Vec::new());
    }

    let normalized = path.trim_start_matches('/').trim_start_matches("./");
    let mut stmt = conn.prepare(
        "SELECT p.name, p.version, p.revision, f.path, p.size
         FROM files f
         JOIN packages p ON p.id = f.package_id
         WHERE f.path = ?1
         ORDER BY p.name ASC",
    )?;
    let rows = stmt.query_map(params![normalized], |row| {
        Ok(BinaryRepoFileSearchHit {
            repo_name: repo_name.to_string(),
            package_name: row.get(0)?,
            version: row.get(1)?,
            revision: row.get::<_, i64>(2)? as u32,
            path: row.get(3)?,
            size: row.get::<_, i64>(4)? as u64,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(super) fn repo_owns_query_candidates(rootfs: &Path, path: &str) -> Vec<String> {
    let normalized = path.trim_start_matches('/').trim_start_matches("./");
    let mut candidates = BTreeSet::new();
    if !normalized.is_empty() {
        candidates.insert(normalized.to_string());
    }

    let query_path = Path::new(path);
    let fs_path = if query_path.is_absolute() {
        rootfs.join(query_path.strip_prefix("/").unwrap_or(query_path))
    } else {
        rootfs.join(query_path)
    };

    if let Ok(resolved) = fs::canonicalize(&fs_path)
        && let Some(rel) = resolved_repo_owns_path(rootfs, &resolved)
        && !rel.is_empty()
    {
        candidates.insert(rel);
    }

    candidates.into_iter().collect()
}

pub(super) fn resolved_repo_owns_path(rootfs: &Path, resolved: &Path) -> Option<String> {
    if rootfs == Path::new("/") {
        return Some(
            resolved
                .to_string_lossy()
                .trim_start_matches('/')
                .to_string(),
        );
    }

    resolved
        .strip_prefix(rootfs)
        .ok()
        .map(|rel| rel.to_string_lossy().trim_start_matches('/').to_string())
}

/// Fetch repo metadata and resolve file ownership in a binary repo.
pub fn binary_repo_owns_path(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    path: &str,
) -> Result<Vec<BinaryRepoFileSearchHit>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    let mut hits = Vec::new();
    let mut seen = BTreeSet::new();
    for candidate in repo_owns_query_candidates(rootfs, path) {
        for hit in cached_binary_repo_owns_path(repo_name, &db_path, &candidate)? {
            let key = format!(
                "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
                hit.repo_name, hit.package_name, hit.version, hit.revision, hit.path
            );
            if seen.insert(key) {
                hits.push(hit);
            }
        }
    }
    Ok(hits)
}

pub(super) fn query_package_provides(conn: &Connection, package_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM provides WHERE package_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(super) fn query_package_conflicts(conn: &Connection, package_id: i64) -> Result<Vec<String>> {
    let has_conflicts_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='conflicts'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_conflicts_table {
        return Ok(Vec::new());
    }

    let mut stmt =
        conn.prepare("SELECT name FROM conflicts WHERE package_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(super) fn query_package_replaces(conn: &Connection, package_id: i64) -> Result<Vec<String>> {
    let has_replaces_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='replaces'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_replaces_table {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare("SELECT name FROM replaces WHERE package_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(super) fn query_package_runtime_deps(
    conn: &Connection,
    package_id: i64,
) -> Result<Vec<String>> {
    let has_dependencies_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dependencies'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_dependencies_table {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT name FROM dependencies WHERE package_id = ?1 AND kind = 'runtime' ORDER BY name",
    )?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(super) fn query_package_built_against(
    conn: &Connection,
    package_id: i64,
) -> Result<Vec<String>> {
    if !repo_packages_have_built_against(conn)? {
        return Ok(Vec::new());
    }

    let raw: String = conn.query_row(
        "SELECT built_against FROM packages WHERE id = ?1",
        params![package_id],
        |row| row.get(0),
    )?;
    Ok(parse_built_against(&raw))
}

pub(super) fn query_package_optional_deps(
    conn: &Connection,
    package_id: i64,
) -> Result<Vec<String>> {
    let has_dependencies_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='dependencies'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_dependencies_table {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT name FROM dependencies WHERE package_id = ?1 AND kind = 'optional' ORDER BY name",
    )?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(super) fn query_package_groups(conn: &Connection, package_id: i64) -> Result<Vec<String>> {
    let has_groups_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='groups'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_groups_table {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare("SELECT name FROM groups WHERE package_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map(params![package_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub(super) fn find_cached_binary_repo_packages(
    repo_name: &str,
    db_path: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let completed_at_expr = if repo_packages_have_completed_at(&conn)? {
        "p.completed_at"
    } else {
        "NULL"
    };
    let real_name_expr = if repo_packages_have_real_name(&conn)? {
        "p.real_name"
    } else {
        "NULL"
    };
    let abi_breaking_expr = if repo_packages_have_abi_breaking(&conn)? {
        "p.abi_breaking"
    } else {
        "0"
    };
    let built_against_expr = if repo_packages_have_built_against(&conn)? {
        "p.built_against"
    } else {
        "''"
    };
    let sql = format!(
        "SELECT
            p.id,
            p.name,
            {real_name_expr},
            p.version,
            p.revision,
            {abi_breaking_expr},
            {built_against_expr},
            {completed_at_expr},
            p.filename,
            p.size,
            p.sha512,
            p.description,
            p.homepage,
            p.license
         FROM packages p
         WHERE lower(p.name) = lower(?1)
            OR lower({real_name_expr}) = lower(?1)
                        OR EXISTS (
                                SELECT 1 FROM replaces rp
                                WHERE rp.package_id = p.id
                                    AND lower(rp.name) = lower(?1)
                        )
            OR EXISTS (
                SELECT 1 FROM provides pr
                WHERE pr.package_id = p.id
                  AND lower(pr.name) = lower(?1)
            )
         ORDER BY
                        CASE
                                WHEN EXISTS (
                                        SELECT 1 FROM replaces rp
                                        WHERE rp.package_id = p.id
                                            AND lower(rp.name) = lower(?1)
                                ) THEN 0
                                WHEN lower(p.name) = lower(?1) THEN 1
                                WHEN lower({real_name_expr}) = lower(?1) THEN 2
                                ELSE 2
                        END,
            p.name ASC"
    );
    let mut stmt = conn.prepare(&sql)?;

    let rows = stmt.query_map(params![query], |row| {
        let package_id = row.get::<_, i64>(0)?;
        Ok((
            package_id,
            BinaryRepoPackageRecord {
                repo_name: repo_name.to_string(),
                name: row.get(1)?,
                real_name: row.get(2)?,
                version: row.get(3)?,
                revision: row.get::<_, i64>(4)? as u32,
                abi_breaking: row.get(5)?,
                built_against: parse_built_against(&row.get::<_, String>(6)?),
                completed_at: row.get(7)?,
                filename: row.get(8)?,
                size: row.get::<_, i64>(9)? as u64,
                sha512: row.get(10)?,
                description: row.get(11)?,
                homepage: row.get(12)?,
                license: row.get(13)?,
                provides: Vec::new(),
                conflicts: Vec::new(),
                replaces: Vec::new(),
                runtime_dependencies: Vec::new(),
                optional_dependencies: Vec::new(),
                groups: Vec::new(),
            },
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (package_id, mut rec) = row?;
        rec.provides = query_package_provides(&conn, package_id)?;
        rec.conflicts = query_package_conflicts(&conn, package_id)?;
        rec.replaces = query_package_replaces(&conn, package_id)?;
        rec.runtime_dependencies = query_package_runtime_deps(&conn, package_id)?;
        rec.built_against = query_package_built_against(&conn, package_id)?;
        rec.optional_dependencies = query_package_optional_deps(&conn, package_id)?;
        rec.groups = query_package_groups(&conn, package_id)?;
        out.push(rec);
    }
    Ok(out)
}

pub(super) fn find_cached_binary_repo_packages_by_group(
    repo_name: &str,
    db_path: &Path,
    group: &str,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let has_groups_table: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='groups'",
            [],
            |r| {
                let n: i64 = r.get(0)?;
                Ok(n > 0)
            },
        )
        .unwrap_or(false);
    if !has_groups_table {
        return Ok(Vec::new());
    }

    let completed_at_expr = if repo_packages_have_completed_at(&conn)? {
        "p.completed_at"
    } else {
        "NULL"
    };
    let real_name_expr = if repo_packages_have_real_name(&conn)? {
        "p.real_name"
    } else {
        "NULL"
    };
    let abi_breaking_expr = if repo_packages_have_abi_breaking(&conn)? {
        "p.abi_breaking"
    } else {
        "0"
    };
    let built_against_expr = if repo_packages_have_built_against(&conn)? {
        "p.built_against"
    } else {
        "''"
    };
    let sql = format!(
        "SELECT
            p.id,
            p.name,
            {real_name_expr},
            p.version,
            p.revision,
            {abi_breaking_expr},
            {built_against_expr},
            {completed_at_expr},
            p.filename,
            p.size,
            p.sha512,
            p.description,
            p.homepage,
            p.license
         FROM packages p
         WHERE EXISTS (
            SELECT 1 FROM groups g
            WHERE g.package_id = p.id
              AND lower(g.name) = lower(?1)
         )
         ORDER BY p.name ASC"
    );
    let mut stmt = conn.prepare(&sql)?;

    let rows = stmt.query_map(params![group], |row| {
        let package_id = row.get::<_, i64>(0)?;
        Ok((
            package_id,
            BinaryRepoPackageRecord {
                repo_name: repo_name.to_string(),
                name: row.get(1)?,
                real_name: row.get(2)?,
                version: row.get(3)?,
                revision: row.get::<_, i64>(4)? as u32,
                abi_breaking: row.get(5)?,
                built_against: parse_built_against(&row.get::<_, String>(6)?),
                completed_at: row.get(7)?,
                filename: row.get(8)?,
                size: row.get::<_, i64>(9)? as u64,
                sha512: row.get(10)?,
                description: row.get(11)?,
                homepage: row.get(12)?,
                license: row.get(13)?,
                provides: Vec::new(),
                conflicts: Vec::new(),
                replaces: Vec::new(),
                runtime_dependencies: Vec::new(),
                optional_dependencies: Vec::new(),
                groups: Vec::new(),
            },
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (package_id, mut rec) = row?;
        rec.provides = query_package_provides(&conn, package_id)?;
        rec.conflicts = query_package_conflicts(&conn, package_id)?;
        rec.replaces = query_package_replaces(&conn, package_id)?;
        rec.runtime_dependencies = query_package_runtime_deps(&conn, package_id)?;
        rec.built_against = query_package_built_against(&conn, package_id)?;
        rec.optional_dependencies = query_package_optional_deps(&conn, package_id)?;
        rec.groups = query_package_groups(&conn, package_id)?;
        out.push(rec);
    }
    Ok(out)
}

pub(super) fn list_cached_binary_repo_packages(
    repo_name: &str,
    db_path: &Path,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open binary repo DB {}", db_path.display()))?;

    let completed_at_expr = if repo_packages_have_completed_at(&conn)? {
        "p.completed_at"
    } else {
        "NULL"
    };
    let real_name_expr = if repo_packages_have_real_name(&conn)? {
        "p.real_name"
    } else {
        "NULL"
    };
    let abi_breaking_expr = if repo_packages_have_abi_breaking(&conn)? {
        "p.abi_breaking"
    } else {
        "0"
    };
    let built_against_expr = if repo_packages_have_built_against(&conn)? {
        "p.built_against"
    } else {
        "''"
    };
    let sql = format!(
        "SELECT
            p.id,
            p.name,
            {real_name_expr},
            p.version,
            p.revision,
            {abi_breaking_expr},
            {built_against_expr},
            {completed_at_expr},
            p.filename,
            p.size,
            p.sha512,
            p.description,
            p.homepage,
            p.license
         FROM packages p
         ORDER BY p.name ASC, p.version ASC, p.revision ASC"
    );
    let mut stmt = conn.prepare(&sql)?;

    let rows = stmt.query_map([], |row| {
        let package_id = row.get::<_, i64>(0)?;
        Ok((
            package_id,
            BinaryRepoPackageRecord {
                repo_name: repo_name.to_string(),
                name: row.get(1)?,
                real_name: row.get(2)?,
                version: row.get(3)?,
                revision: row.get::<_, i64>(4)? as u32,
                abi_breaking: row.get(5)?,
                built_against: parse_built_against(&row.get::<_, String>(6)?),
                completed_at: row.get(7)?,
                filename: row.get(8)?,
                size: row.get::<_, i64>(9)? as u64,
                sha512: row.get(10)?,
                description: row.get(11)?,
                homepage: row.get(12)?,
                license: row.get(13)?,
                provides: Vec::new(),
                conflicts: Vec::new(),
                replaces: Vec::new(),
                runtime_dependencies: Vec::new(),
                optional_dependencies: Vec::new(),
                groups: Vec::new(),
            },
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (package_id, mut rec) = row?;
        rec.provides = query_package_provides(&conn, package_id)?;
        rec.conflicts = query_package_conflicts(&conn, package_id)?;
        rec.replaces = query_package_replaces(&conn, package_id)?;
        rec.runtime_dependencies = query_package_runtime_deps(&conn, package_id)?;
        rec.built_against = query_package_built_against(&conn, package_id)?;
        rec.optional_dependencies = query_package_optional_deps(&conn, package_id)?;
        rec.groups = query_package_groups(&conn, package_id)?;
        out.push(rec);
    }
    Ok(out)
}

pub(super) fn repo_packages_have_completed_at(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'completed_at'",
        [],
        |row| {
            let count: i64 = row.get(0)?;
            Ok(count > 0)
        },
    )
    .context("Failed to inspect binary repo DB schema")
}

pub(super) fn repo_packages_have_real_name(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'real_name'",
        [],
        |row| {
            let count: i64 = row.get(0)?;
            Ok(count > 0)
        },
    )
    .context("Failed to inspect binary repo DB schema")
}

pub(super) fn repo_packages_have_abi_breaking(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'abi_breaking'",
        [],
        |row| {
            let count: i64 = row.get(0)?;
            Ok(count > 0)
        },
    )
    .context("Failed to inspect binary repo DB schema")
}

pub(super) fn repo_packages_have_built_against(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'built_against'",
        [],
        |row| {
            let count: i64 = row.get(0)?;
            Ok(count > 0)
        },
    )
    .context("Failed to inspect binary repo DB schema")
}

pub(super) fn path_modified_unix_timestamp(path: &Path) -> Result<Option<i64>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("Failed to read modification time for {}", path.display()))?;
    Ok(Some(metadata_time::system_time_to_unix(modified)?))
}

/// Resolve an exact package name/provide match from a binary repo after verifying
/// and caching its signed `repo.db.zst`.
pub fn find_binary_repo_package(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Option<BinaryRepoPackageRecord>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    let mut matches = find_cached_binary_repo_packages(repo_name, &db_path, query)?;
    if matches.len() > 1 {
        crate::log_warn!(
            "Multiple binary packages matched '{}' in repo '{}'; using the first match",
            query,
            repo_name
        );
    }
    Ok(matches.drain(..).next())
}

/// Resolve exact package name/provide matches from a binary repo.
pub fn find_binary_repo_packages(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    query: &str,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    find_cached_binary_repo_packages(repo_name, &db_path, query)
}

/// Resolve package records that belong to the named group from a binary repo.
pub fn find_binary_repo_packages_by_group(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
    group: &str,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    find_cached_binary_repo_packages_by_group(repo_name, &db_path, group)
}

/// List all binary packages from a cached, verified repository database.
pub fn list_binary_repo_packages(
    repo_name: &str,
    repo: &crate::config::BinaryRepo,
    rootfs: &Path,
    package_cache_dir: &Path,
) -> Result<Vec<BinaryRepoPackageRecord>> {
    let db_path = fetch_binary_repo_db(repo_name, repo, rootfs, package_cache_dir)?;
    list_cached_binary_repo_packages(repo_name, &db_path)
}
