//! Package specification structures and TOML parsing

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use std::collections::{BTreeMap, HashSet};
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
    /// Optional per-output alternatives/provides overrides keyed by package name.
    ///
    /// Example:
    /// [package_alternatives.clang]
    /// provides = ["cc", "c++", "gcc"]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_alternatives: BTreeMap<String, Alternatives>,
    /// Optional per-output dependency overrides keyed by package name.
    ///
    /// Example:
    /// [package_dependencies.clang]
    /// runtime = ["llvm-libs", "llvm-libgcc"]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_dependencies: BTreeMap<String, Dependencies>,

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
        let (base_content, appends) =
            preprocess_spec_toml_appends(&content).with_context(|| {
                format!("Failed to preprocess package spec: {}", abs_path.display())
            })?;
        let mut unknown_key = None;
        let deserializer = toml::Deserializer::parse(&base_content)
            .with_context(|| format!("Failed to parse package spec: {}", abs_path.display()))?;
        let mut spec: PackageSpec = serde_ignored::deserialize(deserializer, |path| {
            if unknown_key.is_none() {
                unknown_key = Some(path.to_string());
            }
        })
        .with_context(|| format!("Failed to parse package spec: {}", abs_path.display()))?;
        if let Some(path) = unknown_key {
            anyhow::bail!(
                "Failed to parse package spec: {}: unknown key: {}",
                abs_path.display(),
                path
            );
        }
        spec.spec_dir = abs_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        spec.apply_spec_appends(&appends)?;

        // Require at least one source (remote or manual) unless this is a metapackage.
        if spec.source.is_empty() && spec.manual_sources.is_empty() && !spec.is_metapackage() {
            anyhow::bail!(
                "Package must have at least one source or manual_sources entry (except build.type = \"meta\")"
            );
        }
        spec.validate_manual_sources()?;

        Ok(spec)
    }

    fn apply_spec_appends(
        &mut self,
        appends: &std::collections::HashMap<String, Vec<toml::Value>>,
    ) -> Result<()> {
        for (key, values) in appends {
            if let Some(subkey) = key.strip_prefix("build.flags.") {
                self.apply_append(subkey, values);
                continue;
            }
            if let Some(subkey) = key.strip_prefix("flags.") {
                self.apply_append(subkey, values);
                continue;
            }
            if !key.contains('.') {
                self.apply_append(key, values);
                continue;
            }
            anyhow::bail!("Unsupported '+=' key in package spec: {}", key);
        }
        Ok(())
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
            let file_count = manual.files.iter().filter(|s| !s.trim().is_empty()).count();
            let url_count = manual.urls.iter().filter(|s| !s.trim().is_empty()).count();
            let local_count = usize::from(has_file) + file_count;
            let remote_count = usize::from(has_url) + url_count;

            if local_count == 0 && remote_count == 0 {
                anyhow::bail!(
                    "manual_sources[{}] must specify one of 'file', 'files', 'url', or 'urls'",
                    idx
                );
            }
            if local_count > 0 && remote_count > 0 {
                anyhow::bail!(
                    "manual_sources[{}] cannot mix local ('file'/'files') and remote ('url'/'urls') entries",
                    idx
                );
            }
            if (local_count > 1 || remote_count > 1)
                && manual.dest.as_ref().is_some_and(|d| !d.trim().is_empty())
            {
                anyhow::bail!(
                    "manual_sources[{}] cannot use 'dest' with multiple entries in one block",
                    idx
                );
            }
            if (local_count > 1 || remote_count > 1)
                && manual
                    .sha256
                    .as_ref()
                    .is_some_and(|h| !h.trim().is_empty() && h.trim() != "skip")
            {
                anyhow::bail!(
                    "manual_sources[{}] cannot use one 'sha256' for multiple entries in one block",
                    idx
                );
            }
        }
        Ok(())
    }

    /// Expand variables like `$name` and `$version` in a string.
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

    /// Returns true when this spec is a metadata-only package that exists to pull dependencies.
    pub fn is_metapackage(&self) -> bool {
        matches!(self.build.build_type, BuildType::Meta)
    }

    /// Return all declared package outputs for this spec (primary + any extras).
    pub fn outputs(&self) -> Vec<PackageInfo> {
        let mut v = Vec::new();
        v.push(self.package.clone());
        v.extend(self.packages.clone());
        v
    }

    /// Return the derived documentation package name for an output package.
    pub fn docs_package_name(pkg_name: &str) -> String {
        format!("{pkg_name}-docs")
    }

    /// Build package metadata for an automatically generated documentation output.
    pub fn docs_package_for_output(&self, output: &PackageInfo) -> PackageInfo {
        let mut docs = output.clone();
        docs.name = Self::docs_package_name(&output.name);
        docs.description = format!("Documentation for {}", output.name);
        docs
    }

    fn docs_parent_output_name(&self, pkg_name: &str) -> Option<String> {
        if !self.build.flags.split_docs {
            return None;
        }

        let base = pkg_name.strip_suffix("-docs")?;
        self.outputs()
            .into_iter()
            .find(|output| output.name == base)
            .map(|output| output.name)
    }

    /// Return dependencies for a specific output package name.
    ///
    /// If no per-output override exists, returns the top-level dependencies.
    pub fn dependencies_for_output(&self, pkg_name: &str) -> Dependencies {
        if pkg_name == self.lib32_package_name() {
            return self
                .package_dependencies
                .get(pkg_name)
                .cloned()
                .unwrap_or_else(|| self.lib32_dependencies());
        }

        if let Some(parent_output) = self.docs_parent_output_name(pkg_name) {
            return self
                .package_dependencies
                .get(pkg_name)
                .cloned()
                .unwrap_or_else(|| {
                    let mut deps = Dependencies::default();
                    deps.runtime.push(parent_output);
                    deps
                });
        }

        self.package_dependencies
            .get(pkg_name)
            .cloned()
            .unwrap_or_else(|| self.dependencies.primary_dependencies())
    }

    /// Return the generated lib32 companion package name for this spec.
    pub fn lib32_package_name(&self) -> String {
        format!("lib32-{}", self.package.name)
    }

    /// Return the effective dependency set used by the generated lib32 companion package.
    pub fn lib32_dependencies(&self) -> Dependencies {
        let mut deps = self
            .dependencies
            .lib32_dependencies()
            .unwrap_or_else(|| self.dependencies.primary_dependencies());
        if !deps.runtime.iter().any(|dep| dep == &self.package.name) {
            deps.runtime.push(self.package.name.clone());
        }
        deps
    }

    /// Return local package names/provided features for the selected output set.
    pub fn local_dependency_provides_for_selection(
        &self,
        include_primary_outputs: bool,
        include_lib32_output: bool,
    ) -> HashSet<String> {
        let mut names = HashSet::new();
        if include_primary_outputs {
            for output in self.outputs() {
                let output_name = output.name.clone();
                names.insert(output_name.clone());
                let alternatives = self.alternatives_for_output(&output_name);
                for provided in alternatives.provides {
                    names.insert(provided);
                }
            }
        }
        if include_lib32_output {
            let output_name = self.lib32_package_name();
            names.insert(output_name.clone());
            let alternatives = self.alternatives_for_output(&output_name);
            for provided in alternatives.provides {
                names.insert(provided);
            }
        }
        names
    }

    /// Return alternatives/provides for a specific output package name.
    ///
    /// If no per-output override exists, returns the top-level alternatives.
    pub fn alternatives_for_output(&self, pkg_name: &str) -> Alternatives {
        if self.docs_parent_output_name(pkg_name).is_some() {
            return self
                .package_alternatives
                .get(pkg_name)
                .cloned()
                .unwrap_or_default();
        }

        self.package_alternatives
            .get(pkg_name)
            .cloned()
            .unwrap_or_else(|| self.alternatives.clone())
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
                "replace_cflags" | "replace-cflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags = vec![s.to_string()];
                    }
                }
                "cflags-lib32" | "cflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags_lib32 = vec![s.to_string()];
                    }
                }
                "replace_cflags-lib32" | "replace_cflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags_lib32 = vec![s.to_string()];
                    }
                }
                "cxxflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cxxflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags = vec![s.to_string()];
                    }
                }
                "replace_cxxflags" | "replace-cxxflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cxxflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags = vec![s.to_string()];
                    }
                }
                "cxxflags-lib32" | "cxxflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cxxflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags_lib32 = vec![s.to_string()];
                    }
                }
                "replace_cxxflags-lib32" | "replace_cxxflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cxxflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags_lib32 = vec![s.to_string()];
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
                "replace_ldflags" | "replace-ldflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_ldflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ldflags = vec![s.to_string()];
                    }
                }
                "ltoflags" | "lto_flags" | "lto-flags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.ltoflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ltoflags = vec![s.to_string()];
                    }
                }
                "replace_ltoflags" | "replace_lto-flags" | "replace_lto_flags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_ltoflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ltoflags = vec![s.to_string()];
                    }
                }
                "rustflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.rustflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.rustflags = vec![s.to_string()];
                    }
                }
                "replace_rustflags" | "replace-rustflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_rustflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_rustflags = vec![s.to_string()];
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
                "split_docs" | "split-docs" => {
                    if let Some(b) = toml_value_as_boolish(v) {
                        self.build.flags.split_docs = b;
                    }
                }
                "doc_dirs" | "doc-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.doc_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.doc_dirs = vec![s.to_string()];
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
                "ld" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.ld = s.to_string();
                    }
                }
                "cpp" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.cpp = s.to_string();
                    }
                }
                "prefix" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.prefix = s.to_string();
                    }
                }
                "bindir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.bindir = s.to_string();
                    }
                }
                "sbindir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.sbindir = s.to_string();
                    }
                }
                "libdir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.libdir = s.to_string();
                    }
                }
                "libexecdir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.libexecdir = s.to_string();
                    }
                }
                "sysconfdir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.sysconfdir = s.to_string();
                    }
                }
                "localstatedir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.localstatedir = s.to_string();
                    }
                }
                "sharedstatedir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.sharedstatedir = s.to_string();
                    }
                }
                "includedir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.includedir = s.to_string();
                    }
                }
                "datarootdir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.datarootdir = s.to_string();
                    }
                }
                "datadir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.datadir = s.to_string();
                    }
                }
                "mandir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.mandir = s.to_string();
                    }
                }
                "infodir" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.infodir = s.to_string();
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
                "makeflags" | "make_flags" | "make-flags" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.makeflags = s.to_string();
                    } else if let Some(arr) = v.as_array() {
                        self.build.flags.makeflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ");
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
                "make_exec" | "make-exec" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_exec = s.to_string();
                    }
                }
                "make_target" | "make-target" | "make_build_target" | "make-build-target" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_target = s.to_string();
                    }
                }
                "make_targets" | "make-targets" | "make_build_targets" | "make-build-targets" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_targets = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_targets =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_dirs" | "make-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_dirs =
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
                "make_test_target" | "make-test-target" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_test_target = s.to_string();
                    }
                }
                "make_test_targets" | "make-test-targets" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_test_targets = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_test_targets =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_test_dirs" | "make-test-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_test_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_test_dirs =
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
                "make_install_target" | "make-install-target" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_install_target = s.to_string();
                    }
                }
                "make_install_targets" | "make-install-targets" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_install_targets = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_install_targets =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_install_dirs" | "make-install-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_install_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_install_dirs =
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
                "use_lto" | "use-lto" => {
                    if let Some(b) = toml_value_as_boolish(v) {
                        self.build.flags.use_lto = b;
                    }
                }
                "no_strip" | "no-strip" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_strip = b;
                    }
                }
                "no_delete_static" | "no-delete-static" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_delete_static = b;
                    }
                }
                "no_compress_man"
                | "no-compress-man"
                | "no_compress_manpages"
                | "no-compress-manpages" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_compress_man = b;
                    }
                }
                "skip_tests" | "skip-tests" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.skip_tests = b;
                    }
                }
                "build_32" | "build-32" => {
                    if let Some(b) = toml_value_as_boolish(v) {
                        self.build.flags.build_32 = b;
                    }
                }
                "configure_lib32" | "configure-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.configure_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.configure_lib32 = vec![s.to_string()];
                    }
                }
                "config_setting" | "config_settings" | "config-setting" | "config-settings" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.config_settings = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.config_settings = vec![s.to_string()];
                    }
                }
                "configure_file" | "configure-file" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.configure_file = s.to_string();
                    }
                }
                "post_configure" | "post-configure" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_configure = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure = vec![s.to_string()];
                    }
                }
                "post_configure_lib32" | "post_configure-lib32" | "post-configure-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_configure_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure_lib32 = vec![s.to_string()];
                    }
                }
                "post_compile" | "post-compile" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_compile = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile = vec![s.to_string()];
                    }
                }
                "post_compile_lib32" | "post_compile-lib32" | "post-compile-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_compile_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile_lib32 = vec![s.to_string()];
                    }
                }
                "post_install" | "post-install" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_install = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install = vec![s.to_string()];
                    }
                }
                "post_install_lib32" | "post_install-lib32" | "post-install-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_install_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install_lib32 = vec![s.to_string()];
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
            "replace_cflags" | "replace-cflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags.push(s.to_string());
                    }
                }
            }
            "cflags-lib32" | "cflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags_lib32.push(s.to_string());
                    }
                }
            }
            "replace_cflags-lib32" | "replace_cflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags_lib32.push(s.to_string());
                    }
                }
            }
            "cxxflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cxxflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags.push(s.to_string());
                    }
                }
            }
            "replace_cxxflags" | "replace-cxxflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cxxflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags.push(s.to_string());
                    }
                }
            }
            "cxxflags-lib32" | "cxxflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cxxflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags_lib32.push(s.to_string());
                    }
                }
            }
            "replace_cxxflags-lib32" | "replace_cxxflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cxxflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags_lib32.push(s.to_string());
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
            "replace_ldflags" | "replace-ldflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_ldflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ldflags.push(s.to_string());
                    }
                }
            }
            "ltoflags" | "lto_flags" | "lto-flags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .ltoflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ltoflags.push(s.to_string());
                    }
                }
            }
            "replace_ltoflags" | "replace_lto-flags" | "replace_lto_flags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_ltoflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ltoflags.push(s.to_string());
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
            "doc_dirs" | "doc-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .doc_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.doc_dirs.push(s.to_string());
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
            "configure_lib32" | "configure-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .configure_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.configure_lib32.push(s.to_string());
                    }
                }
            }
            "config_setting" | "config_settings" | "config-setting" | "config-settings" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .config_settings
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.config_settings.push(s.to_string());
                    }
                }
            }
            "configure_file" | "configure-file" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.configure_file = s.to_string();
                }
            }
            "post_configure" | "post-configure" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_configure
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure.push(s.to_string());
                    }
                }
            }
            "post_configure_lib32" | "post_configure-lib32" | "post-configure-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_configure_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure_lib32.push(s.to_string());
                    }
                }
            }
            "post_compile" | "post-compile" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_compile
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile.push(s.to_string());
                    }
                }
            }
            "post_compile_lib32" | "post_compile-lib32" | "post-compile-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_compile_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile_lib32.push(s.to_string());
                    }
                }
            }
            "post_install" | "post-install" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_install
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install.push(s.to_string());
                    }
                }
            }
            "post_install_lib32" | "post_install-lib32" | "post-install-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_install_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install_lib32.push(s.to_string());
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
            "replace_rustflags" | "replace-rustflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_rustflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_rustflags.push(s.to_string());
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
            "ld" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.ld = s.to_string();
                }
            }
            "cpp" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.cpp = s.to_string();
                }
            }
            "prefix" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.prefix = s.to_string();
                }
            }
            "bindir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.bindir = s.to_string();
                }
            }
            "sbindir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.sbindir = s.to_string();
                }
            }
            "libdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.libdir = s.to_string();
                }
            }
            "libexecdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.libexecdir = s.to_string();
                }
            }
            "sysconfdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.sysconfdir = s.to_string();
                }
            }
            "localstatedir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.localstatedir = s.to_string();
                }
            }
            "sharedstatedir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.sharedstatedir = s.to_string();
                }
            }
            "includedir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.includedir = s.to_string();
                }
            }
            "datarootdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.datarootdir = s.to_string();
                }
            }
            "datadir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.datadir = s.to_string();
                }
            }
            "mandir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.mandir = s.to_string();
                }
            }
            "infodir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.infodir = s.to_string();
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
            "makeflags" | "make_flags" | "make-flags" | "MAKEFLAGS" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        let joined = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ");
                        append_whitespace_separated(&mut self.build.flags.makeflags, &joined);
                    } else if let Some(s) = v.as_str() {
                        append_whitespace_separated(&mut self.build.flags.makeflags, s);
                    }
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
            "make_exec" | "make-exec" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_exec = s.to_string();
                }
            }
            "make_target" | "make-target" | "make_build_target" | "make-build-target" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_target = s.to_string();
                }
            }
            "make_targets" | "make-targets" | "make_build_targets" | "make-build-targets" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_targets
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_targets
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_dirs" | "make-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_dirs
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
            "make_test_target" | "make-test-target" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_test_target = s.to_string();
                }
            }
            "make_test_targets" | "make-test-targets" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_test_targets
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_test_targets
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_test_dirs" | "make-test-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_test_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_test_dirs
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
            "make_install_target" | "make-install-target" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_install_target = s.to_string();
                }
            }
            "make_install_targets" | "make-install-targets" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_install_targets
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_install_targets
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_install_dirs" | "make-install-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_install_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_install_dirs
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
            "use_lto" | "use-lto" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.use_lto = b;
                }
            }
            "no_strip" | "no-strip" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_strip = b;
                }
            }
            "no_delete_static" | "no-delete-static" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_delete_static = b;
                }
            }
            "no_compress_man"
            | "no-compress-man"
            | "no_compress_manpages"
            | "no-compress-manpages" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_compress_man = b;
                }
            }
            "skip_tests" | "skip-tests" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.skip_tests = b;
                }
            }
            "build_32" | "build-32" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.build_32 = b;
                }
            }
            "split_docs" | "split-docs" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.split_docs = b;
                }
            }
            _ => {}
        }
    }
}

