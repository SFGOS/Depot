//! Depot-managed kernel module source packaging.

use crate::cross::CrossConfig;
use crate::install::scripts;
use crate::package::{DkmsModule, PackageSpec};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

const DEPOT_KMOD_MANIFEST: &str = ".depot-kmod.toml";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct DepotKmodManifest {
    pub name: String,
    pub version: String,
    pub install_dir: String,
    pub make_args: Vec<String>,
    pub pre_build: Vec<String>,
    pub modules: Vec<DepotKmodModule>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct DepotKmodModule {
    pub name: String,
    pub dest_name: String,
    pub build_dir: String,
    pub built_location: String,
    pub install_dir: String,
}

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    _cross: Option<&CrossConfig>,
    _export_compiler_flags: bool,
    _host_build_dir: Option<&Path>,
) -> Result<()> {
    if spec.build.flags.lib32_variant {
        anyhow::bail!("build.type = \"dkms\" does not support lib32 variants");
    }

    fs::create_dir_all(destdir)
        .with_context(|| format!("Failed to create DESTDIR: {}", destdir.display()))?;

    let source_root = resolve_dkms_source_root(spec, src_dir)?;
    let source_dest = staged_source_dir(spec, destdir);
    if source_dest.exists() {
        fs::remove_dir_all(&source_dest).with_context(|| {
            format!(
                "Failed to remove existing DKMS source staging dir: {}",
                source_dest.display()
            )
        })?;
    }
    crate::fs_copy::copy_tree_preserving_links(&source_root, &source_dest).with_context(|| {
        format!(
            "Failed to stage DKMS source tree from {} to {}",
            source_root.display(),
            source_dest.display()
        )
    })?;

    let manifest = manifest_from_spec(spec)?;
    let manifest_toml =
        toml::to_string_pretty(&manifest).context("Failed to serialize DKMS metadata")?;
    fs::write(source_dest.join(DEPOT_KMOD_MANIFEST), manifest_toml).with_context(|| {
        format!(
            "Failed to write DKMS metadata: {}",
            source_dest.join(DEPOT_KMOD_MANIFEST).display()
        )
    })?;

    Ok(())
}

pub(crate) fn stage_lifecycle_scripts(spec: &PackageSpec, destdir: &Path) -> Result<()> {
    if !spec.build.flags.dkms_autoinstall {
        return Ok(());
    }

    let scripts_dir = scripts::staged_scripts_dir(destdir);
    fs::create_dir_all(&scripts_dir)
        .with_context(|| format!("Failed to create scripts dir: {}", scripts_dir.display()))?;

    let source_rel = format!("/usr/src/{}", source_dir_name(spec));
    let package_name = spec.effective_dkms_name();
    stage_script_command(
        &scripts_dir.join("post_install"),
        &autoinstall_command(&source_rel),
    )?;
    stage_script_command(
        &scripts_dir.join("post_update"),
        &autoinstall_command(&source_rel),
    )?;
    stage_script_command(
        &scripts_dir.join("pre_remove"),
        &remove_command(&package_name),
    )?;
    stage_script_command(
        &scripts_dir.join("pre_update"),
        &remove_command(&package_name),
    )?;
    Ok(())
}

pub(crate) fn manifest_from_spec(spec: &PackageSpec) -> Result<DepotKmodManifest> {
    let default_install_dir = spec.effective_dkms_install_dir();
    let modules = spec
        .build
        .flags
        .dkms_modules
        .iter()
        .map(|module| manifest_module_from_spec(spec, module, &default_install_dir))
        .collect::<Result<Vec<_>>>()?;

    let mut build_dirs = BTreeSet::new();
    for module in &modules {
        build_dirs.insert(module.build_dir.clone());
    }
    if build_dirs.is_empty() {
        anyhow::bail!("DKMS manifest must contain at least one module");
    }

    Ok(DepotKmodManifest {
        name: spec.effective_dkms_name(),
        version: spec.effective_dkms_version(),
        install_dir: default_install_dir,
        make_args: spec
            .build
            .flags
            .dkms_make_args
            .iter()
            .map(|arg| spec.expand_vars(arg))
            .collect(),
        pre_build: spec
            .build
            .flags
            .dkms_pre_build
            .iter()
            .map(|arg| spec.expand_vars(arg))
            .collect(),
        modules,
    })
}

fn manifest_module_from_spec(
    spec: &PackageSpec,
    module: &DkmsModule,
    default_install_dir: &str,
) -> Result<DepotKmodModule> {
    let name = spec.expand_vars(module.name.trim());
    let dest_name = spec.effective_dkms_module_dest_name(module);
    let build_dir = normalized_rel_string(&spec.expand_vars(module.build_dir.trim()), true)?;
    let built_location = if module.built_location.trim().is_empty() {
        build_dir.clone()
    } else {
        normalized_rel_string(&spec.expand_vars(module.built_location.trim()), true)?
    };
    let install_dir = if module.install_dir.trim().is_empty() {
        default_install_dir.to_string()
    } else {
        spec.effective_dkms_module_install_dir(module)
    };

    Ok(DepotKmodModule {
        name,
        dest_name,
        build_dir,
        built_location,
        install_dir,
    })
}

