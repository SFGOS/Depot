use super::*;

fn package_spec_from_archive_metadata(metadata: &toml::Value) -> package::PackageSpec {
    let mut spec = package::PackageSpec {
        package: package::PackageInfo {
            name: metadata
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            real_name: metadata
                .get("real_name")
                .and_then(|v| v.as_str())
                .map(String::from),
            version: metadata
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            revision: metadata
                .get("revision")
                .and_then(|v| v.as_integer())
                .unwrap_or(1) as u32,
            description: metadata
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            homepage: metadata
                .get("homepage")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            abi_breaking: metadata
                .get("abi_breaking")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            built_against: super::super::parse_metadata_string_list(metadata, "built_against"),
            license: super::super::parse_licenses_from_toml(metadata),
        },
        packages: Vec::new(),
        alternatives: package::Alternatives::default(),
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: package::Build {
            build_type: package::BuildType::Bin,
            flags: package::BuildFlags {
                keep: super::super::parse_keep_list(metadata),
                ..package::BuildFlags::default()
            },
        },
        dependencies: package::Dependencies {
            build: Vec::new(),
            runtime: super::super::parse_dependency_list(metadata, "runtime"),
            test: Vec::new(),
            optional: super::super::parse_dependency_list(metadata, "optional"),
            groups: super::super::parse_dependency_list(metadata, "groups"),
            lib32: None,
        },
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    if let Some(provides) = metadata.get("provides").and_then(|v| v.as_array()) {
        spec.alternatives.provides = provides
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }
    if let Some(conflicts) = metadata.get("conflicts").and_then(|v| v.as_array()) {
        spec.alternatives.conflicts = conflicts
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }
    if let Some(replaces) = metadata.get("replaces").and_then(|v| v.as_array()) {
        spec.alternatives.replaces = replaces
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
    }

    spec
}

fn load_package_spec_from_staging(staged_dir: &Path) -> Result<package::PackageSpec> {
    let metadata_path = staged_dir.join(".metadata.toml");
    let metadata_content = fs::read_to_string(&metadata_path)
        .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
    let metadata: toml::Value = toml::from_str(&metadata_content)
        .with_context(|| format!("Failed to parse {}", metadata_path.display()))?;
    Ok(package_spec_from_archive_metadata(&metadata))
}

fn parse_license_list_from_repo(license: &Option<String>) -> Vec<String> {
    let Some(raw) = license.as_ref() else {
        return Vec::new();
    };
    raw.split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(String::from)
        .collect()
}

pub(crate) fn package_spec_from_repo_record(
    record: &db::repo::BinaryRepoPackageRecord,
) -> package::PackageSpec {
    package::PackageSpec {
        package: package::PackageInfo {
            name: record.name.clone(),
            real_name: record.real_name.clone(),
            version: record.version.clone(),
            revision: record.revision,
            description: record.description.clone().unwrap_or_default(),
            homepage: record.homepage.clone().unwrap_or_default(),
            abi_breaking: record.abi_breaking,
            built_against: record.built_against.clone(),
            license: parse_license_list_from_repo(&record.license),
        },
        packages: Vec::new(),
        alternatives: package::Alternatives {
            provides: record.provides.clone(),
            conflicts: record.conflicts.clone(),
            replaces: record.replaces.clone(),
            lib32: None,
        },
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: package::Build {
            build_type: package::BuildType::Bin,
            flags: package::BuildFlags::default(),
        },
        dependencies: package::Dependencies {
            build: Vec::new(),
            runtime: record.runtime_dependencies.clone(),
            test: Vec::new(),
            optional: record.optional_dependencies.clone(),
            groups: record.groups.clone(),
            lib32: None,
        },
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    }
}

pub(crate) fn load_package_spec_from_staging_or_repo_record(
    staged_dir: &Path,
    record: &db::repo::BinaryRepoPackageRecord,
) -> Result<package::PackageSpec> {
    let metadata_path = staged_dir.join(".metadata.toml");
    if metadata_path.exists() {
        load_package_spec_from_staging(staged_dir)
    } else {
        Ok(package_spec_from_repo_record(record))
    }
}

pub(crate) fn staging_temp_root(config: &config::Config) -> PathBuf {
    config.build_dir.join("staging")
}

fn create_archive_staging_dir(
    config: &config::Config,
    archive_path: &Path,
) -> Result<tempfile::TempDir> {
    let staging_root = staging_temp_root(config);
    fs::create_dir_all(&staging_root)
        .with_context(|| format!("Failed to create staging root {}", staging_root.display()))?;
    tempfile::Builder::new()
        .prefix("archive-")
        .tempdir_in(&staging_root)
        .with_context(|| {
            format!(
                "Failed to create staging dir for {} under {}",
                archive_path.display(),
                staging_root.display()
            )
        })
}

pub(crate) fn load_package_archive_into_staging(
    config: &config::Config,
    archive_path: &Path,
) -> Result<(package::PackageSpec, tempfile::TempDir)> {
    let tmp_dir = create_archive_staging_dir(config, archive_path).with_context(|| {
        format!(
            "Failed to create staging dir for {}",
            archive_path.display()
        )
    })?;
    let extract_dir = tmp_dir.path().to_path_buf();

    let file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive {}", archive_path.display()))?;
    let zstd_decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("Failed to read zstd stream {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(zstd_decoder);

    let mut metadata_content = String::new();
    for entry in archive.entries().with_context(|| {
        format!(
            "Failed to read archive entries from {}",
            archive_path.display()
        )
    })? {
        crate::interrupts::check()?;
        let mut entry = entry.with_context(|| {
            format!(
                "Failed to read archive entry from {}",
                archive_path.display()
            )
        })?;
        if entry.path()?.to_string_lossy() == ".metadata.toml" {
            use std::io::Read;
            entry
                .read_to_string(&mut metadata_content)
                .with_context(|| {
                    format!(
                        "Failed to read .metadata.toml in {}",
                        archive_path.display()
                    )
                })?;
            break;
        }
    }

    if metadata_content.is_empty() {
        anyhow::bail!(
            "Package archive does not contain .metadata.toml: {}",
            archive_path.display()
        );
    }

    let file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive {}", archive_path.display()))?;
    let zstd_decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("Failed to read zstd stream {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(zstd_decoder);
    archive.set_preserve_permissions(true);
    crate::interrupts::unpack_tar_archive(&mut archive, &extract_dir).with_context(|| {
        format!(
            "Failed to extract package archive {} into {}",
            archive_path.display(),
            extract_dir.display()
        )
    })?;

    let metadata: toml::Value = toml::from_str(&metadata_content).with_context(|| {
        format!(
            "Failed to parse .metadata.toml in {}",
            archive_path.display()
        )
    })?;

    Ok((package_spec_from_archive_metadata(&metadata), tmp_dir))
}

pub(crate) fn extract_package_archive_to_staging(
    config: &config::Config,
    archive_path: &Path,
) -> Result<tempfile::TempDir> {
    let tmp_dir = create_archive_staging_dir(config, archive_path).with_context(|| {
        format!(
            "Failed to create staging dir for {}",
            archive_path.display()
        )
    })?;
    let extract_dir = tmp_dir.path().to_path_buf();

    let file = fs::File::open(archive_path)
        .with_context(|| format!("Failed to open archive {}", archive_path.display()))?;
    let zstd_decoder = zstd::stream::read::Decoder::new(file)
        .with_context(|| format!("Failed to read zstd stream {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(zstd_decoder);
    archive.set_preserve_permissions(true);
    crate::interrupts::unpack_tar_archive(&mut archive, &extract_dir).with_context(|| {
        format!(
            "Failed to extract package archive {} into {}",
            archive_path.display(),
            extract_dir.display()
        )
    })?;
    Ok(tmp_dir)
}