fn preprocess_spec_toml_appends(
    input: &str,
) -> Result<(String, std::collections::HashMap<String, Vec<toml::Value>>)> {
    let mut base_text = String::new();
    let mut appends = std::collections::HashMap::new();
    let mut current_table: Option<String> = None;
    let mut in_array_table = false;

    for line in input.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("[[") && trimmed.ends_with("]]") && trimmed.len() >= 4 {
            current_table = Some(trimmed[2..trimmed.len() - 2].trim().to_string());
            in_array_table = true;
            base_text.push_str(line);
            base_text.push('\n');
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
            current_table = Some(trimmed[1..trimmed.len() - 1].trim().to_string());
            in_array_table = false;
            base_text.push_str(line);
            base_text.push('\n');
            continue;
        }

        if trimmed.is_empty() || trimmed.starts_with('#') {
            base_text.push_str(line);
            base_text.push('\n');
            continue;
        }

        if let Some(plus_idx) = trimmed.find("+=") {
            if in_array_table {
                anyhow::bail!(
                    "'+=' is not supported inside array-of-table sections ({})",
                    current_table.as_deref().unwrap_or("")
                );
            }
            let key = trimmed[..plus_idx].trim();
            let val_str = trimmed[plus_idx + 2..].trim();
            let val: toml::Value = toml::from_str::<toml::Value>(&format!("v = {}", val_str))
                .context("Failed to parse append value")?
                .get("v")
                .cloned()
                .unwrap();

            let full_key = if key.contains('.') {
                key.to_string()
            } else if let Some(table) = current_table.as_deref() {
                format!("{}.{}", table, key)
            } else {
                key.to_string()
            };

            appends.entry(full_key).or_insert_with(Vec::new).push(val);
            // Preserve line numbering for parser diagnostics.
            base_text.push('\n');
            continue;
        }

        base_text.push_str(line);
        base_text.push('\n');
    }

    Ok((base_text, appends))
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
        assert!(spec.sources()[0].cherry_pick.is_empty());
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
    fn parse_source_without_sha256_defaults_to_skip() {
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
extract_dir = "foo"

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.sources()[0].sha256, "skip");
    }

    #[test]
    fn parse_git_source_with_cherry_pick() {
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
url = "https://example.com/foo.git#main"
sha256 = "skip"
extract_dir = "foo"
cherry_pick = ["deadbeef", "cafebabe"]

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.sources()[0].cherry_pick,
            vec!["deadbeef".to_string(), "cafebabe".to_string()]
        );
    }

    #[test]
    fn parse_package_dependencies_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "llvm"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/llvm.tar.gz"
