use crate::config::Config;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const STATE_FILENAME: &str = "system.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SystemState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) arch: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) layers: BTreeMap<String, Vec<String>>,
}

pub(crate) fn state_path(config: &Config) -> PathBuf {
    config.db_dir.join(STATE_FILENAME)
}

pub(crate) fn load(config: &Config) -> Result<SystemState> {
    let path = state_path(config);
    if !path.exists() {
        return Ok(SystemState::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

pub(crate) fn save(config: &Config, state: &SystemState) -> Result<PathBuf> {
    let path = state_path(config);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(state).context("Failed to serialize system state")?;
    fs::write(&path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

pub(crate) fn set_stage(config: &Config, stage: String) -> Result<SystemState> {
    let stage = normalize_token("stage", &stage)?;
    let mut state = load(config)?;
    state.stage = Some(stage);
    save(config, &state)?;
    Ok(state)
}

pub(crate) fn add_packages_to_layer(
    config: &Config,
    layer: String,
    packages: &[String],
) -> Result<SystemState> {
    let layer = normalize_token("layer", &layer)?;
    let mut state = load(config)?;
    let existing = state.layers.remove(&layer).unwrap_or_default();
    let mut merged = existing.into_iter().collect::<BTreeSet<_>>();
    for package in packages {
        merged.insert(normalize_token("package", package)?);
    }
    state.layers.insert(layer, merged.into_iter().collect());
    save(config, &state)?;
    Ok(state)
}

pub(crate) fn set_layer_packages(
    config: &Config,
    layer: String,
    packages: &[String],
) -> Result<SystemState> {
    let layer = normalize_token("layer", &layer)?;
    let mut state = load(config)?;
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for package in packages {
        let package = normalize_token("package", package)?;
        if seen.insert(package.clone()) {
            normalized.push(package);
        }
    }
    state.layers.insert(layer, normalized);
    save(config, &state)?;
    Ok(state)
}

pub(crate) fn remove_packages_from_layer(
    config: &Config,
    layer: String,
    packages: &[String],
) -> Result<SystemState> {
    let layer = normalize_token("layer", &layer)?;
    let remove = packages
        .iter()
        .map(|package| normalize_token("package", package))
        .collect::<Result<BTreeSet<_>>>()?;
    let mut state = load(config)?;
    if let Some(existing) = state.layers.get_mut(&layer) {
        existing.retain(|package| !remove.contains(package));
        if existing.is_empty() {
            state.layers.remove(&layer);
        }
    }
    save(config, &state)?;
    Ok(state)
}

pub(crate) fn init_lbi_layout(
    rootfs: &Path,
    config: &Config,
    target: &str,
    arch: Option<&str>,
    force: bool,
) -> Result<SystemState> {
    let target = normalize_token("target", target)?;
    let arch = match arch {
        Some(arch) => normalize_token("arch", arch)?,
        None => crate::cross::target_arch_from_triple(&target).to_string(),
    };

    ensure_lbi_layout_paths(rootfs)?;
    write_lbi_build_config(rootfs, &target, &arch, force)?;

    let mut state = load(config)?;
    state.target = Some(target);
    state.arch = Some(arch);
    state.stage.get_or_insert_with(|| "layout".to_string());
    save(config, &state)?;
    Ok(state)
}

pub(crate) fn ensure_lbi_layout_paths(rootfs: &Path) -> Result<()> {
    create_lbi_directories(rootfs)?;
    create_lbi_links(rootfs)?;
    Ok(())
}

fn normalize_token(kind: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{kind} must not be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\0') {
        bail!("{kind} must not contain '/' or NUL bytes: {trimmed}");
    }
    Ok(trimmed.to_string())
}

fn create_lbi_directories(rootfs: &Path) -> Result<()> {
    let dirs = [
        "system",
        "system/configuration",
        "system/binaries",
        "system/systembinaries",
        "system/libraries",
        "system/headers",
        "system/share",
        "system/documentation",
        "system/documentation/man-pages",
        "system/documentation/info",
        "system/tools",
        "system/variable",
        "system/variable/lib",
        "system/users",
        "system/charlie",
        "system/devices",
        "system/devices/pts",
        "system/devices/shm",
        "system/processes",
        "system/run",
        "system/system",
        "system/temporary",
        "usr",
    ];

    for dir in dirs {
        let path = rootfs.join(dir);
        fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
    }
    Ok(())
}

fn create_lbi_links(rootfs: &Path) -> Result<()> {
    let links = [
        ("etc", "system/configuration"),
        ("bin", "system/binaries"),
        ("sbin", "system/systembinaries"),
        ("lib", "system/libraries"),
        ("var", "system/variable"),
        ("home", "system/users"),
        ("root", "system/charlie"),
        ("dev", "system/devices"),
        ("proc", "system/processes"),
        ("run", "system/run"),
        ("sys", "system/system"),
        ("usr/bin", "../system/binaries"),
        ("usr/sbin", "../system/systembinaries"),
        ("usr/lib", "../system/libraries"),
        ("usr/include", "../system/headers"),
        ("usr/share", "../system/share"),
        ("system/lib", "libraries"),
    ];

    for (link, target) in links {
        ensure_relative_symlink(rootfs, Path::new(link), Path::new(target))?;
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_relative_symlink(rootfs: &Path, link: &Path, target: &Path) -> Result<()> {
    use std::os::unix::fs as unix_fs;

    let link_path = rootfs.join(link);
    if let Ok(metadata) = fs::symlink_metadata(&link_path) {
        if !metadata.file_type().is_symlink() {
            if metadata.is_dir() {
                let target_path = link_path
                    .parent()
                    .unwrap_or(rootfs)
                    .join(target)
                    .components()
                    .collect::<PathBuf>();
                merge_dir_contents(&link_path, &target_path).with_context(|| {
                    format!(
                        "Failed to merge existing directory {} into {}",
                        link_path.display(),
                        target_path.display()
                    )
                })?;
                fs::remove_dir(&link_path)
                    .with_context(|| format!("Failed to remove {}", link_path.display()))?;
            } else {
                bail!(
                    "Refusing to replace non-directory path while creating LBI layout: {}",
                    link_path.display()
                );
            }
        } else {
            let existing = fs::read_link(&link_path)
                .with_context(|| format!("Failed to read symlink {}", link_path.display()))?;
            if existing == target {
                return Ok(());
            }
            fs::remove_file(&link_path)
                .with_context(|| format!("Failed to replace symlink {}", link_path.display()))?;
        }
    }
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    unix_fs::symlink(target, &link_path)
        .with_context(|| format!("Failed to create symlink {}", link_path.display()))?;
    Ok(())
}

fn merge_dir_contents(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("Failed to create {}", dest.display()))?;
    let mut entries = fs::read_dir(src)
        .with_context(|| format!("Failed to read {}", src.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to read entry from {}", src.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let source_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if dest_path.exists() {
            let source_type = entry
                .file_type()
                .with_context(|| format!("Failed to inspect {}", source_path.display()))?;
            let dest_type = fs::symlink_metadata(&dest_path)
                .with_context(|| format!("Failed to inspect {}", dest_path.display()))?
                .file_type();
            if source_type.is_dir() && dest_type.is_dir() {
                merge_dir_contents(&source_path, &dest_path)?;
                fs::remove_dir(&source_path)
                    .with_context(|| format!("Failed to remove {}", source_path.display()))?;
                continue;
            }
            bail!(
                "Refusing to overwrite existing path while merging LBI directory: {}",
                dest_path.display()
            );
        }
        fs::rename(&source_path, &dest_path).with_context(|| {
            format!(
                "Failed to move {} to {}",
                source_path.display(),
                dest_path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_relative_symlink(_rootfs: &Path, _link: &Path, _target: &Path) -> Result<()> {
    bail!("LBI layout initialization requires Unix symlink support")
}

fn write_lbi_build_config(rootfs: &Path, target: &str, arch: &str, force: bool) -> Result<PathBuf> {
    let path = rootfs.join("etc/depot.d/build.toml");
    if path.exists() && !force {
        bail!(
            "{} already exists; re-run with --force to replace it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let content = lbi_build_config_toml(target, arch);
    fs::write(&path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

fn lbi_build_config_toml(target: &str, arch: &str) -> String {
    let makeflags = format!(
        "-j{}",
        std::thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(1)
    );
    format!(
        r#"# Generated by `depot system init-lbi`.
# These defaults mirror the Linux by Intent /system layout.

[flags]
prefix = "/system"
bindir = "/system/binaries"
sbindir = "/system/systembinaries"
libdir = "/system/libraries"
libexecdir = "/system/libraries"
sysconfdir = "/system/configuration"
localstatedir = "/system/variable"
sharedstatedir = "/system/variable/lib"
includedir = "/system/headers"
datarootdir = "/system/share"
datadir = "/system/share"
mandir = "/system/documentation/man-pages"
infodir = "/system/documentation/info"
tool_dir = "/system/tools/bin"
cc = "$TOOL_DIR/clang"
cxx = "$TOOL_DIR/clang++"
ar = "$TOOL_DIR/llvm-ar"
ld = "$TOOL_DIR/ld.lld"
carch = "{arch}"
chost = "{target}"
target = "{target}"
cflags = ["-O2", "-pipe"]
cxxflags = ["-O2", "-pipe"]
ldflags = ["-Wl,-rpath,/system/libraries"]
makeflags = "{makeflags}"
"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_add_sorts_and_deduplicates_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::for_rootfs(tmp.path());
        let state = add_packages_to_layer(
            &config,
            "base".to_string(),
            &["zlib".to_string(), "musl".to_string(), "zlib".to_string()],
        )
        .unwrap();
        assert_eq!(state.layers["base"], vec!["musl", "zlib"]);
    }

    #[test]
    fn set_layer_packages_replaces_only_named_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::for_rootfs(tmp.path());
        add_packages_to_layer(&config, "custom".to_string(), &["kept".to_string()]).unwrap();
        add_packages_to_layer(
            &config,
            "base".to_string(),
            &["old".to_string(), "zlib".to_string()],
        )
        .unwrap();
        let state = set_layer_packages(
            &config,
            "base".to_string(),
            &["zlib".to_string(), "musl".to_string(), "zlib".to_string()],
        )
        .unwrap();
        assert_eq!(state.layers["base"], vec!["zlib", "musl"]);
        assert_eq!(state.layers["custom"], vec!["kept"]);
    }

    #[test]
    fn stage_is_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::for_rootfs(tmp.path());
        set_stage(&config, "cross-tools".to_string()).unwrap();
        let loaded = load(&config).unwrap();
        assert_eq!(loaded.stage.as_deref(), Some("cross-tools"));
    }

    #[cfg(unix)]
    #[test]
    fn init_lbi_creates_layout_and_build_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::for_rootfs(tmp.path());
        let state = init_lbi_layout(
            tmp.path(),
            &config,
            "x86_64-unknown-linux-musl",
            None,
            false,
        )
        .unwrap();
        assert_eq!(state.stage.as_deref(), Some("layout"));
        assert_eq!(state.arch.as_deref(), Some("x86_64"));
        assert!(tmp.path().join("system/binaries").is_dir());
        assert!(tmp.path().join("system/devices/pts").is_dir());
        assert!(tmp.path().join("system/devices/shm").is_dir());
        assert_eq!(
            fs::read_link(tmp.path().join("usr/bin")).unwrap(),
            PathBuf::from("../system/binaries")
        );
        assert_eq!(
            fs::read_link(tmp.path().join("dev")).unwrap(),
            PathBuf::from("system/devices")
        );
        assert_eq!(
            fs::read_link(tmp.path().join("proc")).unwrap(),
            PathBuf::from("system/processes")
        );
        assert_eq!(
            fs::read_link(tmp.path().join("sys")).unwrap(),
            PathBuf::from("system/system")
        );
        assert_eq!(
            fs::read_link(tmp.path().join("run")).unwrap(),
            PathBuf::from("system/run")
        );
        let build_toml = fs::read_to_string(tmp.path().join("etc/depot.d/build.toml")).unwrap();
        assert!(build_toml.contains("prefix = \"/system\""));
        assert!(build_toml.contains("chost = \"x86_64-unknown-linux-musl\""));
        let makeflags_line = build_toml
            .lines()
            .find(|line| line.trim_start().starts_with("makeflags = \"-j"))
            .expect("expected makeflags default in generated build.toml");
        let jobs = makeflags_line
            .trim()
            .trim_start_matches("makeflags = \"-j")
            .trim_end_matches('"')
            .parse::<usize>()
            .expect("expected numeric makeflags job count");
        assert!(jobs >= 1);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_lbi_layout_paths_reconciles_existing_usr_include_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("usr/include")).unwrap();
        fs::write(tmp.path().join("usr/include/marker.h"), "/* marker */").unwrap();

        ensure_lbi_layout_paths(tmp.path()).unwrap();

        assert_eq!(
            fs::read_link(tmp.path().join("usr/include")).unwrap(),
            PathBuf::from("../system/headers")
        );
        assert!(tmp.path().join("system/headers/marker.h").exists());
    }

    #[cfg(unix)]
    #[test]
    fn init_lbi_migrates_existing_var_contents_before_linking() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("var/lib/depot")).unwrap();
        fs::write(tmp.path().join("var/lib/depot/lock"), "").unwrap();
        let config = Config::for_rootfs(tmp.path());
        init_lbi_layout(
            tmp.path(),
            &config,
            "x86_64-unknown-linux-musl",
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            fs::read_link(tmp.path().join("var")).unwrap(),
            PathBuf::from("system/variable")
        );
        assert!(tmp.path().join("system/variable/lib/depot/lock").exists());
    }
}