fn resolve_dkms_source_root(spec: &PackageSpec, src_dir: &Path) -> Result<PathBuf> {
    let base = if spec.build.flags.source_subdir.trim().is_empty() {
        src_dir.to_path_buf()
    } else {
        let subdir =
            normalized_rel_string(&spec.expand_vars(&spec.build.flags.source_subdir), false)?;
        src_dir.join(subdir)
    };
    let source_dir = spec.expand_vars(spec.build.flags.dkms_source_dir.trim());
    let source_root = if source_dir.trim().is_empty() {
        base
    } else {
        base.join(normalized_rel_string(&source_dir, false)?)
    };
    if !source_root.is_dir() {
        anyhow::bail!("DKMS source directory not found: {}", source_root.display());
    }
    Ok(source_root)
}

fn staged_source_dir(spec: &PackageSpec, destdir: &Path) -> PathBuf {
    destdir.join("usr/src").join(source_dir_name(spec))
}

fn source_dir_name(spec: &PackageSpec) -> String {
    format!(
        "{}-{}",
        spec.effective_dkms_name(),
        spec.effective_dkms_version()
    )
}

fn autoinstall_command(source_rel: &str) -> String {
    format!(
        "depot internal dkms-autoinstall --rootfs \"$DEPOT_ROOTFS\" --source {}",
        sh_quote(source_rel)
    )
}

fn remove_command(name: &str) -> String {
    format!(
        "depot internal dkms-remove --rootfs \"$DEPOT_ROOTFS\" --name {}",
        sh_quote(name)
    )
}

fn stage_script_command(path: &Path, command: &str) -> Result<()> {
    if path.exists() {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open lifecycle script: {}", path.display()))?;
        writeln!(file).with_context(|| format!("Failed to update {}", path.display()))?;
        writeln!(file, "# Depot DKMS autoinstall").with_context(|| {
            format!(
                "Failed to append lifecycle script marker: {}",
                path.display()
            )
        })?;
        writeln!(file, "{command}")
            .with_context(|| format!("Failed to append lifecycle script: {}", path.display()))?;
    } else {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{command}\n"))
            .with_context(|| format!("Failed to write lifecycle script: {}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms)
            .with_context(|| format!("Failed to make script executable: {}", path.display()))?;
    }
    Ok(())
}

fn normalized_rel_string(raw: &str, allow_empty: bool) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        if allow_empty {
            return Ok(".".to_string());
        }
        anyhow::bail!("DKMS path cannot be empty");
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        anyhow::bail!("DKMS path must be relative: {}", trimmed);
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => anyhow::bail!("DKMS path contains unsafe component: {}", trimmed),
        }
    }
    if out.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(out.to_string_lossy().into_owned())
    }
}

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, Source,
    };
    use tempfile::tempdir;

    fn mk_spec() -> PackageSpec {
        let flags = BuildFlags {
            dkms_modules: vec![
                DkmsModule {
                    name: "zfs".into(),
                    build_dir: "module".into(),
                    built_location: "module/zfs".into(),
                    ..DkmsModule::default()
                },
                DkmsModule {
                    name: "spl".into(),
                    dest_name: "spl_compat".into(),
                    build_dir: "module".into(),
                    built_location: "module/spl".into(),
                    install_dir: "updates/storage".into(),
                },
            ],
            dkms_make_args: vec!["V=1".into()],
            dkms_pre_build: vec!["./configure --with-linux=$kernel_build_dir".into()],
            ..BuildFlags::default()
        };
        PackageSpec {
            package: PackageInfo {
                name: "zfs-dkms".into(),
                real_name: None,
                version: "2.4.3".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                built_against: Vec::new(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.test/zfs.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "zfs".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Dkms,
                flags,
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn dkms_build_stages_source_metadata_and_scripts() -> Result<()> {
        let tmp = tempdir()?;
        let src = tmp.path().join("src");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(src.join("module/zfs"))?;
        fs::write(src.join("module/zfs/zfs.c"), "source")?;

        let spec = mk_spec();
        build(&spec, &src, &dest, None, true, None)?;
        fs::create_dir_all(dest.join("scripts"))?;
        fs::write(
            dest.join("scripts/post_install"),
            "#!/bin/sh\necho package\n",
        )?;
        stage_lifecycle_scripts(&spec, &dest)?;

        let staged = dest.join("usr/src/zfs-dkms-2.4.3");
        assert!(staged.join("module/zfs/zfs.c").exists());
        let manifest: DepotKmodManifest =
            toml::from_str(&fs::read_to_string(staged.join(DEPOT_KMOD_MANIFEST))?)?;
        assert_eq!(manifest.name, "zfs-dkms");
        assert_eq!(manifest.pre_build.len(), 1);
        assert_eq!(manifest.modules.len(), 2);
        assert_eq!(manifest.modules[1].dest_name, "spl_compat");
        let post_install = fs::read_to_string(dest.join("scripts/post_install"))?;
        assert!(post_install.contains("echo package"));
        assert!(post_install.contains("dkms-autoinstall"));
        assert!(dest.join("scripts/pre_update").exists());
        Ok(())
    }
}