sha256 = "skip"
extract_dir = "llvm"

[build]
type = "custom"

[dependencies]
runtime = ["base"]

[package_dependencies.clang]
runtime = ["llvm-libs", "llvm-libgcc"]

[package_dependencies.llvm-libs]
runtime = ["llvm-libgcc", "zstd"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.dependencies_for_output("llvm").runtime,
            vec!["base".to_string()]
        );
        assert_eq!(
            spec.dependencies_for_output("clang").runtime,
            vec!["llvm-libs".to_string(), "llvm-libgcc".to_string()]
        );
        assert_eq!(
            spec.dependencies_for_output("llvm-libs").runtime,
            vec!["llvm-libgcc".to_string(), "zstd".to_string()]
        );
    }

    #[test]
    fn parse_lib32_dependencies_override() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "llvm"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/llvm.tar.gz"
sha256 = "skip"
extract_dir = "llvm"

[build]
type = "custom"

[dependencies]
runtime = ["base"]

[dependencies.lib32]
build = ["gcc-multilib"]
runtime = ["lib32-zlib"]
test = ["bats"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.dependencies_for_output("llvm").runtime,
            vec!["base".to_string()]
        );
        assert_eq!(
            spec.dependencies_for_output("lib32-llvm").build,
            vec!["gcc-multilib".to_string()]
        );
        assert_eq!(
            spec.dependencies_for_output("lib32-llvm").runtime,
            vec!["lib32-zlib".to_string(), "llvm".to_string()]
        );
        assert_eq!(
            spec.dependencies_for_output("lib32-llvm").test,
            vec!["bats".to_string()]
        );
    }

    #[test]
    fn parse_package_alternatives_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "llvm"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/llvm.tar.gz"
sha256 = "skip"
extract_dir = "llvm"

[build]
type = "custom"

[alternatives]
provides = ["toolchain"]
conflicts = ["gcc"]

[package_alternatives.clang]
provides = ["cc", "c++", "gcc"]
conflicts = ["clang-legacy"]

