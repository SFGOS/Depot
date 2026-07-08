//! Runtime support for Depot-managed kernel module source packages.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const DEPOT_KMOD_MANIFEST: &str = ".depot-kmod.toml";
const TRACKING_DIR_REL: &str = "var/lib/depot/kmods";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct DepotKmodManifest {
    name: String,
    version: String,
    #[serde(default)]
    install_dir: String,
    #[serde(default)]
    make_args: Vec<String>,
    #[serde(default)]
    pre_build: Vec<String>,
    modules: Vec<DepotKmodModule>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct DepotKmodModule {
    name: String,
    dest_name: String,
    build_dir: String,
    built_location: String,
    install_dir: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct InstalledKmodManifest {
    name: String,
    version: String,
    entries: Vec<InstalledKmodEntry>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct InstalledKmodEntry {
    kernel: String,
    module: String,
    path: String,
}

#[derive(Debug, Clone)]
struct KernelTarget {
    version: String,
    build_dir: PathBuf,
}

/// Build and install a Depot-managed kernel module source tree for all kernels.
pub(crate) fn autoinstall(rootfs: &Path, source: &Path) -> Result<()> {
    let source_dir = resolve_rootfs_path(rootfs, source);
    let manifest = read_source_manifest(&source_dir)?;
    if manifest.modules.is_empty() {
        anyhow::bail!(
            "DKMS source manifest contains no modules: {}",
            source_dir.display()
        );
    }

    let kernels = discover_kernels(rootfs)?;
    if kernels.is_empty() {
        crate::log_warn!(
            "Skipping Depot DKMS autoinstall for {}/{}: no kernels under {}",
            manifest.name,
            manifest.version,
            rootfs.join("lib/modules").display()
        );
        return Ok(());
    }

    let buildable: Vec<_> = kernels
        .into_iter()
        .filter_map(|kernel| {
            if kernel.build_dir.exists() {
                Some(kernel)
            } else {
                crate::log_warn!(
                    "Skipping kernel {} for {}/{}: missing build directory {}",
                    kernel.version,
                    manifest.name,
                    manifest.version,
                    kernel.build_dir.display()
                );
                None
            }
        })
        .collect();
    if buildable.is_empty() {
        anyhow::bail!(
            "No buildable kernels found for {}/{} under {}",
            manifest.name,
            manifest.version,
            rootfs.join("lib/modules").display()
        );
    }

    let mut installed = Vec::new();
    for kernel in &buildable {
        build_for_kernel(rootfs, &source_dir, &manifest, kernel)?;
        installed.extend(install_for_kernel(rootfs, &source_dir, &manifest, kernel)?);
        run_depmod(rootfs, &kernel.version)?;
    }

    write_tracking_manifest(rootfs, &manifest, installed)?;
    Ok(())
}

/// Remove Depot-installed kernel modules by source tree or package name.
pub(crate) fn remove(rootfs: &Path, source: Option<&Path>, name: Option<&str>) -> Result<()> {
    let selected = selected_tracking_manifests(rootfs, source, name)?;
    if selected.is_empty() {
        if let Some(name) = name {
            crate::log_warn!("No installed Depot DKMS modules found for {}", name);
        }
        return Ok(());
    }

    let mut depmod_kernels = BTreeSet::new();
    for (manifest_path, manifest) in selected {
        for entry in &manifest.entries {
            let module_path = rootfs.join(&entry.path);
            if module_path.exists() {
                fs::remove_file(&module_path).with_context(|| {
                    format!(
                        "Failed to remove installed module {}",
                        module_path.display()
                    )
                })?;
            }
            depmod_kernels.insert(entry.kernel.clone());
        }
        fs::remove_file(&manifest_path).with_context(|| {
            format!(
                "Failed to remove Depot DKMS tracking manifest {}",
                manifest_path.display()
            )
        })?;
    }

    for kernel in depmod_kernels {
        run_depmod(rootfs, &kernel)?;
    }
    Ok(())
}

fn read_source_manifest(source_dir: &Path) -> Result<DepotKmodManifest> {
    let manifest_path = source_dir.join(DEPOT_KMOD_MANIFEST);
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    toml::from_str(&raw).with_context(|| format!("Failed to parse {}", manifest_path.display()))
}

fn discover_kernels(rootfs: &Path) -> Result<Vec<KernelTarget>> {
    let modules_root = rootfs.join("lib/modules");
    if !modules_root.exists() {
        return Ok(Vec::new());
    }
    if !modules_root.is_dir() {
        anyhow::bail!(
            "Kernel modules path is not a directory: {}",
            modules_root.display()
        );
    }

    let mut kernels = Vec::new();
    for entry in fs::read_dir(&modules_root)
        .with_context(|| format!("Failed to read {}", modules_root.display()))?
    {
        let entry = entry.with_context(|| format!("Failed to list {}", modules_root.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("Failed to inspect {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let version = entry.file_name().to_string_lossy().into_owned();
        if version.trim().is_empty() {
            continue;
        }
        kernels.push(KernelTarget {
            version,
            build_dir: entry.path().join("build"),
        });
    }
    kernels.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(kernels)
}

fn build_for_kernel(
    rootfs: &Path,
    source_dir: &Path,
    manifest: &DepotKmodManifest,
    kernel: &KernelTarget,
) -> Result<()> {
    run_pre_build_commands(rootfs, source_dir, manifest, kernel)?;
    let build_dirs = module_build_dirs(source_dir, manifest)?;
    for build_dir in build_dirs {
        crate::log_info!(
            "Building Depot DKMS module {}/{} for kernel {}",
            manifest.name,
            manifest.version,
            kernel.version
        );
        let mut cmd = Command::new("make");
        cmd.arg("-C")
            .arg(&kernel.build_dir)
            .arg(format!("M={}", build_dir.display()))
            .arg("modules")
            .args(&manifest.make_args);
        crate::builder::prepare_tool_command(&mut cmd, &Vec::new());
        let status = crate::interrupts::command_status(&mut cmd).with_context(|| {
            format!(
                "Failed to run kernel module build for {} on {}",
                manifest.name, kernel.version
            )
        })?;
        if !status.success() {
            anyhow::bail!(
                "Kernel module build failed for {}/{} on {}",
                manifest.name,
                manifest.version,
                kernel.version
            );
        }
    }
    Ok(())
}

fn run_pre_build_commands(
    rootfs: &Path,
    source_dir: &Path,
    manifest: &DepotKmodManifest,
    kernel: &KernelTarget,
) -> Result<()> {
    for raw in &manifest.pre_build {
        let command = expand_kernel_command(raw, rootfs, source_dir, kernel);
        if command.trim().is_empty() {
            continue;
        }
        crate::log_info!(
            "Preparing Depot DKMS module {}/{} for kernel {}: {}",
            manifest.name,
            manifest.version,
            kernel.version,
            command
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command).current_dir(source_dir);
        crate::builder::prepare_tool_command(&mut cmd, &Vec::new());
        let status = crate::interrupts::command_status(&mut cmd).with_context(|| {
            format!(
                "Failed to run DKMS pre-build command for {} on {}",
                manifest.name, kernel.version
            )
        })?;
        if !status.success() {
            anyhow::bail!(
                "DKMS pre-build command failed for {}/{} on {}: {}",
                manifest.name,
                manifest.version,
                kernel.version,
                command
            );
        }
    }
    Ok(())
}

fn expand_kernel_command(
    raw: &str,
    rootfs: &Path,
    source_dir: &Path,
    kernel: &KernelTarget,
) -> String {
    raw.replace("$kernel_build_dir", &shell_path(&kernel.build_dir))
        .replace("${kernel_build_dir}", &shell_path(&kernel.build_dir))
        .replace("$source_dir", &shell_path(source_dir))
        .replace("${source_dir}", &shell_path(source_dir))
        .replace("$rootfs", &shell_path(rootfs))
        .replace("${rootfs}", &shell_path(rootfs))
        .replace("$kernel", &kernel.version)
        .replace("${kernel}", &kernel.version)
}

fn shell_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn module_build_dirs(source_dir: &Path, manifest: &DepotKmodManifest) -> Result<Vec<PathBuf>> {
    let mut seen = BTreeSet::new();
    let mut dirs = Vec::new();
    for module in &manifest.modules {
        let rel = safe_rel_path(&module.build_dir)?;
        let dir = source_dir.join(rel);
        if seen.insert(dir.clone()) {
            dirs.push(dir);
        }
    }
    Ok(dirs)
}

fn install_for_kernel(
    rootfs: &Path,
    source_dir: &Path,
    manifest: &DepotKmodManifest,
    kernel: &KernelTarget,
) -> Result<Vec<InstalledKmodEntry>> {
    let mut entries = Vec::new();
    for module in &manifest.modules {
        let built_location = source_dir.join(safe_rel_path(&module.built_location)?);
        let built_module = built_location.join(format!("{}.ko", module.name));
        if !built_module.is_file() {
            anyhow::bail!(
                "Built module not found for {}/{} on {}: {}",
                manifest.name,
                module.name,
                kernel.version,
                built_module.display()
            );
        }

        let install_dir = safe_rel_path(&module.install_dir)?;
        let dest_rel = Path::new("lib/modules")
            .join(&kernel.version)
            .join(install_dir)
            .join(format!("{}.ko", module.dest_name));
        let dest = rootfs.join(&dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        fs::copy(&built_module, &dest).with_context(|| {
            format!(
                "Failed to install module {} to {}",
                built_module.display(),
                dest.display()
            )
        })?;
        entries.push(InstalledKmodEntry {
            kernel: kernel.version.clone(),
            module: module.dest_name.clone(),
            path: dest_rel.to_string_lossy().into_owned(),
        });
    }
    Ok(entries)
}

fn run_depmod(rootfs: &Path, kernel: &str) -> Result<()> {
    let mut cmd = Command::new("depmod");
    if rootfs == Path::new("/") {
        cmd.arg("-a").arg(kernel);
    } else {
        cmd.arg("-b").arg(rootfs).arg(kernel);
    }
    let status = crate::interrupts::command_status(&mut cmd)
        .with_context(|| format!("Failed to run depmod for kernel {}", kernel))?;
    if !status.success() {
        anyhow::bail!("depmod failed for kernel {}", kernel);
    }
    Ok(())
}

fn write_tracking_manifest(
    rootfs: &Path,
    manifest: &DepotKmodManifest,
    entries: Vec<InstalledKmodEntry>,
) -> Result<()> {
    let tracking_dir = rootfs.join(TRACKING_DIR_REL);
    fs::create_dir_all(&tracking_dir)
        .with_context(|| format!("Failed to create {}", tracking_dir.display()))?;
    let installed = InstalledKmodManifest {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        entries,
    };
    let path = tracking_manifest_path(rootfs, &manifest.name, &manifest.version);
    let raw = toml::to_string_pretty(&installed)
        .context("Failed to serialize Depot DKMS tracking manifest")?;
    fs::write(&path, raw).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

fn selected_tracking_manifests(
    rootfs: &Path,
    source: Option<&Path>,
    name: Option<&str>,
) -> Result<Vec<(PathBuf, InstalledKmodManifest)>> {
    let mut selected = BTreeMap::new();
    if let Some(source) = source {
        let source_dir = resolve_rootfs_path(rootfs, source);
        if source_dir.join(DEPOT_KMOD_MANIFEST).exists() {
            let manifest = read_source_manifest(&source_dir)?;
            let tracking = tracking_manifest_path(rootfs, &manifest.name, &manifest.version);
            if tracking.exists() {
                selected.insert(tracking.clone(), read_tracking_manifest(&tracking)?);
            }
        }
    }

    if let Some(name) = name {
        for (path, manifest) in tracking_manifests_by_name(rootfs, name)? {
            selected.insert(path, manifest);
        }
    }

    Ok(selected.into_iter().collect())
}

fn tracking_manifests_by_name(
    rootfs: &Path,
    name: &str,
) -> Result<Vec<(PathBuf, InstalledKmodManifest)>> {
    let tracking_dir = rootfs.join(TRACKING_DIR_REL);
    if !tracking_dir.exists() {
        return Ok(Vec::new());
    }
    let mut manifests = Vec::new();
    for entry in fs::read_dir(&tracking_dir)
        .with_context(|| format!("Failed to read {}", tracking_dir.display()))?
    {
        let entry = entry.with_context(|| format!("Failed to list {}", tracking_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let manifest = read_tracking_manifest(&path)?;
        if manifest.name == name {
            manifests.push((path, manifest));
        }
    }
    manifests.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(manifests)
}

fn read_tracking_manifest(path: &Path) -> Result<InstalledKmodManifest> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("Failed to parse {}", path.display()))
}

fn tracking_manifest_path(rootfs: &Path, name: &str, version: &str) -> PathBuf {
    rootfs.join(TRACKING_DIR_REL).join(format!(
        "{}-{}.toml",
        safe_tracking_component(name),
        safe_tracking_component(version)
    ))
}

fn safe_tracking_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '+' | '-' => ch,
            _ => '_',
        })
        .collect()
}

fn resolve_rootfs_path(rootfs: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        rootfs.join(path.strip_prefix(Path::new("/")).unwrap_or(path))
    } else {
        rootfs.join(path)
    }
}

fn safe_rel_path(raw: &str) -> Result<PathBuf> {
    let trimmed = raw.trim().trim_start_matches('/');
    if trimmed.is_empty() || trimmed == "." {
        return Ok(PathBuf::new());
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        anyhow::bail!("Expected relative path: {}", raw);
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => anyhow::bail!("Unsafe path component in DKMS manifest path: {}", raw),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use tempfile::tempdir;

    fn write_tracking(rootfs: &Path) -> Result<PathBuf> {
        let path = tracking_manifest_path(rootfs, "zfs", "2.4.3");
        fs::create_dir_all(path.parent().unwrap())?;
        let manifest = InstalledKmodManifest {
            name: "zfs".into(),
            version: "2.4.3".into(),
            entries: vec![InstalledKmodEntry {
                kernel: "6.1.0".into(),
                module: "zfs".into(),
                path: "lib/modules/6.1.0/updates/depot/zfs.ko".into(),
            }],
        };
        fs::write(&path, toml::to_string(&manifest)?)?;
        Ok(path)
    }

    #[test]
    fn remove_uses_tracking_manifest_and_only_removes_tracked_modules() -> Result<()> {
        let tmp = tempdir()?;
        let rootfs = tmp.path();
        let module_path = rootfs.join("lib/modules/6.1.0/updates/depot/zfs.ko");
        fs::create_dir_all(module_path.parent().unwrap())?;
        fs::write(&module_path, "module")?;
        let tracking = write_tracking(rootfs)?;

        let depmod = rootfs.join("depmod-bin");
        fs::write(&depmod, "#!/bin/sh\nexit 0\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&depmod)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&depmod, perms)?;
        }

        let mut env = TestEnv::new();
        env.set_var("PATH", rootfs);
        fs::rename(&depmod, rootfs.join("depmod"))?;
        remove(rootfs, None, Some("zfs"))?;

        assert!(!module_path.exists());
        assert!(!tracking.exists());
        Ok(())
    }

    #[test]
    fn safe_rel_path_rejects_traversal() {
        assert!(safe_rel_path("../x").is_err());
        assert!(safe_rel_path("updates/depot").is_ok());
    }
}
