use super::*;

impl RepoManager {
    pub fn new(repo_dir: PathBuf) -> Self {
        Self { repo_dir }
    }

    /// Create a compressed SQLite repository database from a directory of packages
    pub fn create_repo_db(&self) -> Result<PathBuf> {
        let db_path = self.repo_dir.join("repo.db");
        let compressed_db_path = self.repo_dir.join("repo.db.zst");

        // Remove existing DB if it exists
        if db_path.exists() {
            fs::remove_file(&db_path)?;
        }

        let mut conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to create repo database at {}", db_path.display()))?;

        self.configure_repo_build_pragmas(&mut conn)?;
        self.init_repo_schema(&mut conn)?;

        let package_paths = self.collect_repo_package_paths()?;
        let indexed_packages = self.collect_indexed_packages_parallel(&package_paths)?;

        conn.execute_batch("BEGIN IMMEDIATE TRANSACTION;")
            .context("Failed to begin repo DB write transaction")?;
        let insert_result: Result<()> = (|| {
            for indexed in indexed_packages {
                self.insert_indexed_package(&mut conn, indexed)?;
            }
            Ok(())
        })();
        match insert_result {
            Ok(()) => {
                conn.execute_batch("COMMIT;")
                    .context("Failed to commit repo DB write transaction")?;
            }
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(err);
            }
        }

        self.create_repo_indexes(&mut conn)?;

        conn.close().map_err(|(_, e)| e)?;

        // Compress the database
        self.compress_db(&db_path, &compressed_db_path)?;

        // Remove the uncompressed DB
        fs::remove_file(&db_path)?;