[package_alternatives.llvm]
provides = ["binutils"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.alternatives_for_output("llvm").provides,
            vec!["binutils".to_string()]
        );
        assert_eq!(
            spec.alternatives_for_output("llvm").conflicts,
            Vec::<String>::new()
        );
        assert_eq!(
            spec.alternatives_for_output("clang").provides,
            vec!["cc".to_string(), "c++".to_string(), "gcc".to_string()]
        );
        assert_eq!(
            spec.alternatives_for_output("clang").conflicts,
            vec!["clang-legacy".to_string()]
        );
        assert_eq!(
            spec.alternatives_for_output("other").provides,
            vec!["toolchain".to_string()]
        );
        assert_eq!(
            spec.alternatives_for_output("other").conflicts,
            vec!["gcc".to_string()]
        );
    }

    #[test]
    fn parse_python_build_type() {
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
type = "python"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(matches!(spec.build.build_type, BuildType::Python));
    }

    #[test]
    fn parse_perl_build_type() {
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
type = "perl"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(matches!(spec.build.build_type, BuildType::Perl));
    }

    #[test]
    fn parse_python_config_settings_from_spec() {
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
type = "python"

[build.flags]
config-setting = ["editable_mode=compat", "setup-args=--plat-name=x86_64"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.config_settings,
            vec![
                "editable_mode=compat".to_string(),
                "setup-args=--plat-name=x86_64".to_string()
            ]
        );
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
    fn parse_allows_metapackage_without_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pkg.toml");

        std::fs::write(
            &path,
            r#"
[package]
name = "foo-meta"
version = "1.0"
description = "metapackage"
homepage = "https://example.com"
license = "MIT"

[build]
type = "meta"

[dependencies]
runtime = ["foo", "bar"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.source.is_empty());
        assert!(spec.manual_sources.is_empty());
        assert!(spec.is_metapackage());
        assert_eq!(spec.dependencies.runtime, vec!["foo", "bar"]);
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
        assert!(
            err.to_string()
                .contains("must specify one of 'file', 'files', 'url', or 'urls'")
        );
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
                .contains("cannot mix local ('file'/'files') and remote ('url'/'urls') entries")
        );
    }

    #[test]
    fn parse_manual_source_with_files_array() {
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
files = ["other", "system-auth"]

[build]
type = "custom"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.manual_sources.len(), 1);
        assert_eq!(spec.manual_sources[0].files, vec!["other", "system-auth"]);
        assert!(spec.manual_sources[0].urls.is_empty());
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
ld = "ld.lld"
CPP = "clang-cpp"
cflags = ["-O2"]
replace_cflags = ["-O2=>-O3"]
cxxflags = ["-O2", "-pipe"]
replace_cxxflags = ["-pipe=>-fPIC"]
passthrough_env = ["RUSTFLAGS"]
bindir = "/opt/bin"
sbindir = "/opt/sbin"
libdir = "/opt/lib64"
sysconfdir = "/opt/etc"
datarootdir = "/opt/share-root"
makeflags = "-j8"
make_vars = ["V=1"]
make_dirs = ["lib"]
make_test_dirs = ["tests"]
make_install_dirs = ["lib"]
ltoflags = ["-flto=auto"]
replace_ltoflags = ["auto=>thin"]
rustflags = ["-C", "debuginfo=2"]
replace_rustflags = ["debuginfo=2=>opt-level=2"]
use_lto = true
no_flags = true
no_strip = true
no_delete_static = true
no_compress_man = true
skip_tests = true
keep = ["etc/locale.gen"]
configure_file = "configure.gnu"
config-setting = ["editable_mode=compat"]
post_configure = ["echo configured"]
"#,
        )
        .unwrap();
        config.appends.insert(
            "build.flags.cflags".to_string(),
            vec![toml::Value::String("-g".to_string())],
        );
        config.appends.insert(
            "build.flags.replace_cflags".to_string(),
            vec![toml::Value::String(
                "-D_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".to_string(),
            )],
        );
        config.appends.insert(
            "build.flags.cxxflags".to_string(),
            vec![toml::Value::String("-stdlib=libc++".to_string())],
        );
        config.appends.insert(
            "build.flags.replace_cxxflags".to_string(),
            vec![toml::Value::String(
                "-stdlib=libc++=>-stdlib=libstdc++".to_string(),
            )],
        );
        config.appends.insert(
            "build.flags.rustflags".to_string(),
            vec![toml::Value::Array(vec![
                toml::Value::String("-C".to_string()),
                toml::Value::String("opt-level=3".to_string()),
            ])],
        );
        config.appends.insert(
            "build.flags.replace_rustflags".to_string(),
            vec![toml::Value::String("opt-level=3=>opt-level=z".to_string())],
        );
        config.appends.insert(
            "build.flags.keep".to_string(),
            vec![toml::Value::Array(vec![toml::Value::String(
                "etc/locale.gen".to_string(),
            )])],
        );
        config.appends.insert(
            "build.flags.ltoflags".to_string(),
            vec![toml::Value::Array(vec![toml::Value::String(
                "-fno-fat-lto-objects".to_string(),
            )])],
        );
        config.appends.insert(
            "build.flags.replace_ltoflags".to_string(),
            vec![toml::Value::String(
                "-fno-fat-lto-objects=>-flto-jobs=8".to_string(),
            )],
        );
        config.appends.insert(
            "build.flags.use_lto".to_string(),
            vec![toml::Value::Boolean(false)],
        );
        config.appends.insert(
            "build.flags.no_strip".to_string(),
            vec![toml::Value::Boolean(false)],
        );
        config.appends.insert(
            "build.flags.no_compress_man".to_string(),
            vec![toml::Value::Boolean(false)],
        );
        config.appends.insert(
            "build.flags.no_delete_static".to_string(),
            vec![toml::Value::Boolean(false)],
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
            "build.flags.makeflags".to_string(),
            vec![toml::Value::String("--output-sync=target".to_string())],
        );
        config.appends.insert(
            "build.flags.make_dirs".to_string(),
            vec![toml::Value::String("libelf".to_string())],
        );
        config.appends.insert(
            "build.flags.make_test_dirs".to_string(),
            vec![toml::Value::String("fuzz".to_string())],
        );
        config.appends.insert(
            "build.flags.make_install_dirs".to_string(),
            vec![toml::Value::String("tools".to_string())],
        );
        config.appends.insert(
            "build.flags.make_install_vars".to_string(),
            vec![toml::Value::String("DESTDIR=/tmp/pkg".to_string())],
        );
        config.appends.insert(
            "build.flags.configure_file".to_string(),
            vec![toml::Value::String("build-aux/configure".to_string())],
        );
        config.appends.insert(
            "build.flags.libexecdir".to_string(),
            vec![toml::Value::String("/opt/libexec".to_string())],
        );
        config.appends.insert(
            "build.flags.datadir".to_string(),
            vec![toml::Value::String("/opt/share-data".to_string())],
        );
        config.appends.insert(
            "build.flags.config-setting".to_string(),
            vec![toml::Value::String(
                "setup-args=--plat-name=x86_64".to_string(),
            )],
        );
        config.appends.insert(
            "build.flags.post_configure".to_string(),
            vec![toml::Value::String("touch configured.stamp".to_string())],
        );

        spec.apply_config(&config);

        assert_eq!(spec.build.flags.cc, "my-cc");
        assert_eq!(spec.build.flags.ld, "ld.lld");
        assert_eq!(spec.build.flags.cpp, "clang-cpp");
        assert!(spec.build.flags.cflags.contains(&"-O2".to_string()));
        assert!(spec.build.flags.cflags.contains(&"-g".to_string()));
        assert!(
            spec.build
                .flags
                .replace_cflags
                .contains(&"-O2=>-O3".to_string())
        );
        assert!(
            spec.build
                .flags
                .replace_cflags
                .contains(&"-D_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".to_string())
        );
        assert!(spec.build.flags.cxxflags.contains(&"-O2".to_string()));
        assert!(spec.build.flags.cxxflags.contains(&"-pipe".to_string()));
        assert!(
            spec.build
                .flags
                .cxxflags
                .contains(&"-stdlib=libc++".to_string())
        );
        assert!(
            spec.build
                .flags
                .replace_cxxflags
                .contains(&"-pipe=>-fPIC".to_string())
        );
        assert!(
            spec.build
                .flags
                .replace_cxxflags
                .contains(&"-stdlib=libc++=>-stdlib=libstdc++".to_string())
        );
        assert!(spec.build.flags.rustflags.contains(&"-C".to_string()));
        assert!(
            spec.build
                .flags
                .rustflags
                .contains(&"debuginfo=2".to_string())
        );
        assert!(
            spec.build
                .flags
                .rustflags
                .contains(&"opt-level=3".to_string())
        );
        assert!(
            spec.build
                .flags
                .replace_rustflags
                .contains(&"debuginfo=2=>opt-level=2".to_string())
        );
        assert!(
            spec.build
                .flags
                .replace_rustflags
                .contains(&"opt-level=3=>opt-level=z".to_string())
        );
        assert!(
            spec.build
                .flags
                .ltoflags
                .contains(&"-flto=auto".to_string())
        );
        assert!(
            spec.build
                .flags
                .ltoflags
                .contains(&"-fno-fat-lto-objects".to_string())
        );
        assert!(
            spec.build
                .flags
                .replace_ltoflags
                .contains(&"auto=>thin".to_string())
        );
        assert!(
            spec.build
                .flags
                .replace_ltoflags
                .contains(&"-fno-fat-lto-objects=>-flto-jobs=8".to_string())
        );
        assert!(!spec.build.flags.use_lto);
        assert!(spec.build.flags.no_flags);
        assert!(!spec.build.flags.no_strip);
        assert!(!spec.build.flags.no_delete_static);
        assert!(!spec.build.flags.no_compress_man);
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
        assert_eq!(spec.build.flags.bindir, "/opt/bin");
        assert_eq!(spec.build.flags.sbindir, "/opt/sbin");
        assert_eq!(spec.build.flags.libdir, "/opt/lib64");
        assert_eq!(spec.build.flags.libexecdir, "/opt/libexec");
        assert_eq!(spec.build.flags.sysconfdir, "/opt/etc");
        assert_eq!(spec.build.flags.datarootdir, "/opt/share-root");
        assert_eq!(spec.build.flags.datadir, "/opt/share-data");
        assert_eq!(spec.build.flags.makeflags, "-j8 --output-sync=target");
        assert!(spec.build.flags.make_vars.contains(&"V=1".to_string()));
        assert!(spec.build.flags.make_dirs.contains(&"lib".to_string()));
        assert!(spec.build.flags.make_dirs.contains(&"libelf".to_string()));
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
                .make_test_dirs
                .contains(&"tests".to_string())
        );
        assert!(
            spec.build
                .flags
                .make_test_dirs
                .contains(&"fuzz".to_string())
        );
        assert!(
            spec.build
                .flags
                .make_install_vars
                .contains(&"DESTDIR=/tmp/pkg".to_string())
        );
        assert!(
            spec.build
                .flags
                .make_install_dirs
                .contains(&"lib".to_string())
        );
        assert!(
            spec.build
                .flags
                .make_install_dirs
                .contains(&"tools".to_string())
        );
        assert_eq!(spec.build.flags.configure_file, "build-aux/configure");
        assert!(
            spec.build
                .flags
                .config_settings
                .contains(&"editable_mode=compat".to_string())
        );
        assert!(
            spec.build
                .flags
                .config_settings
                .contains(&"setup-args=--plat-name=x86_64".to_string())
        );
        assert!(
            spec.build
                .flags
                .post_configure
                .contains(&"echo configured".to_string())
        );
        assert!(
            spec.build
                .flags
                .post_configure
                .contains(&"touch configured.stamp".to_string())
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
    fn parse_ltoflags_and_use_lto_from_spec() {
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
ltoflags = ["-flto=auto", "-fuse-linker-plugin"]
use_lto = false
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.ltoflags,
            vec!["-flto=auto".to_string(), "-fuse-linker-plugin".to_string()]
        );
        assert!(!spec.build.flags.use_lto);
    }

    #[test]
    fn parse_ltoflags_and_use_lto_aliases_from_spec() {
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
LTOFLAGS = "-flto=auto"
"use-lto" = false
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.build.flags.ltoflags, vec!["-flto=auto".to_string()]);
        assert!(!spec.build.flags.use_lto);
    }

    #[test]
    fn parse_no_strip_no_delete_static_and_no_compress_man_from_spec() {
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
no_strip = true
"no-delete-static" = true
no-compress-man = true
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.build.flags.no_strip);
        assert!(spec.build.flags.no_delete_static);
        assert!(spec.build.flags.no_compress_man);
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
    fn reject_unknown_nested_keys_from_spec() {
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
runtime = ["glibc"]
skip_tests = true
"#,
        )
        .unwrap();

        let err = PackageSpec::from_file(&path).expect_err("expected unknown nested key to fail");
        assert!(
            err.to_string()
                .contains("unknown key: dependencies.skip_tests")
        );
    }

    #[test]
    fn parse_configure_file_from_spec() {
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
configure_file = "build-aux/configure"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.build.flags.configure_file, "build-aux/configure");
    }

    #[test]
    fn parse_install_dirs_from_spec() {
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
type = "cmake"

[build.flags]
bindir = "/custom/bin"
sbindir = "/custom/sbin"
libdir = "/custom/lib64"
libexecdir = "/custom/libexec"
sysconfdir = "/custom/etc"
localstatedir = "/custom/var"
sharedstatedir = "/custom/var/lib"
includedir = "/custom/include"
datarootdir = "/custom/share-root"
datadir = "/custom/share"
mandir = "/custom/share/man"
infodir = "/custom/share/info"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.build.flags.bindir, "/custom/bin");
        assert_eq!(spec.build.flags.sbindir, "/custom/sbin");
        assert_eq!(spec.build.flags.libdir, "/custom/lib64");
        assert_eq!(spec.build.flags.libexecdir, "/custom/libexec");
        assert_eq!(spec.build.flags.sysconfdir, "/custom/etc");
        assert_eq!(spec.build.flags.localstatedir, "/custom/var");
        assert_eq!(spec.build.flags.sharedstatedir, "/custom/var/lib");
        assert_eq!(spec.build.flags.includedir, "/custom/include");
        assert_eq!(spec.build.flags.datarootdir, "/custom/share-root");
        assert_eq!(spec.build.flags.datadir, "/custom/share");
        assert_eq!(spec.build.flags.mandir, "/custom/share/man");
        assert_eq!(spec.build.flags.infodir, "/custom/share/info");
    }

    #[test]
    fn parse_lib32_build_flags_from_spec() {
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
"build-32" = "true"
"CFLAGS-lib32" = ["-mstackrealign"]
"CXXFLAGS-lib32" = ["-fno-rtti"]
"configure-lib32" = ["--disable-static"]
"post_configure-lib32" = ["echo configured lib32"]
"post_compile-lib32" = ["echo compiled lib32"]
"post_install-lib32" = ["echo lib32"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.build.flags.build_32);
        assert_eq!(spec.build.flags.cflags_lib32, vec!["-mstackrealign"]);
        assert_eq!(spec.build.flags.cxxflags_lib32, vec!["-fno-rtti"]);
        assert_eq!(spec.build.flags.configure_lib32, vec!["--disable-static"]);
        assert_eq!(
            spec.build.flags.post_configure_lib32,
            vec!["echo configured lib32"]
        );
        assert_eq!(
            spec.build.flags.post_compile_lib32,
            vec!["echo compiled lib32"]
        );
        assert_eq!(spec.build.flags.post_install_lib32, vec!["echo lib32"]);
    }

    #[test]
    fn parse_post_configure_from_spec() {
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
type = "cmake"

[build.flags]
post_configure = ["cmake -L . > cmake-options.txt"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.post_configure,
            vec!["cmake -L . > cmake-options.txt".to_string()]
        );
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
    fn parse_split_docs_from_spec() {
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
split_docs = true
doc_dirs = ["/opt/docs", "usr/share/devhelp"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert!(spec.build.flags.split_docs);
        assert_eq!(
            spec.build.flags.doc_dirs,
            vec!["/opt/docs".to_string(), "usr/share/devhelp".to_string()]
        );
    }

    #[test]
    fn parse_build_flags_appends_from_spec_file() {
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
replace_cflags = ["-O2=>-O3"]
replace_rustflags = ["debuginfo=2=>opt-level=2"]
cxxflags = ["-O2"]
cxxflags += [ "-Wno-gnu-statement-expression-from-macro-expansion" ]
ldflags += "-Wl,--as-needed"
replace_cflags += [ "_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2" ]
replace_rustflags += "opt-level=3=>opt-level=z"
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.cxxflags,
            vec![
                "-O2".to_string(),
                "-Wno-gnu-statement-expression-from-macro-expansion".to_string()
            ]
        );
        assert_eq!(
            spec.build.flags.ldflags,
            vec!["-Wl,--as-needed".to_string()]
        );
        assert_eq!(
            spec.build.flags.replace_cflags,
            vec![
                "-O2=>-O3".to_string(),
                "_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".to_string()
            ]
        );
        assert_eq!(
            spec.build.flags.replace_rustflags,
            vec![
                "debuginfo=2=>opt-level=2".to_string(),
                "opt-level=3=>opt-level=z".to_string()
            ]
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
	optional = ["gtk-doc"]
	"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.dependencies.test,
            vec!["python".to_string(), "bats".to_string()]
        );
        assert_eq!(spec.dependencies.optional, vec!["gtk-doc".to_string()]);
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
make_exec = "ninja"
make_target = "bootstrap"
make_targets = ["stage1", "stage2"]
make_dirs = ["lib", "libelf"]
make_test_vars = ["TESTS=unit"]
make_test_target = "test"
make_test_targets = ["test-unit", "test-integration"]
make_test_dirs = ["tests"]
make_install_vars = ["STRIPPROG=true"]
make_install_target = "install/strip"
make_install_targets = ["install-runtime", "install-devel"]
make_install_dirs = ["lib", "apps"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(
            spec.build.flags.make_vars,
            vec!["V=1".to_string(), "CC=clang".to_string()]
        );
        assert_eq!(spec.build.flags.make_exec, "ninja");
        assert_eq!(spec.build.flags.make_target, "bootstrap");
        assert_eq!(
            spec.build.flags.make_targets,
            vec!["stage1".to_string(), "stage2".to_string()]
        );
        assert_eq!(
            spec.build.flags.make_dirs,
            vec!["lib".to_string(), "libelf".to_string()]
        );
        assert_eq!(
            spec.build.flags.make_test_vars,
            vec!["TESTS=unit".to_string()]
        );
        assert_eq!(spec.build.flags.make_test_target, "test".to_string());
        assert_eq!(
            spec.build.flags.make_test_targets,
            vec!["test-unit".to_string(), "test-integration".to_string()]
        );
        assert_eq!(spec.build.flags.make_test_dirs, vec!["tests".to_string()]);
        assert_eq!(
            spec.build.flags.make_install_vars,
            vec!["STRIPPROG=true".to_string()]
        );
        assert_eq!(
            spec.build.flags.make_install_target,
            "install/strip".to_string()
        );
        assert_eq!(
            spec.build.flags.make_install_targets,
            vec!["install-runtime".to_string(), "install-devel".to_string()]
        );
        assert_eq!(
            spec.build.flags.make_install_dirs,
            vec!["lib".to_string(), "apps".to_string()]
        );
    }

    #[test]
    fn parse_makeflags_from_spec() {
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
MAKEFLAGS = ["-j12", "--output-sync=target"]
"#,
        )
        .unwrap();

        let spec = PackageSpec::from_file(&path).unwrap();
        assert_eq!(spec.build.flags.makeflags, "-j12 --output-sync=target");
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
            repo_settings: crate::config::RepoSettings::default(),
            source_repos: std::collections::BTreeMap::new(),
            binary_repos: std::collections::BTreeMap::new(),
            mirrors: std::collections::HashMap::new(),
            repo_clone_dir: PathBuf::from("/tmp"),
            package_cache_dir: PathBuf::from("/tmp"),
            install_test_deps: false,
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

    #[test]
    fn docs_output_uses_runtime_dependency_on_parent_package() {
        let mut spec = mk_spec("foo", "1.0");
        spec.build.flags.split_docs = true;
        let docs_name = PackageSpec::docs_package_name("foo");

        let deps = spec.dependencies_for_output(&docs_name);
        assert_eq!(deps.runtime, vec!["foo".to_string()]);

        let alternatives = spec.alternatives_for_output(&docs_name);
        assert!(alternatives.provides.is_empty());
        assert!(alternatives.conflicts.is_empty());
    }

    #[test]
    fn docs_package_for_output_derives_name_and_description() {
        let mut spec = mk_spec("foo", "1.0");
        spec.build.flags.split_docs = true;

        let docs = spec.docs_package_for_output(&spec.package);
        assert_eq!(docs.name, "foo-docs");
        assert_eq!(docs.description, "Documentation for foo");
        assert_eq!(docs.version, "1.0");
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
        if !self.alternatives.conflicts.is_empty() {
            writeln!(f, "Conflicts: {}", self.alternatives.conflicts.join(", "))?;
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

/// Package alternatives such as virtual provides and install conflicts.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct Alternatives {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// Reserved for future package replacement feature
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[allow(dead_code)]
    pub replaces: Vec<String>,
}

/// Source tarball information
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Source {
    pub url: String,
    /// Checksum for the source (e.g. `sha256:...`, `sha512:...`, `md5:...`, `b2:...`, `b2sum:...`, or raw SHA256 hex).
    /// Defaults to `skip` when omitted.
    #[serde(default = "default_source_sha256")]
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

    /// Optional list of git commit hashes/revs to cherry-pick after checkout.
    ///
    /// This is only valid for git sources (`*.git` URL or `url#rev` git form).
    /// Example:
    /// cherry_pick = ["a1b2c3d4", "deadbeef"]
    #[serde(
        default,
        alias = "cherry-pick",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub cherry_pick: Vec<String>,
}

/// Manual source copied before standard source fetching.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ManualSource {
    /// Filename in the spec directory (local manual source mode).
    #[serde(default)]
    pub file: Option<String>,
    /// Multiple filenames in the spec directory (local manual source mode).
    #[serde(default)]
    pub files: Vec<String>,
    /// Remote URL to fetch (remote manual source mode).
    #[serde(default)]
    pub url: Option<String>,
    /// Multiple remote URLs to fetch (remote manual source mode).
    #[serde(default)]
    pub urls: Vec<String>,
    /// Checksum (optional, use "skip" to bypass verification).
    #[serde(default)]
    pub sha256: Option<String>,
    /// Destination path relative to build work directory.
    /// Defaults to `file` for local mode or a derived filename for URL mode.
    #[serde(default)]
    pub dest: Option<String>,
}

fn default_source_sha256() -> String {
    "skip".to_string()
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
    Perl,
    Custom,
    Python,
    Rust,
    Makefile,
    Bin,
    Meta,
}

/// Build flags and toolchain configuration
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BuildFlags {
    /// Extra flags exported to `CFLAGS`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub cflags: Vec<String>,
    /// Ordered replacement rules applied to `cflags` before export.
    ///
    /// Each entry may use `old=>new`. Plain `old=new` is also accepted and
    /// disambiguated against the current flag set when possible.
    #[serde(
        default,
        alias = "replace-cflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cflags: Vec<String>,
    /// Extra flags exported to `CFLAGS` only for the lib32 build variant.
    #[serde(
        default,
        alias = "cflags-lib32",
        alias = "cflags_lib32",
        alias = "CFLAGS-lib32",
        alias = "CFLAGS_lib32",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub cflags_lib32: Vec<String>,
    /// Ordered replacement rules applied to lib32-only `cflags`.
    #[serde(
        default,
        alias = "replace-cflags-lib32",
        alias = "replace_cflags-lib32",
        alias = "replace_cflags_lib32",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cflags_lib32: Vec<String>,
    /// Extra flags exported to `CXXFLAGS`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub cxxflags: Vec<String>,
    /// Ordered replacement rules applied to `cxxflags` before export.
    #[serde(
        default,
        alias = "replace-cxxflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cxxflags: Vec<String>,
    /// Extra flags exported to `CXXFLAGS` only for the lib32 build variant.
    #[serde(
        default,
        alias = "cxxflags-lib32",
        alias = "cxxflags_lib32",
        alias = "CXXFLAGS-lib32",
        alias = "CXXFLAGS_lib32",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub cxxflags_lib32: Vec<String>,
    /// Ordered replacement rules applied to lib32-only `cxxflags`.
    #[serde(
        default,
        alias = "replace-cxxflags-lib32",
        alias = "replace_cxxflags-lib32",
        alias = "replace_cxxflags_lib32",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_cxxflags_lib32: Vec<String>,
    /// Extra flags exported to `LDFLAGS`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub ldflags: Vec<String>,
    /// Ordered replacement rules applied to `ldflags` before export.
    #[serde(
        default,
        alias = "replace-ldflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_ldflags: Vec<String>,
    /// Link-time optimization flags exported to `LTOFLAGS`.
    ///
    /// When `use_lto` is true (default), these flags are also appended to
    /// `CFLAGS`, `CXXFLAGS`, and `LDFLAGS`.
    #[serde(
        default,
        alias = "lto-flags",
        alias = "lto_flags",
        alias = "LTOFLAGS",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub ltoflags: Vec<String>,
    /// Ordered replacement rules applied to `ltoflags` before export/injection.
    #[serde(
        default,
        alias = "replace-ltoflags",
        alias = "replace_lto-flags",
        alias = "replace_lto_flags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_ltoflags: Vec<String>,
    /// Keep existing files and install package-provided replacement as `<path>.depotnew`.
    #[serde(default, deserialize_with = "deserialize_string_or_array")]
    pub keep: Vec<String>,
    /// Split documentation trees into a derived `<package>-docs` output during staging.
    #[serde(
        default,
        alias = "split-docs",
        deserialize_with = "deserialize_boolish"
    )]
    pub split_docs: bool,
    /// Additional documentation directories to move into `<package>-docs`.
    #[serde(
        default,
        alias = "doc-dirs",
        alias = "doc_dirs",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub doc_dirs: Vec<String>,
    /// Disable automatic LTOFLAGS injection into CFLAGS/CXXFLAGS/LDFLAGS.
    #[serde(
        default = "default_use_lto",
        alias = "use-lto",
        deserialize_with = "deserialize_boolish"
    )]
    pub use_lto: bool,
    /// Disable exporting CFLAGS/CXXFLAGS/LDFLAGS for this package build.
    #[serde(default, alias = "no-flags")]
    pub no_flags: bool,
    /// Disable automatic stripping of ELF files during staging.
    #[serde(default, alias = "no-strip")]
    pub no_strip: bool,
    /// Disable automatic deletion of static libraries (`*.a`) during staging.
    #[serde(
        default,
        alias = "no-delete-static",
        alias = "no_remove_static",
        alias = "no-remove-static"
    )]
    pub no_delete_static: bool,
    /// Disable automatic zstd compression of man pages during staging.
    #[serde(
        default,
        alias = "no-compress-man",
        alias = "no_compress_manpages",
        alias = "no-compress-manpages"
    )]
    pub no_compress_man: bool,
    /// Skip automatic build-system test execution (e.g. Autotools `make check`/`make test`).
    #[serde(default, alias = "skip-tests")]
    pub skip_tests: bool,
    /// Run an additional lib32 build pass and emit a `lib32-*` package.
    #[serde(
        default,
        alias = "build-32",
        alias = "build_32",
        deserialize_with = "deserialize_boolish"
    )]
    pub build_32: bool,
    #[serde(default)]
    pub configure: Vec<String>,
    /// PEP 517 config settings for Python builds (each entry is `KEY=VALUE` or `KEY`).
    #[serde(
        default,
        alias = "config-setting",
        alias = "config-settings",
        alias = "config_setting",
        alias = "config_settings",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub config_settings: Vec<String>,
    /// Configure arguments used only for the lib32 build variant (replaces `configure` when set).
    #[serde(default, alias = "configure-lib32", alias = "configure_lib32")]
    pub configure_lib32: Vec<String>,
    /// Autotools configure script path, relative to source root or absolute.
    #[serde(default, alias = "configure-file")]
    pub configure_file: String,
    /// C compiler
    #[serde(default = "default_cc")]
    pub cc: String,
    /// C++ compiler
    #[serde(default = "default_cxx")]
    pub cxx: String,
    /// Archiver
    #[serde(default = "default_ar")]
    pub ar: String,
    /// Linker executable or linker flavor override for supported builders.
    #[serde(default)]
    pub ld: String,
    /// C preprocessor executable exported as `CPP` when configured.
    #[serde(default, alias = "CPP")]
    pub cpp: String,
    /// Dynamic loader path
    #[serde(default)]
    pub libc: String,
    /// Root filesystem for installation (per-package override)
    #[serde(default = "default_rootfs")]
    #[allow(dead_code)]
    pub rootfs: String,
    /// Commands to run after configure/setup step, before compile/build step.
    #[serde(default, alias = "post-configure")]
    pub post_configure: Vec<String>,
    /// Commands to run after configure/setup for the lib32 build variant.
    #[serde(
        default,
        alias = "post-configure-lib32",
        alias = "post_configure-lib32",
        alias = "post_configure_lib32"
    )]
    pub post_configure_lib32: Vec<String>,
    /// Commands to run after compile (after make, before make install).
    #[serde(default, alias = "post-compile")]
    pub post_compile: Vec<String>,
    /// Commands to run after compile for the lib32 build variant.
    #[serde(
        default,
        alias = "post-compile-lib32",
        alias = "post_compile-lib32",
        alias = "post_compile_lib32"
    )]
    pub post_compile_lib32: Vec<String>,
    /// Commands to run after install (after make install)
    #[serde(default, alias = "post-install")]
    pub post_install: Vec<String>,
    /// Commands to run after the lib32 install step (replaces `post_install` when set).
    #[serde(
        default,
        alias = "post-install-lib32",
        alias = "post_install-lib32",
        alias = "post_install_lib32"
    )]
    pub post_install_lib32: Vec<String>,

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
    /// MAKEFLAGS environment variable passed to build commands.
    #[serde(
        default,
        alias = "make-flags",
        alias = "make_flags",
        alias = "MAKEFLAGS",
        deserialize_with = "deserialize_string_or_array_joined"
    )]
    pub makeflags: String,
    /// Variable overrides passed directly to `make` (compile step), e.g. ["V=1", "CC=clang"].
    #[serde(
        default,
        alias = "make-vars",
        alias = "make_build_vars",
        alias = "make-build-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_vars: Vec<String>,
    /// Make-like executable for build/test/install phases (default: `make`), e.g. `ninja`.
    #[serde(default, alias = "make-exec")]
    pub make_exec: String,
    /// Target for the compile/build phase (e.g. `all`, `bootstrap`).
    #[serde(
        default,
        alias = "make-target",
        alias = "make_build_target",
        alias = "make-build-target"
    )]
    pub make_target: String,
    /// Targets for the compile/build phase (e.g. `["all", "bootstrap"]`).
    #[serde(
        default,
        alias = "make-targets",
        alias = "make_build_targets",
        alias = "make-build-targets",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_targets: Vec<String>,
    /// Subdirectories (relative to build directory) where `make` should run.
    #[serde(
        default,
        alias = "make-dirs",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_dirs: Vec<String>,
    /// Variable overrides passed directly to `make check` / `make test`.
    #[serde(
        default,
        alias = "make-test-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_test_vars: Vec<String>,
    /// Target for the test phase, passed to the make-like executable.
    #[serde(default, alias = "make-test-target")]
    pub make_test_target: String,
    /// Targets for the test phase, passed to the make-like executable.
    #[serde(
        default,
        alias = "make-test-targets",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_test_targets: Vec<String>,
    /// Subdirectories (relative to build directory) where test targets should run.
    #[serde(
        default,
        alias = "make-test-dirs",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_test_dirs: Vec<String>,
    /// Variable overrides passed directly to `make install`.
    #[serde(
        default,
        alias = "make-install-vars",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_install_vars: Vec<String>,
    /// Target for the install phase (default: `install`).
    #[serde(default, alias = "make-install-target")]
    pub make_install_target: String,
    /// Targets for the install phase.
    #[serde(
        default,
        alias = "make-install-targets",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_install_targets: Vec<String>,
    /// Subdirectories (relative to build directory) where `make install` should run.
    #[serde(
        default,
        alias = "make-install-dirs",
        deserialize_with = "deserialize_string_or_array"
    )]
    pub make_install_dirs: Vec<String>,
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
    /// Ordered replacement rules applied to `rustflags` before export.
    #[serde(
        default,
        alias = "replace-rustflags",
        deserialize_with = "deserialize_string_or_array_no_split"
    )]
    pub replace_rustflags: Vec<String>,
    /// Additional cargo arguments (short name)
    #[serde(default)]
    pub cargs: Vec<String>,
    /// Binary installation directory relative to DESTDIR (default: /usr/bin)
    #[serde(default = "default_bindir")]
    pub bindir: String,
    /// System binary installation directory for supported builders (default: /usr/bin).
    #[serde(default)]
    pub sbindir: String,
    /// Library installation directory for supported builders.
    ///
    /// Defaults to `/usr/lib`, or `/usr/lib32` for the lib32 build variant.
    #[serde(default)]
    pub libdir: String,
    /// Library helper executable installation directory for supported builders.
    ///
    /// Defaults to the effective `libdir`.
    #[serde(default)]
    pub libexecdir: String,
    /// System configuration directory for supported builders (default: /etc).
    #[serde(default)]
    pub sysconfdir: String,
    /// Variable state directory for supported builders (default: /var).
    #[serde(default)]
    pub localstatedir: String,
    /// Shared variable state directory for supported builders (default: /var/lib).
    #[serde(default)]
    pub sharedstatedir: String,
    /// Header installation directory for supported builders (default: /usr/include).
    #[serde(default)]
    pub includedir: String,
    /// Data root installation directory for supported builders (default: /usr/share).
    #[serde(default)]
    pub datarootdir: String,
    /// Architecture-independent data installation directory for supported builders.
    ///
    /// Defaults to the effective `datarootdir`.
    #[serde(default)]
    pub datadir: String,
    /// Manual page installation directory for supported builders (default: /usr/share/man).
    #[serde(default)]
    pub mandir: String,
    /// Info page installation directory for supported builders (default: /usr/share/info).
    #[serde(default)]
    pub infodir: String,

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
    /// Internal runtime marker used to adjust builder behavior for the lib32 variant.
    #[serde(skip)]
    pub lib32_variant: bool,
}

