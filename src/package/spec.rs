//! Package specification structures and TOML parsing

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// Complete package specification from TOML
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PackageSpec {
    pub package: PackageInfo,
    /// Optional additional package outputs produced from the same spec/destdir
    #[serde(default)]
    pub packages: Vec<PackageInfo>,
    #[serde(default)]
    pub alternatives: Alternatives,
    /// Manual (local) sources to copy before fetching remote sources.
    #[serde(default)]
    pub manual_sources: Vec<ManualSource>,
    #[serde(default, deserialize_with = "deserialize_sources")]
    pub source: Vec<Source>,
    pub build: Build,
    #[serde(default)]
    pub dependencies: Dependencies,

    /// Directory containing the spec file (used to resolve relative paths such as patches).
    #[serde(skip)]
    pub spec_dir: PathBuf,
}

impl PackageSpec {
    /// Load package spec from a TOML file
    pub fn from_file(path: &Path) -> Result<Self> {
        // Canonicalize path to ensure spec_dir is absolute
        let abs_path = path
            .canonicalize()
            .with_context(|| format!("Failed to resolve path: {}", path.display()))?;

        let content = fs::read_to_string(&abs_path)
            .with_context(|| format!("Failed to read package spec: {}", abs_path.display()))?;
        let mut spec: PackageSpec = toml::from_str(&content)
            .with_context(|| format!("Failed to parse package spec: {}", abs_path.display()))?;
        spec.spec_dir = abs_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        // Require at least one source (remote or manual)
        if spec.source.is_empty() && spec.manual_sources.is_empty() {
            anyhow::bail!("Package must have at least one source or manual_sources entry");
        }
        spec.validate_manual_sources()?;

        Ok(spec)
    }

