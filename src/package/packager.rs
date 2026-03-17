//! Package creation and archive management

use crate::config::Config;
use crate::metadata_time;
use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tar::Builder;
use zstd::stream::write::Encoder;

fn is_internal_staging_rel_path(rel_path: &Path) -> bool {
    let s = rel_path.to_string_lossy();
    let p = s.trim_start_matches('/');
    p == crate::staging::INTERNAL_DEPOT_DIR
        || p.strip_prefix(crate::staging::INTERNAL_DEPOT_DIR)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn is_skipped_package_payload_rel_path(rel_path: &Path) -> bool {
    crate::staging::is_purged_payload_path(&rel_path.to_string_lossy())
}

fn license_value(licenses: &[String]) -> toml::Value {
    if licenses.len() == 1 {
        toml::Value::String(licenses[0].clone())
    } else {
        toml::Value::Array(
            licenses
                .iter()
                .map(|license| toml::Value::String(license.clone()))
                .collect(),
        )
    }
}

pub struct Packager {
    pub spec: PackageSpec,
    pub destdir: PathBuf,
    pub config: Config,
}

impl Packager {
    pub fn new(spec: PackageSpec, destdir: PathBuf, config: Config) -> Self {
        Self {
            spec,
            destdir,
            config,
        }
    }

    /// Create a package archive (.depot.pkg.tar.zst) from the destdir
    pub fn create_package(&self, output_dir: &Path, arch: &str) -> Result<PathBuf> {
        let filename = self.spec.package_filename(arch);
        let output_path = output_dir.join(&filename);

        crate::log_info!("Creating package {}...", filename);

        // Generate .files.yaml
        self.generate_files_yaml()?;

        // Generate .metadata.toml
        self.generate_metadata_toml()?;

        // Create tar.zst
        let file = fs::File::create(&output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;

        // Respect zstd level from config (default to 19 if not specified)
        let level = self
            .config
            .package_overrides
            .get("compression_level")
            .and_then(|v| v.as_integer())
            .unwrap_or(19) as i32;

        let mut encoder = Encoder::new(file, level)?;
        let _ = encoder.multithread(num_cpus() as u32);

        let mut tar = Builder::new(encoder);

        // Manual walk to ensure symlinks aren't followed (preserving them as links)
        for entry in walkdir::WalkDir::new(&self.destdir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            let rel_path = path.strip_prefix(&self.destdir)?;

            // Skip the root of the destdir
            if rel_path.as_os_str().is_empty() {
                continue;
            }
            if is_internal_staging_rel_path(rel_path) {
                continue;
            }
            if is_skipped_package_payload_rel_path(rel_path) {
                continue;
            }

            let file_type = entry.file_type();
            if file_type.is_dir() {
                tar.append_dir(rel_path, path)?;
            } else if file_type.is_symlink() {
                // For symlinks, we need to read the link and append to tar correctly
                let target = fs::read_link(path)?;
                let mut header = tar::Header::new_gnu();
                header.set_metadata_in_mode(
                    &fs::symlink_metadata(path)?,
                    tar::HeaderMode::Deterministic,
                );
                tar.append_link(&mut header, rel_path, target)?;
            } else {
                // Files
                let mut file = fs::File::open(path)?;
                tar.append_file(rel_path, &mut file)?;
            }
        }

        let encoder = tar.into_inner()?;
        encoder.finish()?;

        crate::log_info!("Created package: {}", output_path.display());
        Ok(output_path)
    }

    fn generate_metadata_toml(&self) -> Result<()> {
        let metadata_path = self.destdir.join(".metadata.toml");
        let completed_at = metadata_time::current_utc_timestamp_string()?;

        // Construct a simple metadata structure
        let mut map = toml::map::Map::new();
        map.insert(
            "name".to_string(),
            toml::Value::String(self.spec.package.name.clone()),
        );
        map.insert(
            "version".to_string(),
            toml::Value::String(self.spec.package.version.clone()),
        );
        if let Some(real_name) = &self.spec.package.real_name {
            map.insert(
                "real_name".to_string(),
                toml::Value::String(real_name.clone()),
            );
        }
        map.insert(
            "revision".to_string(),
            toml::Value::Integer(self.spec.package.revision as i64),
        );
        map.insert(
            "description".to_string(),
            toml::Value::String(self.spec.package.description.clone()),
        );
        map.insert(
            "homepage".to_string(),
            toml::Value::String(self.spec.package.homepage.clone()),
        );
        map.insert(
            "abi_breaking".to_string(),
            toml::Value::Boolean(self.spec.package.abi_breaking),
        );
        map.insert(
            "license".to_string(),
            license_value(&self.spec.package.license),
        );
        map.insert(
            "completed_at".to_string(),
            toml::Value::String(completed_at),
        );

        // Add provides
        map.insert(
            "provides".to_string(),
            toml::Value::Array(
                self.spec
                    .alternatives
                    .provides
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        map.insert(
            "conflicts".to_string(),
            toml::Value::Array(
                self.spec
                    .alternatives
                    .conflicts
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        map.insert(
            "replaces".to_string(),
            toml::Value::Array(
                self.spec
                    .alternatives
                    .replaces
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );

        // Add install-relevant dependency kinds for repo/runtime consumers.
        let mut deps = toml::map::Map::new();
        deps.insert(
            "runtime".to_string(),
            toml::Value::Array(
                self.spec
                    .dependencies
                    .runtime
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        deps.insert(
            "optional".to_string(),
            toml::Value::Array(
                self.spec
                    .dependencies
                    .optional
                    .iter()
                    .map(|s| toml::Value::String(s.clone()))
                    .collect(),
            ),
        );
        map.insert("dependencies".to_string(), toml::Value::Table(deps));
        if !self.spec.build.flags.keep.is_empty() {
            map.insert(
                "keep".to_string(),
                toml::Value::Array(
                    self.spec
                        .build
                        .flags
                        .keep
                        .iter()
                        .map(|s| toml::Value::String(s.clone()))
                        .collect(),
                ),
            );
        }

        let toml_str = toml::to_string(&toml::Value::Table(map))
            .context("Failed to serialize metadata to TOML")?;

        fs::write(&metadata_path, toml_str)
            .with_context(|| format!("Failed to write metadata: {}", metadata_path.display()))?;

        Ok(())
    }

    fn generate_files_yaml(&self) -> Result<()> {
        let files_path = self.destdir.join(".files.yaml");
        let mut files = Vec::new();

        // Recursively list all files in destdir
        self.collect_files(&self.destdir, &self.destdir, &mut files)?;

        let mut out_str = String::new();
        {
            let mut emitter = yaml_rust2::YamlEmitter::new(&mut out_str);
            let yaml_vec: Vec<yaml_rust2::Yaml> =
                files.into_iter().map(yaml_rust2::Yaml::String).collect();
            emitter
                .dump(&yaml_rust2::Yaml::Array(yaml_vec))
                .context("Failed to emit YAML")?;
        }

        fs::write(&files_path, out_str)?;

        Ok(())
    }

    fn collect_files(&self, base: &Path, current: &Path, files: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let relative = path.strip_prefix(base)?.to_string_lossy().to_string();
            if is_internal_staging_rel_path(Path::new(&relative)) {
                continue;
            }
            if is_skipped_package_payload_rel_path(Path::new(&relative)) {
                continue;
            }

            if file_type.is_dir() {
                self.collect_files(base, &path, files)?;
            } else {
                files.push(relative);
            }
        }
        Ok(())
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo};
    use std::io::Read;

    fn mk_packager(destdir: PathBuf) -> Packager {
        Packager::new(
            PackageSpec {
                package: PackageInfo {
                    name: "test".into(),
                    real_name: None,
                    version: "1.0".into(),
                    revision: 1,
                    description: "d".into(),
                    homepage: "h".into(),
                    abi_breaking: false,
                    license: vec!["MIT".into()],
                },
                packages: Vec::new(),
                alternatives: Alternatives::default(),
                manual_sources: Vec::new(),
                source: Vec::new(),
                build: Build {
                    build_type: BuildType::Custom,
                    flags: BuildFlags::default(),
                },
                dependencies: Dependencies::default(),
                package_alternatives: Default::default(),
                package_dependencies: Default::default(),
                spec_dir: PathBuf::from("."),
            },
            destdir,
            Config::for_rootfs(Path::new("/tmp/nonexistent")),
        )
    }

    #[test]
    fn test_collect_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        fs::create_dir_all(dest.join("usr/bin")).unwrap();
        fs::write(dest.join("usr/bin/foo"), "x").unwrap();
        fs::create_dir_all(dest.join("etc")).unwrap();
        fs::write(dest.join("etc/config"), "y").unwrap();

        let packager = mk_packager(dest.to_path_buf());
        let mut files = Vec::new();
        packager.collect_files(dest, dest, &mut files).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains(&"usr/bin/foo".to_string()));
        assert!(files.contains(&"etc/config".to_string()));
    }

    #[test]
    fn test_collect_files_skips_internal_output_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        fs::create_dir_all(dest.join("usr/bin")).unwrap();
        fs::create_dir_all(dest.join(".depot/outputs/clang/usr/bin")).unwrap();
        fs::write(dest.join("usr/bin/foo"), "x").unwrap();
        fs::write(dest.join(".depot/outputs/clang/usr/bin/clang"), "x").unwrap();

        let packager = mk_packager(dest.to_path_buf());
        let mut files = Vec::new();
        packager.collect_files(dest, dest, &mut files).unwrap();

        assert!(files.contains(&"usr/bin/foo".to_string()));
        assert!(!files.iter().any(|f| f.starts_with(".depot/outputs/")));
    }

    #[test]
    fn test_collect_files_skips_purged_payload_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        fs::create_dir_all(dest.join("usr/share/info")).unwrap();
        fs::create_dir_all(dest.join("usr/lib/perl5/5.42/core_perl")).unwrap();
        fs::create_dir_all(dest.join("usr/lib/perl5/5.42/vendor_perl/auto/Error")).unwrap();
        fs::create_dir_all(dest.join("usr/share/doc/perl-error")).unwrap();
        fs::create_dir_all(dest.join("usr/bin")).unwrap();
        fs::write(dest.join("usr/share/info/dir"), "index").unwrap();
        fs::write(
            dest.join("usr/lib/perl5/5.42/core_perl/perllocal.pod"),
            "pod",
        )
        .unwrap();
        fs::write(
            dest.join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist"),
            "packlist",
        )
        .unwrap();
        fs::write(dest.join("usr/share/doc/perl-error/Error.pod"), "pod").unwrap();
        fs::write(dest.join("usr/bin/foo"), "x").unwrap();

        let packager = mk_packager(dest.to_path_buf());
        let mut files = Vec::new();
        packager.collect_files(dest, dest, &mut files).unwrap();

        assert!(!files.contains(&"usr/share/info/dir".to_string()));
        assert!(!files.contains(&"usr/lib/perl5/5.42/core_perl/perllocal.pod".to_string()));
        assert!(
            !files.contains(&"usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist".to_string())
        );
        assert!(!files.contains(&"usr/share/doc/perl-error/Error.pod".to_string()));
        assert!(files.contains(&"usr/bin/foo".to_string()));
    }

    #[test]
    fn test_generate_files_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();
        fs::create_dir_all(dest.join("usr/bin")).unwrap();
        fs::write(dest.join("usr/bin/foo"), "x").unwrap();

        let packager = mk_packager(dest.to_path_buf());
        packager.generate_files_yaml().unwrap();

        let yaml_path = dest.join(".files.yaml");
        assert!(yaml_path.exists());
        let yaml_content = fs::read_to_string(yaml_path).unwrap();
        assert!(yaml_content.contains("usr/bin/foo"));
    }

    #[test]
    fn test_generate_metadata_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();

        let packager = mk_packager(dest.to_path_buf());
        packager.generate_metadata_toml().unwrap();

        let meta_path = dest.join(".metadata.toml");
        assert!(meta_path.exists());
        let content = fs::read_to_string(meta_path).unwrap();
        let val: toml::Value = toml::from_str(&content).unwrap();

        assert_eq!(val.get("name").and_then(|v| v.as_str()), Some("test"));
        assert_eq!(val.get("version").and_then(|v| v.as_str()), Some("1.0"));
        assert_eq!(val.get("revision").and_then(|v| v.as_integer()), Some(1));
        assert_eq!(val.get("license").and_then(|v| v.as_str()), Some("MIT"));
        assert!(
            val.get("replaces")
                .and_then(|v| v.as_array())
                .expect("replaces should be an array")
                .is_empty()
        );
        assert!(
            crate::metadata_time::parse_completed_at_value(&val).is_some(),
            "expected RFC3339 UTC completed_at"
        );

        let deps = val.get("dependencies").unwrap();
        assert!(deps.get("runtime").unwrap().as_array().unwrap().is_empty());
        assert!(deps.get("optional").unwrap().as_array().unwrap().is_empty());
        assert!(deps.get("build").is_none());
        assert!(deps.get("test").is_none());
    }

    #[test]
    fn test_generate_metadata_toml_with_multiple_licenses() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();

        let mut packager = mk_packager(dest.to_path_buf());
        packager.spec.package.license = vec!["MIT".into(), "Apache-2.0".into()];
        packager.generate_metadata_toml().unwrap();

        let meta_path = dest.join(".metadata.toml");
        let content = fs::read_to_string(meta_path).unwrap();
        let val: toml::Value = toml::from_str(&content).unwrap();
        let arr = val
            .get("license")
            .and_then(|v| v.as_array())
            .expect("license should be an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0].as_str(), Some("MIT"));
        assert_eq!(arr[1].as_str(), Some("Apache-2.0"));
    }

    #[test]
    fn test_generate_metadata_toml_includes_real_name_and_abi_breaking() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();

        let mut packager = mk_packager(dest.to_path_buf());
        packager.spec.package.real_name = Some("icu".into());
        packager.spec.package.abi_breaking = true;
        packager.generate_metadata_toml().unwrap();

        let meta_path = dest.join(".metadata.toml");
        let content = fs::read_to_string(meta_path).unwrap();
        let val: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(val.get("real_name").and_then(|v| v.as_str()), Some("icu"));
        assert_eq!(
            val.get("abi_breaking").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn test_generate_metadata_toml_includes_keep_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();

        let mut packager = mk_packager(dest.to_path_buf());
        packager.spec.build.flags.keep = vec!["etc/fstab".into(), "etc/passwd".into()];
        packager.generate_metadata_toml().unwrap();

        let meta_path = dest.join(".metadata.toml");
        let content = fs::read_to_string(meta_path).unwrap();
        let val: toml::Value = toml::from_str(&content).unwrap();
        let keep = val
            .get("keep")
            .and_then(|v| v.as_array())
            .expect("keep should be an array");
        assert_eq!(keep.len(), 2);
        assert_eq!(keep[0].as_str(), Some("etc/fstab"));
        assert_eq!(keep[1].as_str(), Some("etc/passwd"));
    }

    #[test]
    fn test_generate_metadata_toml_includes_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path();

        let mut packager = mk_packager(dest.to_path_buf());
        packager.spec.alternatives.replaces = vec!["findutils".into(), "diffutils".into()];
        packager.generate_metadata_toml().unwrap();

        let meta_path = dest.join(".metadata.toml");
        let content = fs::read_to_string(meta_path).unwrap();
        let val: toml::Value = toml::from_str(&content).unwrap();
        let replaces = val
            .get("replaces")
            .and_then(|v| v.as_array())
            .expect("replaces should be an array");
        assert_eq!(replaces.len(), 2);
        assert_eq!(replaces[0].as_str(), Some("findutils"));
        assert_eq!(replaces[1].as_str(), Some("diffutils"));
    }

    #[test]
    fn test_create_package_skips_purged_payload_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        let out = tmp.path().join("out");
        fs::create_dir_all(dest.join("usr/share/info")).unwrap();
        fs::create_dir_all(dest.join("usr/lib/perl5/5.42/core_perl")).unwrap();
        fs::create_dir_all(dest.join("usr/lib/perl5/5.42/vendor_perl/auto/Error")).unwrap();
        fs::create_dir_all(dest.join("usr/share/doc/perl-error")).unwrap();
        fs::create_dir_all(dest.join("usr/bin")).unwrap();
        fs::write(dest.join("usr/share/info/dir"), "index").unwrap();
        fs::write(
            dest.join("usr/lib/perl5/5.42/core_perl/perllocal.pod"),
            "pod",
        )
        .unwrap();
        fs::write(
            dest.join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist"),
            "packlist",
        )
        .unwrap();
        fs::write(dest.join("usr/share/doc/perl-error/Error.pod"), "pod").unwrap();
        fs::write(dest.join("usr/bin/foo"), "x").unwrap();
        fs::create_dir_all(&out).unwrap();

        let packager = mk_packager(dest.clone());
        let archive_path = packager.create_package(&out, "x86_64").unwrap();

        let archive_file = fs::File::open(&archive_path).unwrap();
        let decoder = zstd::Decoder::new(archive_file).unwrap();
        let mut archive = tar::Archive::new(decoder);
        let mut paths = Vec::new();

        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let mut sink = Vec::new();
            let _ = entry.read_to_end(&mut sink);
            paths.push(path);
        }

        assert!(!paths.contains(&"usr/share/info/dir".to_string()));
        assert!(!paths.contains(&"usr/lib/perl5/5.42/core_perl/perllocal.pod".to_string()));
        assert!(
            !paths.contains(&"usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist".to_string())
        );
        assert!(!paths.contains(&"usr/share/doc/perl-error/Error.pod".to_string()));
        assert!(paths.contains(&"usr/bin/foo".to_string()));
        assert!(paths.contains(&".metadata.toml".to_string()));
        assert!(paths.contains(&".files.yaml".to_string()));
    }
}