impl Default for BuildFlags {
    fn default() -> Self {
        BuildFlags {
            cflags: Vec::new(),
            replace_cflags: Vec::new(),
            cflags_lib32: Vec::new(),
            replace_cflags_lib32: Vec::new(),
            cxxflags: Vec::new(),
            replace_cxxflags: Vec::new(),
            cxxflags_lib32: Vec::new(),
            replace_cxxflags_lib32: Vec::new(),
            ldflags: Vec::new(),
            replace_ldflags: Vec::new(),
            ltoflags: Vec::new(),
            replace_ltoflags: Vec::new(),
            keep: Vec::new(),
            split_docs: false,
            doc_dirs: Vec::new(),
            use_lto: default_use_lto(),
            no_flags: false,
            no_strip: false,
            no_delete_static: false,
            no_compress_man: false,
            skip_tests: false,
            build_32: false,
            configure: Vec::new(),
            config_settings: Vec::new(),
            configure_lib32: Vec::new(),
            configure_file: String::new(),
            cc: default_cc(),
            cxx: default_cxx(),
            ar: default_ar(),
            ld: String::new(),
            cpp: String::new(),
            libc: String::new(),
            rootfs: default_rootfs(),
            post_configure: Vec::new(),
            post_configure_lib32: Vec::new(),
            post_compile: Vec::new(),
            post_compile_lib32: Vec::new(),
            post_install: Vec::new(),
            post_install_lib32: Vec::new(),
            makefile_commands: Vec::new(),
            makefile_install_commands: Vec::new(),
            prefix: default_prefix(),
            chost: String::new(),
            cbuild: String::new(),
            carch: default_carch(),
            makeflags: String::new(),
            make_vars: Vec::new(),
            make_exec: String::new(),
            make_target: String::new(),
            make_targets: Vec::new(),
            make_dirs: Vec::new(),
            make_test_vars: Vec::new(),
            make_test_target: String::new(),
            make_test_targets: Vec::new(),
            make_test_dirs: Vec::new(),
            make_install_vars: Vec::new(),
            make_install_target: String::new(),
            make_install_targets: Vec::new(),
            make_install_dirs: Vec::new(),
            passthrough_env: Vec::new(),
            profile: default_profile(),
            target: String::new(),
            rustflags: Vec::new(),
            replace_rustflags: Vec::new(),
            cargs: Vec::new(),
            bindir: default_bindir(),
            sbindir: String::new(),
            libdir: String::new(),
            libexecdir: String::new(),
            sysconfdir: String::new(),
            localstatedir: String::new(),
            sharedstatedir: String::new(),
            includedir: String::new(),
            datarootdir: String::new(),
            datadir: String::new(),
            mandir: String::new(),
            infodir: String::new(),
            source_subdir: String::new(),
            build_dir: None,
            binary_type: String::new(),
            lib32_variant: false,
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

fn deserialize_string_or_array_no_split<'de, D>(
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
        Some(StringOrArray::String(s)) => Ok(vec![s]),
        Some(StringOrArray::Array(a)) => Ok(a),
        None => Ok(Vec::new()),
    }
}

fn deserialize_string_or_array_joined<'de, D>(
    deserializer: D,
) -> std::result::Result<String, D::Error>
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
        Some(StringOrArray::String(s)) => Ok(s),
        Some(StringOrArray::Array(a)) => Ok(a
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")),
        None => Ok(String::new()),
    }
}