    fn validate_manual_sources(&self) -> Result<()> {
        for (idx, manual) in self.manual_sources.iter().enumerate() {
            let has_file = manual
                .file
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            let has_url = manual
                .url
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);

            match (has_file, has_url) {
                (true, false) | (false, true) => {}
                (false, false) => {
                    anyhow::bail!(
                        "manual_sources[{}] must specify exactly one of 'file' or 'url'",
                        idx
                    );
                }
                (true, true) => {
                    anyhow::bail!(
                        "manual_sources[{}] cannot specify both 'file' and 'url'",
                        idx
                    );
                }
            }
        }
        Ok(())
    }

    /// Expand variables like $name and $version in a string
    pub fn expand_vars(&self, input: &str) -> String {
        let specdir = self.spec_dir.to_string_lossy();
        input
            .replace("$name", &self.package.name)
            .replace("$version", &self.package.version)
            .replace("$specdir", &specdir)
            .replace("$DEPOT_SPECDIR", &specdir)
    }

    pub fn sources(&self) -> &[Source] {
        &self.source
    }

    /// Return all package outputs this spec will produce (primary + any extras)
    pub fn outputs(&self) -> Vec<PackageInfo> {
        let mut v = Vec::new();
        v.push(self.package.clone());
        v.extend(self.packages.clone());
        v
    }

    /// Apply system configuration overrides and appends
    pub fn apply_config(&mut self, config: &crate::config::Config) {
        // Apply build overrides from /etc/depot.d/build.toml
        self.apply_toml_overrides(&config.build_overrides, "build");

        // Apply appends from /etc/depot.d/build.toml (e.g. build.flags.cflags += ["-O3"])
        for (key, values) in &config.appends {
            if let Some(subkey) = key.strip_prefix("build.flags.") {
                self.apply_append(subkey, values);
            }
        }
    }

    fn apply_toml_overrides(&mut self, overrides: &toml::Value, _prefix: &str) {
        // Support both [build.flags] and top-level [build] fields
        if let Some(table) = overrides.as_table() {
            self.apply_flags_table(table);
        }
        if let Some(table) = overrides.get("flags").and_then(|f| f.as_table()) {
            self.apply_flags_table(table);
        }
    }

    fn apply_flags_table(&mut self, table: &toml::map::Map<String, toml::Value>) {
        for (k, v) in table {
            // match case-insensitively for common keys (allow CXX/Cc etc.)
            match k.to_lowercase().as_str() {
                "cflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags = vec![s.to_string()];
                    }
                }
                "ldflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.ldflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ldflags = vec![s.to_string()];
                    }
                }
                "keep" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.keep = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.keep = vec![s.to_string()];
                    }
                }
                "cc" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.cc = s.to_string();
                    }
                }
                "cxx" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.cxx = s.to_string();
                    }
                }
                "ar" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.ar = s.to_string();
                    }
                }
                "prefix" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.prefix = s.to_string();
                    }
                }
                "chost" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.chost = s.to_string();
                    }
                }
                "cbuild" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.cbuild = s.to_string();
                    }
                }
                "carch" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.carch = s.to_string();
                    }
                }
                "make_vars" | "make-vars" | "make_build_vars" | "make-build-vars" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_vars = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_vars =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_test_vars" | "make-test-vars" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_test_vars = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_test_vars =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_install_vars" | "make-install-vars" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_install_vars = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_install_vars =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "passthrough_env" | "passthrough-env" | "pass_env" | "pass-env" | "export_env"
                | "export-env" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.passthrough_env = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.passthrough_env =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "no_flags" | "no-flags" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_flags = b;
                    }
                }
                "skip_tests" | "skip-tests" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.skip_tests = b;
                    }
                }
                // Add more fields as needed
                _ => {}
            }
        }
    }

    fn apply_append(&mut self, key: &str, values: &[toml::Value]) {
        match key {
            "cflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags.push(s.to_string());
                    }
                }
            }
            "ldflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .ldflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ldflags.push(s.to_string());
                    }
                }
            }
            "keep" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .keep
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.keep.push(s.to_string());
                    }
                }
            }
            "configure" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .configure
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.configure.push(s.to_string());
                    }
                }
            }
            "cargs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cargs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cargs.push(s.to_string());
                    }
                }
            }
            "rustflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .rustflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.rustflags.push(s.to_string());
                    }
                }
            }
            "cc" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.cc = s.to_string();
                }
            }
            "ar" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.ar = s.to_string();
                }
            }
            "prefix" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.prefix = s.to_string();
                }
            }
            "chost" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.chost = s.to_string();
                }
            }
            "cbuild" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.cbuild = s.to_string();
                }
            }
            "carch" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.carch = s.to_string();
                }
            }
            "make_vars" | "make-vars" | "make_build_vars" | "make-build-vars" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_vars
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_vars
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_test_vars" | "make-test-vars" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_test_vars
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_test_vars
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_install_vars" | "make-install-vars" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_install_vars
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_install_vars
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "passthrough_env" | "passthrough-env" | "pass_env" | "pass-env" | "export_env"
            | "export-env" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .passthrough_env
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .passthrough_env
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "no_flags" | "no-flags" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_flags = b;
                }
            }
            "skip_tests" | "skip-tests" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.skip_tests = b;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    #[test]
    fn parse_single_source_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo-$version.tar.gz"
sha256 = "skip"
extract_dir = "foo-$version"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.package.name, "foo");
        assert_eq!(spec.sources().len(), 1);
        assert_eq!(
            spec.expand_vars(&spec.sources()[0].url),
            "https://example.com/foo-1.0.tar.gz"
        );
        assert!(spec.sources()[0].patches.is_empty());
        assert!(spec.sources()[0].post_extract.is_empty());
        assert_eq!(spec.spec_dir, tmp.path());
    }

    #[test]
    fn parse_source_array() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[[source]]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[[source]]
