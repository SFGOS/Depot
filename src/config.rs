//! Global configuration for Depot

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

fn default_repo_db_filename() -> String {
    "repo.db.zst".to_string()
}

fn resolve_rootfs_base(rootfs: &Path) -> PathBuf {
    if rootfs.exists() {
        rootfs.canonicalize().unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(rootfs))
                .unwrap_or_else(|_| rootfs.to_path_buf())
        })
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(rootfs))
            .unwrap_or_else(|_| rootfs.to_path_buf())
    }
}

/// Global repo behavior settings loaded from `repos.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoSettings {
    /// Prefer binary repos over source repos when both can satisfy a request.
    #[serde(default)]
    pub prefer_binary: bool,
}

/// Source repository configuration entry loaded from `repos.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRepo {
    /// Git URL for the package repo.
    pub url: String,
    /// Whether the repo is enabled for lookups/sync.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Lower numbers are higher priority.
    #[serde(default)]
    pub priority: i32,
    /// Optional subdirectories to scan/index inside the git checkout.
    #[serde(default)]
    pub subdirs: Vec<String>,
}

/// Binary repository configuration entry loaded from `repos.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryRepoArchEntry {
    /// Whether this architecture is enabled for this repo.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional URL override for this architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Optional repo DB path override for this architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_db: Option<String>,
}

/// Binary repository configuration entry loaded from `repos.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryRepo {
    /// Base URL for the binary repository.
    pub url: String,
    /// Whether the repo is enabled for lookups.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Lower numbers are higher priority.
    #[serde(default)]
    pub priority: i32,
    /// Repo database filename/path relative to `url`.
    #[serde(default = "default_repo_db_filename")]
    pub repo_db: String,
    /// Architecture-specific overrides/enablement.
    ///
    /// Example:
    /// `[binary.core.arch.x86_64]`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub arch: BTreeMap<String, BinaryRepoArchEntry>,
    /// Allow unsigned repo metadata for this repo.
    #[serde(default)]
    pub allow_unsigned: bool,
}

impl Default for BinaryRepo {
    fn default() -> Self {
        Self {
            url: String::new(),
            enabled: true,
            priority: 0,
            repo_db: default_repo_db_filename(),
            arch: BTreeMap::new(),
            allow_unsigned: false,
        }
    }
}

impl BinaryRepo {
    /// Return true if this repo is enabled for the requested machine architecture.
    pub fn supports_arch(&self, machine_arch: &str) -> bool {
        if self.arch.is_empty() {
            return true;
        }
        self.arch
            .get(machine_arch)
            .map(|entry| entry.enabled)
            .unwrap_or(false)
    }

    /// Return the effective base URL for `machine_arch`, including arch overrides.
    pub fn effective_url_for_arch<'a>(&'a self, machine_arch: &str) -> Option<&'a str> {
        if self.arch.is_empty() {
            return Some(self.url.as_str());
        }
        let entry = self.arch.get(machine_arch)?;
        if !entry.enabled {
            return None;
        }
        Some(entry.url.as_deref().unwrap_or(self.url.as_str()))
    }

    /// Return the effective repo DB path for `machine_arch`, including arch overrides.
    pub fn effective_repo_db_for_arch<'a>(&'a self, machine_arch: &str) -> Option<&'a str> {
        if self.arch.is_empty() {
            return Some(self.repo_db.as_str());
        }
        let entry = self.arch.get(machine_arch)?;
        if !entry.enabled {
            return None;
        }
        Some(entry.repo_db.as_deref().unwrap_or(self.repo_db.as_str()))
    }
}

/// Parsed contents of `/etc/depot.d/repos.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoConfigFile {
    /// Global repo behavior settings.
    #[serde(default, skip_serializing_if = "repo_settings_is_default")]
    pub settings: RepoSettings,
    /// Source repos keyed by repo name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source: BTreeMap<String, SourceRepo>,
    /// Binary repos keyed by repo name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binary: BTreeMap<String, BinaryRepo>,
}

