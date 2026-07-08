use super::config::{normalize_append_key, preprocess_spec_toml_appends};
use super::model::*;
use anyhow::{Context, Result};
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
        spec.validate_dkms()?;

        Ok(spec)
    }

    fn apply_spec_appends(
        &mut self,
        appends: &std::collections::HashMap<String, Vec<toml::Value>>,
    ) -> Result<()> {
        for (key, values) in appends {
            let key = normalize_append_key(key);
            if let Some(subkey) = key.strip_prefix("build.flags.") {
                self.apply_append(subkey, values);
                continue;
            }
            if let Some(subkey) = key.strip_prefix("flags.") {
                self.apply_append(subkey, values);
                continue;
            }
            if !key.contains('.') {
                self.apply_append(&key, values);
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

    fn validate_dkms(&self) -> Result<()> {
        if !matches!(self.build.build_type, BuildType::Dkms) {
            return Ok(());
        }

        if self.build.flags.dkms_modules.is_empty() {
            anyhow::bail!(
                "build.type = \"dkms\" requires at least one build.flags.dkms_modules entry"
            );
        }

        validate_dkms_component("build.flags.dkms_name", &self.effective_dkms_name())?;
        validate_dkms_component("build.flags.dkms_version", &self.effective_dkms_version())?;
        validate_dkms_rel_path(
            "build.flags.dkms_source_dir",
            self.build.flags.dkms_source_dir.trim(),
            true,
        )?;
        validate_dkms_install_dir(
            "build.flags.dkms_install_dir",
            self.effective_dkms_install_dir().as_str(),
        )?;

        let mut installed_names = HashSet::new();
        for (idx, module) in self.build.flags.dkms_modules.iter().enumerate() {
            let label = format!("build.flags.dkms_modules[{idx}]");
            validate_dkms_component(&format!("{label}.name"), module.name.trim())?;
            let dest_name = self.effective_dkms_module_dest_name(module);
            validate_dkms_component(&format!("{label}.dest_name"), &dest_name)?;
            validate_dkms_rel_path(&format!("{label}.build_dir"), module.build_dir.trim(), true)?;
            validate_dkms_rel_path(
                &format!("{label}.built_location"),
                module.built_location.trim(),
                true,
            )?;
            let install_dir = self.effective_dkms_module_install_dir(module);
            validate_dkms_install_dir(&format!("{label}.install_dir"), &install_dir)?;

            if !installed_names.insert(dest_name.clone()) {
                anyhow::bail!("Duplicate DKMS destination module name: {}", dest_name);
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

    /// Return the effective Depot DKMS source package name.
    pub fn effective_dkms_name(&self) -> String {
        let configured = self.build.flags.dkms_name.trim();
        if configured.is_empty() {
            self.package.name.clone()
        } else {
            self.expand_vars(configured)
        }
    }

    /// Return the effective Depot DKMS source package version.
    pub fn effective_dkms_version(&self) -> String {
        let configured = self.build.flags.dkms_version.trim();
        if configured.is_empty() {
            self.package.version.clone()
        } else {
            self.expand_vars(configured)
        }
    }

    /// Return the default module install directory for Depot DKMS packages.
    pub fn effective_dkms_install_dir(&self) -> String {
        let configured = self.build.flags.dkms_install_dir.trim();
        if configured.is_empty() {
            "updates/depot".to_string()
        } else {
            normalize_dkms_install_dir(&self.expand_vars(configured))
        }
    }

    /// Return the effective installed module name for a DKMS module entry.
    pub fn effective_dkms_module_dest_name(&self, module: &DkmsModule) -> String {
        let configured = module.dest_name.trim();
        if configured.is_empty() {
            module.name.clone()
        } else {
            self.expand_vars(configured)
        }
    }

    /// Return the effective install directory for a DKMS module entry.
    pub fn effective_dkms_module_install_dir(&self, module: &DkmsModule) -> String {
        let configured = module.install_dir.trim();
        if configured.is_empty() {
            self.effective_dkms_install_dir()
        } else {
            normalize_dkms_install_dir(&self.expand_vars(configured))
        }
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

    /// Return true when this spec should emit the generated `lib32-*` package.
    pub fn builds_lib32_output(&self) -> bool {
        self.build.flags.build_32 || self.build.flags.lib32_only
    }

    /// Return true when only the generated `lib32-*` package should be emitted.
    pub fn builds_only_lib32_output(&self) -> bool {
        self.build.flags.lib32_only
    }

    /// Return true when builder-managed automatic tests should be skipped.
    ///
    /// Automatic test phases are disabled when `build.flags.skip_tests` is set and for
    /// multilib builds, because the generated lib32 output is built in a separate 32-bit pass.
    pub fn should_skip_automatic_tests(&self) -> bool {
        self.build.flags.skip_tests || self.builds_lib32_output()
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
        if pkg_name == self.lib32_package_name() {
            return self
                .package_alternatives
                .get(pkg_name)
                .cloned()
                .or_else(|| self.alternatives.lib32_alternatives())
                .unwrap_or_default();
        }

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
}

fn normalize_dkms_install_dir(raw: &str) -> String {
    raw.trim().trim_start_matches('/').to_string()
}

fn validate_dkms_component(label: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("{label} cannot be empty");
    }
    if value.contains('/') || value.contains('\\') {
        anyhow::bail!("{label} cannot contain path separators: {value}");
    }
    if value == "." || value == ".." {
        anyhow::bail!("{label} cannot be '.' or '..'");
    }
    Ok(())
}

fn validate_dkms_install_dir(label: &str, value: &str) -> Result<()> {
    validate_dkms_rel_path(label, value.trim_start_matches('/'), false)
}

fn validate_dkms_rel_path(label: &str, value: &str, allow_empty: bool) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        if allow_empty {
            return Ok(());
        }
        anyhow::bail!("{label} cannot be empty");
    }
    let path = Path::new(value);
    if path.is_absolute() {
        anyhow::bail!("{label} must be relative: {value}");
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => anyhow::bail!("{label} contains an unsafe path component: {value}"),
        }
    }
    Ok(())
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
        if !self.alternatives.replaces.is_empty() {
            writeln!(f, "Replaces: {}", self.alternatives.replaces.join(", "))?;
        }
        if !self.dependencies.groups.is_empty() {
            writeln!(f, "Groups: {}", self.dependencies.groups.join(", "))?;
        }
        Ok(())
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