url = "https://example.com/bar.tar.gz"
sha256 = "skip"
extract_dir = "bar"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.sources().len(), 2);
        assert_eq!(spec.sources()[0].extract_dir, "foo");
        assert_eq!(spec.sources()[1].extract_dir, "bar");
    }

    #[test]
    fn parse_multiple_licenses() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = ["MIT", "Apache-2.0"]

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.package.license,
            vec!["MIT".to_string(), "Apache-2.0".to_string()]
        );
    }

    #[test]
    fn parse_rejects_empty_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        // `source = []` is not accepted (must have at least one entry)
        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

source = []

[build]
type = "custom"
"#,
        )
        .unwrap();

        assert!(PackageSpec::from_file(&path).is_err());
    }

    #[test]
    fn parse_manual_source_with_url() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[[manual_sources]]
url = "https://example.com/manual.patch"
sha256 = "skip"
dest = "patches/manual.patch"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.manual_sources.len(), 1);
        assert_eq!(
            spec.manual_sources[0].url.as_deref(),
            Some("https://example.com/manual.patch")
        );
        assert_eq!(spec.manual_sources[0].file, None);
    }

    #[test]
    fn parse_manual_source_rejects_missing_file_and_url() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[[manual_sources]]
sha256 = "skip"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let err = PackageSpec::from_file(&path).expect_err("spec should be rejected");
        assert!(err.to_string().contains("exactly one of 'file' or 'url'"));
    }

    #[test]
    fn parse_manual_source_rejects_file_and_url_together() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[[manual_sources]]
file = "manual.patch"
url = "https://example.com/manual.patch"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let err = PackageSpec::from_file(&path).expect_err("spec should be rejected");
        assert!(
            err.to_string()
                .contains("cannot specify both 'file' and 'url'")
        );
    }

    #[test]
    fn test_apply_config() {
        let mut spec = mk_spec("foo", "1.0");
        let mut config = crate::config::Config::for_rootfs(Path::new("/tmp/nonexistent"));

        // Mock some overrides and appends
        config.build_overrides = toml::from_str(
            r#"
[flags]
cc = "my-cc"
cflags = ["-O2"]
passthrough_env = ["RUSTFLAGS"]
make_vars = ["V=1"]
no_flags = true
skip_tests = true
keep = ["etc/locale.gen"]
"#,
        )
        .unwrap();
        config.appends.insert(
            "build.flags.cflags".to_string(),
            vec![toml::Value::String("-g".to_string())],
        );
        config.appends.insert(
            "build.flags.rustflags".to_string(),
            vec![toml::Value::Array(vec![
                toml::Value::String("-C".to_string()),
                toml::Value::String("opt-level=3".to_string()),
            ])],
        );
        config.appends.insert(
            "build.flags.keep".to_string(),
            vec![toml::Value::Array(vec![toml::Value::String(
                "etc/locale.gen".to_string(),
            )])],
        );
        config.appends.insert(
            "build.flags.passthrough_env".to_string(),
            vec![toml::Value::String("CARGO_HOME".to_string())],
        );
        config.appends.insert(
            "build.flags.make_test_vars".to_string(),
            vec![toml::Value::String("TESTS=smoke".to_string())],
        );
        config.appends.insert(
            "build.flags.make_install_vars".to_string(),
            vec![toml::Value::String("DESTDIR=/tmp/pkg".to_string())],
        );

        spec.apply_config(&config);

        assert_eq!(spec.build.flags.cc, "my-cc");
        assert!(spec.build.flags.cflags.contains(&"-O2".to_string()));
        assert!(spec.build.flags.cflags.contains(&"-g".to_string()));
        assert!(spec.build.flags.rustflags.contains(&"-C".to_string()));
        assert!(
            spec.build
                .flags
                .rustflags
                .contains(&"opt-level=3".to_string())
        );
        assert!(spec.build.flags.no_flags);
        assert!(
            spec.build
                .flags
                .keep
                .contains(&"etc/locale.gen".to_string())
        );
        assert!(
            spec.build
                .flags
                .passthrough_env
                .contains(&"RUSTFLAGS".to_string())
        );
        assert!(
            spec.build
                .flags
                .passthrough_env
                .contains(&"CARGO_HOME".to_string())
        );
        assert!(spec.build.flags.make_vars.contains(&"V=1".to_string()));
        assert!(spec.build.flags.skip_tests);
        assert!(
            spec.build
                .flags
                .make_test_vars
                .contains(&"TESTS=smoke".to_string())
        );
        assert!(
            spec.build
                .flags
                .make_install_vars
                .contains(&"DESTDIR=/tmp/pkg".to_string())
        );
    }

    #[test]
    fn parse_no_flags_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