        Ok(compressed_db_path)
    }

    pub(super) fn configure_repo_build_pragmas(&self, conn: &mut Connection) -> Result<()> {
        // Speed-focused settings are scoped to this temporary repo DB build process.
        conn.execute_batch(
            "PRAGMA synchronous = OFF;
             PRAGMA journal_mode = MEMORY;
             PRAGMA temp_store = MEMORY;
             PRAGMA locking_mode = EXCLUSIVE;
             PRAGMA cache_size = -200000;",
        )
        .context("Failed to apply SQLite build PRAGMAs for repo DB creation")?;
        Ok(())
    }

    pub(super) fn collect_repo_package_paths(&self) -> Result<Vec<PathBuf>> {
        let mut package_paths = Vec::new();
        for entry in fs::read_dir(&self.repo_dir)
            .with_context(|| format!("Failed to read {}", self.repo_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.to_string_lossy().ends_with(".depot.pkg.tar.zst") {
                package_paths.push(path);
            }
        }
        package_paths.sort();
        Ok(package_paths)
    }

    pub(super) fn collect_indexed_packages_parallel(
        &self,
        package_paths: &[PathBuf],
    ) -> Result<Vec<IndexedPackage>> {
        if package_paths.is_empty() {
            return Ok(Vec::new());
        }

        let worker_count = num_cpus().min(package_paths.len());
        crate::log_info!(
            "Using {} thread(s) to index {} package(s)...",
            worker_count,
            package_paths.len()
        );

        let next_index = AtomicUsize::new(0);
        let mut indexed = std::thread::scope(|scope| -> Result<Vec<(usize, IndexedPackage)>> {
            let (tx, rx) = mpsc::channel::<(usize, Result<IndexedPackage>)>();

            for _ in 0..worker_count {
                let tx = tx.clone();
                let next_index = &next_index;
                scope.spawn(move || {
                    loop {
                        let idx = next_index.fetch_add(1, Ordering::Relaxed);
                        if idx >= package_paths.len() {
                            break;
                        }
                        let result = self.read_indexed_package(&package_paths[idx]);
                        if tx.send((idx, result)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);

            let mut indexed = Vec::with_capacity(package_paths.len());
            for _ in 0..package_paths.len() {
                let (idx, result) = rx
                    .recv()
                    .context("Failed to receive package indexing result from worker")?;
                indexed.push((idx, result?));
            }
            Ok(indexed)
        })?;

        indexed.sort_by_key(|(idx, _)| *idx);
        Ok(indexed.into_iter().map(|(_, pkg)| pkg).collect())
    }

    pub(super) fn init_repo_schema(&self, conn: &mut Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE packages (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                real_name TEXT,
                version TEXT NOT NULL,
                revision INTEGER NOT NULL,
                abi_breaking INTEGER NOT NULL DEFAULT 0,
                built_against TEXT NOT NULL DEFAULT '',
                completed_at INTEGER,
                description TEXT,
                homepage TEXT,
                license TEXT,
                filename TEXT NOT NULL,
                size INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                sha512 TEXT NOT NULL
            );
            CREATE TABLE provides (
                package_id INTEGER,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE TABLE conflicts (
                package_id INTEGER,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE TABLE replaces (
                package_id INTEGER,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE TABLE dependencies (
                package_id INTEGER,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE TABLE groups (
                package_id INTEGER,
                name TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );
            CREATE TABLE files (
                package_id INTEGER,
                path TEXT NOT NULL,
                FOREIGN KEY(package_id) REFERENCES packages(id)
            );",
        )
        .context("Failed to initialize repo schema")?;
        Ok(())
    }

    pub(super) fn create_repo_indexes(&self, conn: &mut Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE INDEX idx_packages_name ON packages(name);
             CREATE INDEX idx_provides_name ON provides(name);
             CREATE INDEX idx_conflicts_name ON conflicts(name);
             CREATE INDEX idx_replaces_name ON replaces(name);
             CREATE INDEX idx_dependencies_name ON dependencies(name);
             CREATE INDEX idx_dependencies_kind ON dependencies(kind);
             CREATE INDEX idx_groups_name ON groups(name);
             CREATE INDEX idx_repo_files_path ON files(path);",
        )
        .context("Failed to create repo DB indexes")?;
        Ok(())
    }

    pub(super) fn read_indexed_package(&self, pkg_path: &Path) -> Result<IndexedPackage> {
        crate::log_info!("Indexing package {}...", pkg_path.display());

        let filename = pkg_path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("Invalid package filename: {}", pkg_path.display()))?
            .to_string();
        let file = fs::File::open(pkg_path)?;
        let size = file.metadata()?.len();
        let mut hashing_reader = HashingReader::new(file);

        let mut name = String::new();
        let mut real_name = None;
        let mut version = String::new();
        let mut revision = 1;
        let mut abi_breaking = false;
        let mut built_against = Vec::new();
        let mut completed_at = path_modified_unix_timestamp(pkg_path)?;
        let mut description = None;
        let mut homepage = None;
        let mut license = None;
        let mut provides = Vec::new();
        let mut conflicts = Vec::new();
        let mut replaces = Vec::new();
        let mut runtime_dependencies = Vec::new();
        let mut optional_dependencies = Vec::new();
        let mut groups = Vec::new();
        let mut archive_files = Vec::new();

        {
            let zstd_decoder = zstd::stream::read::Decoder::new(&mut hashing_reader)?;
            let mut archive = tar::Archive::new(zstd_decoder);
            for entry in archive.entries()? {
                let mut entry = entry?;
                let path = entry.path()?;
                let path_str = path.to_string_lossy().to_string();
                if path_str == ".metadata.toml" {
                    let mut content = String::new();
                    use std::io::Read;
                    entry.read_to_string(&mut content)?;
                    let metadata: toml::Value = toml::from_str(&content).with_context(|| {
                        format!("Failed to parse .metadata.toml in {}", pkg_path.display())
                    })?;

                    name = metadata
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    real_name = metadata
                        .get("real_name")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    version = metadata
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    revision = metadata
                        .get("revision")
                        .and_then(|v| v.as_integer())
                        .unwrap_or(1) as u32;
                    abi_breaking = metadata
                        .get("abi_breaking")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    built_against = parse_string_array_metadata(&metadata, "built_against");
                    completed_at =
                        metadata_time::parse_completed_at_value(&metadata).or(completed_at);
                    description = metadata
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    homepage = metadata
                        .get("homepage")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    license = parse_license_text(&metadata);

                    if let Some(provides_arr) = metadata.get("provides").and_then(|v| v.as_array())
                    {
                        provides = provides_arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect();
                    }
                    if let Some(conflicts_arr) =
                        metadata.get("conflicts").and_then(|v| v.as_array())
                    {
                        conflicts = conflicts_arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect();
                    }
                    if let Some(replaces_arr) = metadata.get("replaces").and_then(|v| v.as_array())
                    {
                        replaces = replaces_arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect();
                    }
                    if let Some(runtime_arr) = metadata
                        .get("dependencies")
                        .and_then(|v| v.get("runtime"))
                        .and_then(|v| v.as_array())
                    {
                        runtime_dependencies = runtime_arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect();
                    }
                    if let Some(optional_arr) = metadata
                        .get("dependencies")
                        .and_then(|v| v.get("optional"))
                        .and_then(|v| v.as_array())
                    {
                        optional_dependencies = optional_arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect();
                    }
                    if let Some(groups_arr) = metadata
                        .get("dependencies")
                        .and_then(|v| v.get("groups"))
                        .and_then(|v| v.as_array())
                    {
                        groups = groups_arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect();
                    }
                    continue;
                }

                let entry_type = entry.header().entry_type();
                if entry_type.is_file() || entry_type.is_symlink() || entry_type.is_hard_link() {
                    let normalized = path_str.trim_start_matches("./").to_string();
                    if normalized == ".metadata.toml" {
                        continue;
                    }
                    archive_files.push(normalized);
                }
            }
        }
        let (sha256, sha512) = hashing_reader.finalize_hex();

        if name.is_empty() {
            // Fallback for packages WITHOUT metadata (e.g. legacy or during transition)
            let name_parts: Vec<&str> = filename.split('-').collect();
            if name_parts.len() < 4 {
                anyhow::bail!(
                    "Invalid package filename and no .metadata.toml: {}",
                    filename
                );
            }
            name = name_parts[0].to_string();
            version = name_parts[1].to_string();
            revision = name_parts[2].parse().unwrap_or(1);
        }

        Ok(IndexedPackage {
            name,
            real_name,
            version,
            revision,
            abi_breaking,
            built_against,
            completed_at,
            description,
            homepage,
            license,
            filename,
            size,
            sha256,
            sha512,
            provides,
            conflicts,
            replaces,
            runtime_dependencies,
            optional_dependencies,
            groups,
            archive_files,
        })
    }

    pub(super) fn insert_indexed_package(
        &self,
        conn: &mut Connection,
        indexed: IndexedPackage,
    ) -> Result<()> {
        let IndexedPackage {
            name,
            real_name,
            version,
            revision,
            abi_breaking,
            built_against,
            completed_at,
            description,
            homepage,
            license,
            filename,
            size,
            sha256,
            sha512,
            provides,
            conflicts,
            replaces,
            runtime_dependencies,
            optional_dependencies,
            groups,
            archive_files,
        } = indexed;

        // Insert into database
        conn.execute(
            "INSERT INTO packages (name, real_name, version, revision, abi_breaking, built_against, completed_at, description, homepage, license, filename, size, sha256, sha512)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                name,
                real_name,
                version,
                revision as i64,
                abi_breaking,
                format_built_against(&built_against),
                completed_at,
                description,
                homepage,
                license,
                filename,
                size as i64,
                sha256,
                sha512
            ],
        )?;

        let package_id = conn.last_insert_rowid();

        // Insert into provides
        for provide in provides {
            conn.execute(
                "INSERT INTO provides (package_id, name) VALUES (?1, ?2)",
                params![package_id, provide],
            )?;
        }
        for conflict in conflicts {
            conn.execute(
                "INSERT INTO conflicts (package_id, name) VALUES (?1, ?2)",
                params![package_id, conflict],
            )?;
        }
        for replacement in replaces {
            conn.execute(
                "INSERT INTO replaces (package_id, name) VALUES (?1, ?2)",
                params![package_id, replacement],
            )?;
        }

        for dep in runtime_dependencies {
            conn.execute(
                "INSERT INTO dependencies (package_id, kind, name) VALUES (?1, 'runtime', ?2)",
                params![package_id, dep],
            )?;
        }
        for dep in optional_dependencies {
            conn.execute(
                "INSERT INTO dependencies (package_id, kind, name) VALUES (?1, 'optional', ?2)",
                params![package_id, dep],
            )?;
        }
        for group in groups {
            conn.execute(
                "INSERT INTO groups (package_id, name) VALUES (?1, ?2)",
                params![package_id, group],
            )?;
        }

        for file_path in archive_files {
            conn.execute(
                "INSERT INTO files (package_id, path) VALUES (?1, ?2)",
                params![package_id, file_path],
            )?;
        }

        Ok(())
    }

    pub(super) fn compress_db(&self, source: &Path, dest: &Path) -> Result<()> {
        let mut input = fs::File::open(source)?;
        let output = fs::File::create(dest)?;
        let mut encoder = Encoder::new(output, 19)?; // High compression for repo DB
        encoder.multithread(num_cpus() as u32)?;
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?;
        Ok(())
    }
}
