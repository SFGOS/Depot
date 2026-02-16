//! Global configuration for Depot

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Global configuration settings
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
}

impl Config {
    /// Create config with paths relative to the given rootfs
    pub fn for_rootfs(rootfs: &Path) -> Self {
        let abs_rootfs = if rootfs.exists() {
            rootfs.canonicalize().unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|cwd| cwd.join(rootfs))
                    .unwrap_or_else(|_| rootfs.to_path_buf())
            })
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(rootfs))
                .unwrap_or_else(|_| rootfs.to_path_buf())
        };

        let is_system_root = abs_rootfs == Path::new("/") || abs_rootfs.as_os_str() == "/";
        let is_root = crate::fakeroot::is_root();

        let (cache_dir, build_dir, db_dir) = if is_system_root && !is_root {
            let home = std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"));
            (
                home.join(".cache/depot/sources"),
                home.join(".cache/depot/build"),
                home.join(".local/share/depot"),
            )
        } else {
            (
                abs_rootfs.join("var/cache/depot/sources"),
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
        };

        if let Err(e) = config.load_system(&abs_rootfs) {
            eprintln!("Warning: Failed to load system config: {}", e);
        }

        config
    }

    /// Load system-level and user-level overrides
    pub fn load_system(&mut self, rootfs: &Path) -> Result<()> {
        let mut config_paths = vec![rootfs.join("etc/depot.toml")];

        // Add user-level config paths if we are not root or if HOME is set
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

                for (k, v) in appends {
                    self.appends.insert(k, v);
                }
            }
        }

        // Keep existing etc/depot.d/ support for backward compatibility/modular config
        let build_path = rootfs.join("etc/depot.d/build.toml");
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

        let package_path = rootfs.join("etc/depot.d/package.toml");
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

        Ok(())
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
        // We can't easily mock is_root() here without more complex infrastructure,
        // but we can at least check if the logic for abs_rootfs == "/" triggers
        // the HOME join if we were non-root.
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
    }
}