no_flags = true
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.build.flags.no_flags);
    }

    #[test]
    fn parse_no_flags_alias_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
"no-flags" = true
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.build.flags.no_flags);
    }

    #[test]
    fn parse_skip_tests_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
skip_tests = true
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.build.flags.skip_tests);
    }

    #[test]
    fn parse_skip_tests_alias_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
"skip-tests" = true
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.build.flags.skip_tests);
    }

    #[test]
    fn parse_keep_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
keep = ["etc/locale.gen", "etc/resolv.conf"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.keep,
            vec!["etc/locale.gen".to_string(), "etc/resolv.conf".to_string()]
        );
    }

    #[test]
    fn parse_passthrough_env_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
passthrough_env = ["RUSTFLAGS", "CARGO_HOME"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.passthrough_env,
            vec!["RUSTFLAGS".to_string(), "CARGO_HOME".to_string()]
        );
    }

    #[test]
    fn parse_test_dependencies_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[dependencies]
build = ["make"]
test = ["python", "bats"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.dependencies.test,
            vec!["python".to_string(), "bats".to_string()]
        );
    }

    #[test]
    fn parse_make_var_overrides_from_spec() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
make_vars = ["V=1", "CC=clang"]
make_test_vars = ["TESTS=unit"]
make_install_vars = ["STRIPPROG=true"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.make_vars,
            vec!["V=1".to_string(), "CC=clang".to_string()]
        );
        assert_eq!(
            spec.build.flags.make_test_vars,
            vec!["TESTS=unit".to_string()]
        );
        assert_eq!(
            spec.build.flags.make_install_vars,
            vec!["STRIPPROG=true".to_string()]
        );
    }

    #[test]
    fn test_chost_cbuild_overrides() {
        let mut spec = mk_spec("foo", "1.0");
        let config = crate::config::Config {
            cache_dir: "/tmp".into(),
            build_dir: "/tmp".into(),
            db_dir: "/tmp".into(),
            build_overrides: toml::from_str(
                r#"
chost = "x86_64-sfg-linux-gnu"
cbuild = "x86_64-pc-linux-gnu"
"#,
            )
            .unwrap(),
            package_overrides: toml::Value::Table(toml::map::Map::new()),
            appends: std::collections::HashMap::new(),
            mirrors: std::collections::HashMap::new(),
            repo_clone_dir: PathBuf::from("/tmp"),
        };

        spec.apply_config(&config);
        assert_eq!(spec.build.flags.chost, "x86_64-sfg-linux-gnu");
        assert_eq!(spec.build.flags.cbuild, "x86_64-pc-linux-gnu");
    }

    #[test]
    fn test_default_and_override_carch() {
        let mut spec = mk_spec("foo", "1.0");
        // Default should be host arch
        assert_eq!(spec.build.flags.carch, std::env::consts::ARCH.to_string());

        // Override via config
        let mut config = crate::config::Config::for_rootfs(Path::new("/tmp/nonexistent"));
        config.build_overrides = toml::from_str(
            r#"[flags]
carch = "armv7"
"#,
        )
        .unwrap();
        spec.apply_config(&config);
        assert_eq!(spec.build.flags.carch, "armv7");
    }

    #[test]
    fn test_package_filename() {
        let mut spec = mk_spec("foo", "1.0");
        spec.package.revision = 2;
        assert_eq!(
            spec.package_filename("x86_64"),
            "foo-1.0-2-x86_64.depot.pkg.tar.zst"
        );
    }

    #[test]
    fn parse_packages_array() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[[packages]]