fn deserialize_boolish<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Boolish {
        Bool(bool),
        String(String),
    }

    match Option::<Boolish>::deserialize(deserializer)? {
        Some(Boolish::Bool(v)) => Ok(v),
        Some(Boolish::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "expected boolean string for lib32 flag, got '{}'",
                other
            ))),
        },
        None => Ok(false),
    }
}

fn toml_value_as_boolish(value: &toml::Value) -> Option<bool> {
    if let Some(b) = value.as_bool() {
        return Some(b);
    }
    value
        .as_str()
        .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        })
}

fn append_whitespace_separated(dst: &mut String, value: &str) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    if dst.is_empty() {
        dst.push_str(trimmed);
    } else {
        dst.push(' ');
        dst.push_str(trimmed);
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

fn default_use_lto() -> bool {
    true
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

/// Nested dependency override group for a specific output variant.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct DependencyGroup {
    /// Dependencies required for building packages.
    #[serde(default)]
    pub build: Vec<String>,
    /// Dependencies required at runtime.
    #[serde(default)]
    pub runtime: Vec<String>,
    /// Dependencies required to run package test suites.
    #[serde(default)]
    pub test: Vec<String>,
    /// Optional runtime integrations that enhance functionality when installed.
    #[serde(default)]
    pub optional: Vec<String>,
}

impl DependencyGroup {
    fn to_dependencies(&self) -> Dependencies {
        Dependencies {
            build: self.build.clone(),
            runtime: self.runtime.clone(),
            test: self.test.clone(),
            optional: self.optional.clone(),
            lib32: None,
        }
    }
}

/// Package dependencies
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
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
    /// Optional runtime integrations that enhance functionality when installed.
    #[serde(default)]
    pub optional: Vec<String>,
    /// Optional dependency overrides used only for the generated `lib32-*` companion package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lib32: Option<DependencyGroup>,
}

impl Dependencies {
    /// Return the top-level dependency set without any nested output-specific overrides.
    pub fn primary_dependencies(&self) -> Dependencies {
        Dependencies {
            build: self.build.clone(),
            runtime: self.runtime.clone(),
            test: self.test.clone(),
            optional: self.optional.clone(),
            lib32: None,
        }
    }

    /// Return the optional lib32-specific dependency override set.
    pub fn lib32_dependencies(&self) -> Option<Dependencies> {
        self.lib32.as_ref().map(DependencyGroup::to_dependencies)
    }
}