fn repo_settings_is_default(settings: &RepoSettings) -> bool {
    !settings.prefer_binary
}

/// Return the canonical `repos.toml` path for the given rootfs.
pub fn repos_toml_path(rootfs: &Path) -> PathBuf {
    resolve_rootfs_base(rootfs).join("etc/depot.d/repos.toml")
}

/// Load `repos.toml` for the given rootfs. Missing file returns defaults.
pub fn load_repos_config_file(rootfs: &Path) -> Result<RepoConfigFile> {
    let path = repos_toml_path(rootfs);
    if !path.exists() {
        return Ok(RepoConfigFile::default());
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let parsed: RepoConfigFile =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(parsed)
}

/// Save `repos.toml` for the given rootfs, creating `/etc/depot.d` as needed.
pub fn save_repos_config_file(rootfs: &Path, repos: &RepoConfigFile) -> Result<PathBuf> {
    let path = repos_toml_path(rootfs);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(repos).context("Failed to serialize repos.toml")?;
    fs::write(&path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

/// Global configuration settings
#[derive(Clone)]
pub struct Config {
    /// Directory for cached source tarballs
    pub cache_dir: PathBuf,
    /// Directory for building packages
    pub build_dir: PathBuf,
    /// Directory for package database
    pub db_dir: PathBuf,
    /// System-level build overrides from /etc/depot.d/build.toml
    pub build_overrides: toml::Value,
    /// System-level package overrides from /etc/depot.d/package.toml
    pub package_overrides: toml::Value,
    /// Appends found in system TOML files (key -> values to append)
    pub appends: HashMap<String, Vec<toml::Value>>,
    /// Parsed repo settings from `/etc/depot.d/repos.toml`.
    pub repo_settings: RepoSettings,
    /// Source repos from `/etc/depot.d/repos.toml` (and legacy mirrors fallback).
    pub source_repos: BTreeMap<String, SourceRepo>,
    /// Binary repos from `/etc/depot.d/repos.toml`.
    pub binary_repos: BTreeMap<String, BinaryRepo>,
    /// Mirrors mapping read from /etc/depot.d/mirrors.toml (reponame -> git url)
    pub mirrors: std::collections::HashMap<String, String>,
    /// Directory where git mirrors are cloned (absolute path). Defaults to
    /// <rootfs>/usr/src/depot unless overridden in depot.toml
    pub repo_clone_dir: PathBuf,
    /// Cache directory for binary packages and repo metadata.
    pub package_cache_dir: PathBuf,
    /// Install test dependencies alongside build/runtime dependencies.
    pub install_test_deps: bool,
}

impl Config {
    /// Create config with paths relative to the given rootfs
    pub fn for_rootfs(rootfs: &Path) -> Self {
        let abs_rootfs = resolve_rootfs_base(rootfs);

        let is_system_root = abs_rootfs == Path::new("/") || abs_rootfs.as_os_str() == "/";
        let is_root = crate::fakeroot::is_root();

        let (cache_dir, package_cache_dir, build_dir, db_dir) = if is_system_root && !is_root {
            let home = std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"));
            (
                home.join(".cache/depot/sources"),
                home.join(".cache/depot/packages"),
                home.join(".cache/depot/build"),
                home.join(".local/share/depot"),
            )
        } else {
            (
                abs_rootfs.join("var/cache/depot/sources"),
                abs_rootfs.join("var/cache/depot/packages"),
                abs_rootfs.join("var/cache/depot/build"),
                abs_rootfs.join("var/lib/depot"),
            )
        };

        let mut config = Self {
            cache_dir,
            build_dir,
            db_dir,
            build_overrides: toml::Value::Table(toml::map::Map::new()),
            package_overrides: toml::Value::Table(toml::map::Map::new()),
            appends: HashMap::new(),
            repo_settings: RepoSettings::default(),
            source_repos: BTreeMap::new(),
            binary_repos: BTreeMap::new(),
            mirrors: std::collections::HashMap::new(),
            repo_clone_dir: abs_rootfs.join("usr/src/depot"),
            package_cache_dir,
            install_test_deps: false,
        };

        if let Err(e) = config.load_system(&abs_rootfs) {
            crate::log_warn!("Failed to load system config: {}", e);
        }

        config
    }

    /// Return the package database path used to query what is installed in `rootfs`.
    ///
    /// For the live system root (`/`), non-root processes still read the system DB
    /// from `/var/lib/depot/packages.db` even though writable state is redirected
    /// to per-user directories under `$HOME`.
    pub fn installed_db_path(&self, rootfs: &Path) -> PathBuf {
        let abs_rootfs = resolve_rootfs_base(rootfs);
        abs_rootfs.join("var/lib/depot/packages.db")
    }

    /// Load system-level and user-level overrides
    pub fn load_system(&mut self, rootfs: &Path) -> Result<()> {
        // Load host system config (fallback) and then the requested rootfs config
        // so that rootfs-specific settings override host settings when present.
        let mut config_paths: Vec<PathBuf> = Vec::new();
        let host_etc = PathBuf::from("/etc/depot.toml");
        let rootfs_etc = rootfs.join("etc/depot.toml");

        if host_etc.exists() && host_etc != rootfs_etc {
            config_paths.push(host_etc);
        }
        config_paths.push(rootfs_etc);

        // Add user-level config paths (user override) — these should remain highest precedence
        if let Ok(home) = std::env::var("HOME") {
            let home_path = PathBuf::from(home);
            config_paths.push(home_path.join(".config/depot.toml"));
            config_paths.push(home_path.join(".local/share/depot.toml"));
        }

        for path in config_paths {
            if path.exists() {
                let content = fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read config: {}", path.display()))?;
                let (val, appends) = self.preprocess_toml(&content)?;

                // If it has a [build] section, merge it into build_overrides
                if let Some(build) = val.get("build") {
                    merge_toml_values(&mut self.build_overrides, build);
                }
                // If it has a [package] section, merge it into package_overrides
                if let Some(pkg) = val.get("package") {
                    merge_toml_values(&mut self.package_overrides, pkg);
                }
                if let Some(include_test_deps) = val
                    .get("install")
                    .and_then(|v| v.get("test_deps"))
                    .and_then(|v| v.as_bool())
                {
                    self.install_test_deps = include_test_deps;
                }

                for (k, v) in appends {
                    self.appends.insert(k, v);
                }
            }
        }

        // Keep existing etc/depot.d/ support.  Check host (/etc/depot.d) first as a
        // fallback, then the rootfs path so the rootfs can override host settings.
        let host_build = PathBuf::from("/etc/depot.d/build.toml");
        let build_path = rootfs.join("etc/depot.d/build.toml");

        if host_build.exists() && host_build != build_path {
            let content = fs::read_to_string(&host_build).with_context(|| {
                format!(
                    "Failed to read system build config: {}",
                    host_build.display()
                )
            })?;
            let (val, appends) = self.preprocess_toml(&content)?;
            merge_toml_values(&mut self.build_overrides, &val);
            for (k, v) in appends {
                self.appends.insert(format!("build.{}", k), v);
            }
        }

        if build_path.exists() {
            let content = fs::read_to_string(&build_path).with_context(|| {
                format!(
                    "Failed to read system build config: {}",
                    build_path.display()
                )
            })?;
            let (val, appends) = self.preprocess_toml(&content)?;
            merge_toml_values(&mut self.build_overrides, &val);
            for (k, v) in appends {
                self.appends.insert(format!("build.{}", k), v);
            }
        }

        let host_package = PathBuf::from("/etc/depot.d/package.toml");
        let package_path = rootfs.join("etc/depot.d/package.toml");

        if host_package.exists() && host_package != package_path {
            let content = fs::read_to_string(&host_package).with_context(|| {
                format!(
                    "Failed to read system package config: {}",
                    host_package.display()
                )
            })?;
            let (val, appends) = self.preprocess_toml(&content)?;
            merge_toml_values(&mut self.package_overrides, &val);
            for (k, v) in appends {
                self.appends.insert(format!("package.{}", k), v);
            }
        }

        if package_path.exists() {
            let content = fs::read_to_string(&package_path).with_context(|| {
                format!(
                    "Failed to read system package config: {}",
                    package_path.display()
                )
            })?;
            let (val, appends) = self.preprocess_toml(&content)?;
            merge_toml_values(&mut self.package_overrides, &val);
            for (k, v) in appends {
                self.appends.insert(format!("package.{}", k), v);
            }
        }

        // Load new repos file: /etc/depot.d/repos.toml (host fallback then rootfs override).
        let host_repos_path = PathBuf::from("/etc/depot.d/repos.toml");
        let repos_path = rootfs.join("etc/depot.d/repos.toml");
        for path in [host_repos_path, repos_path] {
            if !path.exists() {
                continue;
            }
            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read repos config: {}", path.display()))?;
            let parsed: RepoConfigFile = toml::from_str(&content)
                .with_context(|| format!("Failed to parse repos config: {}", path.display()))?;
            self.repo_settings = parsed.settings;
            for (name, repo) in parsed.source {
                self.source_repos.insert(name, repo);
            }
            for (name, repo) in parsed.binary {
                self.binary_repos.insert(name, repo);
            }
        }

        // Load legacy mirrors file: /etc/depot.d/mirrors.toml
        let mirrors_path = rootfs.join("etc/depot.d/mirrors.toml");
        if mirrors_path.exists() {
            let content = fs::read_to_string(&mirrors_path).with_context(|| {
                format!("Failed to read mirrors config: {}", mirrors_path.display())
            })?;
            let val: toml::Value = toml::from_str(&content)?;
            if let Some(table) = val.as_table() {
                for (k, v) in table {
                    if let Some(s) = v.as_str() {
                        self.mirrors.insert(k.clone(), s.to_string());
                        self.source_repos.entry(k.clone()).or_insert(SourceRepo {
                            url: s.to_string(),
                            enabled: true,
                            priority: 0,
                            subdirs: Vec::new(),
                        });
                    }
                }
            }
        }

        // Allow overriding repo clone dir via [repo] clone_dir in depot.toml
        if let Some(repo_table) = self.build_overrides.get("repo").and_then(|v| v.as_table())
            && let Some(clone_val) = repo_table.get("clone_dir").and_then(|v| v.as_str())
        {
            let p = PathBuf::from(clone_val);
            // If relative path, make it relative to rootfs
            let repo_dir = if p.is_absolute() { p } else { rootfs.join(p) };
            self.repo_clone_dir = repo_dir;
        }

        Ok(())
    }

    /// Return enabled source repos as `name -> git URL` pairs for legacy sync/status code.
    pub fn enabled_source_mirror_map(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        for (name, repo) in &self.source_repos {
            if repo.enabled {
                out.insert(name.clone(), repo.url.clone());
            }
        }
        out
    }
}

fn merge_toml_values(base: &mut toml::Value, over: &toml::Value) {
    if let (Some(base_table), Some(over_table)) = (base.as_table_mut(), over.as_table()) {
        for (k, v) in over_table {
            if v.is_table() && base_table.contains_key(k) && base_table[k].is_table() {
                merge_toml_values(&mut base_table[k], v);
            } else {
                base_table.insert(k.clone(), v.clone());
            }
        }
    }
}

impl Config {
    /// Preprocess TOML to support `key += value` syntax.
    /// Returns the base toml::Value and a map of append operations.
    fn preprocess_toml(
        &self,
        input: &str,
    ) -> Result<(toml::Value, HashMap<String, Vec<toml::Value>>)> {
        let mut base_text = String::new();
        let mut appends = HashMap::new();

        for line in input.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                base_text.push_str(line);
                base_text.push('\n');
                continue;
            }

            if let Some(plus_idx) = trimmed.find("+=") {
                let key = trimmed[..plus_idx].trim().to_string();
                let val_str = trimmed[plus_idx + 2..].trim();
                let val: toml::Value = toml::from_str::<toml::Value>(&format!("v = {}", val_str))
                    .context("Failed to parse append value")?
                    .get("v")
                    .cloned()
                    .unwrap();

                appends.entry(key).or_insert_with(Vec::new).push(val);
            } else {
                base_text.push_str(line);
                base_text.push('\n');
            }
        }

        let base_val: toml::Value = toml::from_str::<toml::Value>(&base_text)?;
        Ok((base_val, appends))
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::for_rootfs(Path::new("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn test_config_for_rootfs() {
        let root = PathBuf::from("/tmp/test_root");
        let config = Config::for_rootfs(&root);

        // Canonicalization might happen, so let's just check ends_with or construct reliably
        assert!(
            config
                .cache_dir
                .to_string_lossy()
                .contains("var/cache/depot/sources")
        );
        assert!(
            config
                .build_dir
                .to_string_lossy()
                .contains("var/cache/depot/build")
        );
        assert!(config.db_dir.to_string_lossy().contains("var/lib/depot"));
    }

    #[test]
    fn test_preprocess_toml() {
        let config = Config::for_rootfs(Path::new("/tmp/nonexistent"));
        let input = r#"
[flags]
cc = "clang"
cflags = ["-O2"]

# An append operation
cflags += ["-DDEBUG"]
ldflags += "-L/usr/local/lib"
"#;
        let (base, appends) = config.preprocess_toml(input).unwrap();

        // Base value should have the non-append parts
        assert_eq!(
            base.get("flags")
                .and_then(|f| f.get("cc"))
                .and_then(|v| v.as_str()),
            Some("clang")
        );
        assert_eq!(
            base.get("flags")
                .and_then(|f| f.get("cflags"))
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );

        // Appends should be captured
        assert!(appends.contains_key("cflags"));
        assert_eq!(appends.get("cflags").unwrap().len(), 1);
        assert_eq!(
            appends.get("cflags").unwrap()[0].as_array().unwrap()[0].as_str(),
            Some("-DDEBUG")
        );

        assert!(appends.contains_key("ldflags"));
        assert_eq!(
            appends.get("ldflags").unwrap()[0].as_str(),
            Some("-L/usr/local/lib")
        );
    }

    #[test]
    fn test_load_system() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let etc = root.join("etc/depot.d");
        fs::create_dir_all(&etc).unwrap();

        fs::write(
            etc.join("build.toml"),
            r#"
[flags]
cflags = ["-O2"]
cflags += ["-g"]
"#,
        )
        .unwrap();

        let config = Config::for_rootfs(root);
        assert_eq!(
            config
                .build_overrides
                .get("flags")
                .and_then(|f| f.get("cflags"))
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
        assert!(config.appends.contains_key("build.cflags"));
        assert_eq!(
            config.appends.get("build.cflags").unwrap()[0]
                .as_array()
                .unwrap()[0]
                .as_str(),
            Some("-g")
        );
    }

    #[test]
    fn test_config_non_root_fallback() {
        let fake_home = tempfile::tempdir().unwrap();
        let mut env = TestEnv::new();
        env.set_var("HOME", fake_home.path());

        let config = Config::for_rootfs(Path::new("/"));

        if !crate::fakeroot::is_root() {
            let home = std::env::var("HOME").unwrap();
            assert!(config.cache_dir.to_string_lossy().starts_with(&home));
            assert!(
                config
                    .cache_dir
                    .to_string_lossy()
                    .contains(".cache/depot/sources")
            );
        } else {
            // If running as root (e.g. in some CI), it should use /var
            assert_eq!(config.cache_dir, PathBuf::from("/var/cache/depot/sources"));
        }
    }

    #[test]
    fn test_installed_db_path_targets_rootfs_db() {
        let root = PathBuf::from("/tmp/test_root");
        let config = Config::for_rootfs(&root);
        assert_eq!(
            config.installed_db_path(&root),
            PathBuf::from("/tmp/test_root/var/lib/depot/packages.db")
        );
    }

    #[test]
    fn test_installed_db_path_uses_system_db_for_live_root() {
        let config = Config::for_rootfs(Path::new("/"));
        assert_eq!(
            config.installed_db_path(Path::new("/")),
            PathBuf::from("/var/lib/depot/packages.db")
        );
    }

    #[test]
    fn test_load_depot_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let etc = root.join("etc");
        fs::create_dir_all(&etc).unwrap();

        fs::write(
            etc.join("depot.toml"),
            r#"
[build]
prefix = "/opt/depot"
cc = "clang"

[build.flags]
cflags = ["-O3"]

[install]
test_deps = true
"#,
        )
        .unwrap();

        let config = Config::for_rootfs(root);
        // Config construction calls load_system automatically

        assert_eq!(
            config
                .build_overrides
                .get("prefix")
                .and_then(|v| v.as_str()),
            Some("/opt/depot")
        );
        assert_eq!(
            config.build_overrides.get("cc").and_then(|v| v.as_str()),
            Some("clang")
        );
        assert_eq!(
            config
                .build_overrides
                .get("flags")
                .and_then(|f| f.get("cflags"))
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
        assert!(config.install_test_deps);
    }

    #[test]
    fn test_load_repos_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let etc = root.join("etc/depot.d");
        fs::create_dir_all(&etc).unwrap();

        fs::write(
            etc.join("repos.toml"),
            r#"
[settings]
prefer_binary = true

[source.vertex]
url = "https://gitlab.com/vertex-linux/packages.git"
enabled = true
priority = 10
subdirs = ["core", "extra"]

[binary.vertex]
url = "https://repo.example.invalid"
enabled = false
priority = 5
repo_db = "repo.db.zst"

[binary.vertex.arch.x86_64]
enabled = true
"#,
        )
        .unwrap();

        let config = Config::for_rootfs(root);
        assert!(config.repo_settings.prefer_binary);
        let src = config.source_repos.get("vertex").unwrap();
        assert_eq!(src.url, "https://gitlab.com/vertex-linux/packages.git");
        assert_eq!(src.subdirs, vec!["core".to_string(), "extra".to_string()]);
        let bin = config.binary_repos.get("vertex").unwrap();
        assert_eq!(bin.url, "https://repo.example.invalid");
        assert!(!bin.enabled);
        assert!(bin.arch.contains_key("x86_64"));
        assert!(bin.supports_arch("x86_64"));
        assert!(!bin.supports_arch("aarch64"));
    }

    #[test]
    fn test_save_and_load_repos_config_file_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let mut repos = RepoConfigFile::default();
        repos.source.insert(
            "vertex".to_string(),
            SourceRepo {
                url: "https://gitlab.com/vertex-linux/packages.git".to_string(),
                enabled: true,
                priority: 1,
                subdirs: vec!["core".to_string()],
            },
        );
        repos.binary.insert(
            "vertex".to_string(),
            BinaryRepo {
                url: "https://repo.example.invalid".to_string(),
                enabled: true,
                priority: 2,
                repo_db: "repo.db.zst".to_string(),
                arch: {
                    let mut map = BTreeMap::new();
                    map.insert("x86_64".to_string(), BinaryRepoArchEntry::default());
                    map
                },
                allow_unsigned: false,
            },
        );

        let path = save_repos_config_file(root, &repos).unwrap();
        assert!(path.exists());

        let loaded = load_repos_config_file(root).unwrap();
        assert!(loaded.source.contains_key("vertex"));
        assert!(loaded.binary.contains_key("vertex"));
    }
}