name = "foo-dev"
version = "1.0"
description = "development files"
homepage = "h"
license = "MIT"

[[source]]
url = "https://example.com/foo-1.0.tar.gz"
sha256 = "skip"
extract_dir = "foo-1.0"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        let outputs = spec.outputs();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "foo");
        assert_eq!(outputs[1].name, "foo-dev");
    }

    fn mk_spec(name: &str, version: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: name.into(),
                version: version.into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "h".into(),
                sha256: "s".into(),
                extract_dir: "e".into(),
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
}

impl fmt::Display for PackageSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Package: {} v{}",
            self.package.name, self.package.version
        )?;
        writeln!(f, "Description: {}", self.package.description)?;
        writeln!(f, "Homepage: {}", self.package.homepage)?;
        writeln!(f, "License: {}", self.package.license.join(", "))?;
        writeln!(f, "Sources: {}", self.source.len())?;
        writeln!(f, "Build Type: {:?}", self.build.build_type)?;
        if !self.alternatives.provides.is_empty() {
            writeln!(f, "Provides: {}", self.alternatives.provides.join(", "))?;
        }
        Ok(())
    }
}

/// Package metadata
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    /// Maintenance revision of the package (defaults to 1)
    #[serde(default = "default_revision")]
    pub revision: u32,
    pub description: String,
    pub homepage: String,
    #[serde(
        deserialize_with = "deserialize_licenses",
        serialize_with = "serialize_licenses"
    )]
    pub license: Vec<String>,
}

fn default_revision() -> u32 {
    1
}

fn deserialize_licenses<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrArray {
        String(String),
        Array(Vec<String>),
    }

    match StringOrArray::deserialize(deserializer)? {
        StringOrArray::String(s) => Ok(vec![s]),
        StringOrArray::Array(v) => Ok(v),
    }
}

fn serialize_licenses<S>(licenses: &[String], serializer: S) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if licenses.len() == 1 {
        serializer.serialize_str(&licenses[0])
    } else {
        licenses.serialize(serializer)
    }
}

impl PackageSpec {
    /// Generate the standard package filename: <name>-<version>-<revision>-<arch>.depot.pkg.tar.zst
    pub fn package_filename(&self, arch: &str) -> String {
        format!(
            "{}-{}-{}-{}.depot.pkg.tar.zst",
            self.package.name, self.package.version, self.package.revision, arch
        )
    }
}

/// Package alternatives (provides/replaces)
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Alternatives {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    /// Reserved for future package replacement feature
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[allow(dead_code)]
    pub replaces: Vec<String>,
}

/// Source tarball information
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Source {
    pub url: String,
    /// Checksum for the source (e.g. `sha256:...`, `sha512:...`, `md5:...`, or raw SHA256 hex).
    /// Use `skip` to bypass verification.
    pub sha256: String,
    /// Directory name after extraction (supports $name, $version)
    pub extract_dir: String,

    /// Patch files or URLs to apply after extraction.
    ///
    /// Example:
    /// patches = ["fix-build.patch", "<https://example.com/patches/foo.patch>"]
    #[serde(default)]
    pub patches: Vec<String>,

    /// Commands to run after extraction (and after patches), executed in the source directory.
    ///
    /// Example:
    /// post_extract = ["autoreconf -fi"]
    #[serde(default)]
    pub post_extract: Vec<String>,
}

/// Manual source copied before standard source fetching.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ManualSource {
    /// Filename in the spec directory (local manual source mode).
    #[serde(default)]
    pub file: Option<String>,
    /// Remote URL to fetch (remote manual source mode).
    #[serde(default)]
    pub url: Option<String>,
    /// Checksum (optional, use "skip" to bypass verification).
    #[serde(default)]
    pub sha256: Option<String>,
    /// Destination path relative to build work directory.
    /// Defaults to `file` for local mode or a derived filename for URL mode.
    #[serde(default)]
    pub dest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrManySources {
    One(Source),
    Many(Vec<Source>),
}

fn deserialize_sources<'de, D>(deserializer: D) -> std::result::Result<Vec<Source>, D::Error>
where
    D: Deserializer<'de>,
{
    // Try to deserialize; if the field is missing/null, return empty vec
    let parsed = Option::<OneOrManySources>::deserialize(deserializer)?;
    match parsed {
        Some(OneOrManySources::One(s)) => Ok(vec![s]),
        Some(OneOrManySources::Many(v)) => Ok(v),
        None => Ok(Vec::new()),
    }
}

/// Build configuration
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Build {
    #[serde(rename = "type")]
    pub build_type: BuildType,
    #[serde(default)]
    pub flags: BuildFlags,
}

/// Supported build systems
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum BuildType {
    Autotools,
    CMake,
    Meson,
    Custom,
    Rust,
    Makefile,
    Bin,
}

/// Build flags and toolchain configuration
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BuildFlags {
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub cflags: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub ldflags: Vec<String>,
    /// Keep existing files and install package-provided replacement as `<path>.depotnew`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub keep: Vec<String>,
    /// Disable exporting CFLAGS/CXXFLAGS/LDFLAGS for this package build.
    #[serde(default, alias = "no-flags")]
    pub no_flags: bool,
    /// Skip automatic build-system test execution (e.g. Autotools `make check`/`make test`).
    #[serde(default, alias = "skip-tests")]
    pub skip_tests: bool,
    #[serde(default)]
    pub configure: Vec<String>,
    /// C compiler
    #[serde(default = "default_cc")]
    pub cc: String,
    /// C++ compiler
    #[serde(default = "default_cxx")]
    pub cxx: String,
    /// Archiver
    #[serde(default = "default_ar")]
    pub ar: String,
    /// Dynamic loader path
    #[serde(default)]
    pub libc: String,
    /// Root filesystem for installation (per-package override)
    #[serde(default = "default_rootfs")]
    #[allow(dead_code)]
    pub rootfs: String,
    /// Commands to run after compile (after make, before make install)
    #[serde(default)]
    pub post_compile: Vec<String>,
    /// Commands to run after install (after make install)
    #[serde(default)]
    pub post_install: Vec<String>,

    /// Specific commands for 'makefile' build type
    #[serde(default)]
    pub makefile_commands: Vec<String>,
    #[serde(default)]
    pub makefile_install_commands: Vec<String>,

    /// Installation prefix (default: /usr)
    #[serde(default = "default_prefix")]
    pub prefix: String,

    /// Target architecture triple (CHOST equivalent)
    #[serde(default)]
    pub chost: String,

    /// Build architecture triple (CBUILD equivalent)
    #[serde(default)]
    pub cbuild: String,

    /// CPU architecture short name (CARCH equivalent), e.g. "x86_64", "aarch64"
    #[serde(default = "default_carch")]
    pub carch: String,
    /// Variable overrides passed directly to `make` (compile step), e.g. ["V=1", "CC=clang"].
    #[serde(
        default,
        alias = "make-vars",
        alias = "make_build_vars",
        alias = "make-build-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_vars: Vec<String>,
    /// Variable overrides passed directly to `make check` / `make test`.
    #[serde(
        default,
        alias = "make-test-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_test_vars: Vec<String>,
    /// Variable overrides passed directly to `make install`.
    #[serde(
        default,
        alias = "make-install-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_install_vars: Vec<String>,
    /// Additional host environment variable names to export unchanged to build commands.
    /// Example: ["RUSTFLAGS", "CARGO_HOME"].
    #[serde(
        default,
        alias = "passthrough-env",
        alias = "pass_env",
        alias = "pass-env",
        alias = "export_env",
        alias = "export-env",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub passthrough_env: Vec<String>,

    // Rust-specific fields
    /// Rust build profile: "debug" or "release" (default: release)
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Rust target triple (e.g., x86_64-unknown-linux-musl). Optional.
    #[serde(default)]
    pub target: String,
    /// RUSTFLAGS environment variable
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub rustflags: Vec<String>,
    /// Additional cargo arguments (short name)
    #[serde(default)]
    pub cargs: Vec<String>,
    /// Binary installation directory relative to DESTDIR (default: /usr/bin)
    #[serde(default = "default_bindir")]
    pub bindir: String,

    /// Subdirectory within extracted source to use as the actual source root.
    /// Useful for monorepos like llvm-project where you want to build just one component.
    #[serde(default)]
    pub source_subdir: String,
    /// Build directory relative to source root (e.g. "build")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_dir: Option<String>,
    /// Binary package type when using BuildType::Bin (e.g. "deb")
    #[serde(default)]
    pub binary_type: String,
}

impl Default for BuildFlags {
    fn default() -> Self {
        BuildFlags {
            cflags: Vec::new(),
            ldflags: Vec::new(),
            keep: Vec::new(),
            no_flags: false,
            skip_tests: false,
            configure: Vec::new(),
            cc: default_cc(),
            cxx: default_cxx(),
            ar: default_ar(),
            libc: String::new(),
            rootfs: default_rootfs(),
            post_compile: Vec::new(),
            post_install: Vec::new(),
            makefile_commands: Vec::new(),
            makefile_install_commands: Vec::new(),
            prefix: default_prefix(),
            chost: String::new(),
            cbuild: String::new(),
            carch: default_carch(),
            make_vars: Vec::new(),
            make_test_vars: Vec::new(),
            make_install_vars: Vec::new(),
            passthrough_env: Vec::new(),
            profile: default_profile(),
            target: String::new(),
            rustflags: Vec::new(),
            cargs: Vec::new(),
            bindir: default_bindir(),
            source_subdir: String::new(),
            build_dir: None,
            binary_type: String::new(),
        }
    }
}

fn deserialize_string_or_array<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrArray {
        String(String),
        Array(Vec<String>),
    }

    match Option::<StringOrArray>::deserialize(deserializer)? {
        Some(StringOrArray::String(s)) => Ok(s.split_whitespace().map(String::from).collect()),
        Some(StringOrArray::Array(a)) => Ok(a),
        None => Ok(Vec::new()),
    }
}

fn default_cc() -> String {
    // Prefer clang if available (supports -print-resource-dir and other useful flags)
    if std::process::Command::new("which")
        .arg("clang")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return "clang".to_string();
    }
    "gcc".to_string()
}

fn default_ar() -> String {
    "ar".to_string()
}

fn default_rootfs() -> String {
    "/".to_string()
}

fn default_profile() -> String {
    "release".to_string()
}

fn default_bindir() -> String {
    "/usr/bin".to_string()
}

fn default_prefix() -> String {
    "/usr".to_string()
}

fn default_carch() -> String {
    std::env::consts::ARCH.to_string()
}

fn default_cxx() -> String {
    // Infer a sensible C++ compiler name from default_cc()
    let cc = default_cc();
    if cc.contains("clang") {
        "clang++".to_string()
    } else {
        "g++".to_string()
    }
}

/// Package dependencies
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Dependencies {
    /// Dependencies required for building packages.
    #[serde(default)]
    pub build: Vec<String>,
    /// Dependencies required at runtime.
    #[serde(default)]
    pub runtime: Vec<String>,
    /// Dependencies required to run package test suites.
    #[serde(default)]
    pub test: Vec<String>,
}
