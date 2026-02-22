//! Lifecycle script staging, installation, and execution.

use crate::fakeroot;
use crate::package::PackageSpec;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

const STAGED_SCRIPTS_DIR: &str = "scripts";
const ALL_HOOKS: [Hook; 6] = [
    Hook::PreInstall,
    Hook::PostInstall,
    Hook::PreUpdate,
    Hook::PostUpdate,
    Hook::PreRemove,
    Hook::PostRemove,
];

/// Lifecycle hook names supported by Depot package scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    /// Runs before a first-time install.
    PreInstall,
    /// Runs after a first-time install.
    PostInstall,
    /// Runs before an update/upgrade.
    PreUpdate,
    /// Runs after an update/upgrade.
    PostUpdate,
    /// Runs before package removal.
    PreRemove,
    /// Runs after package removal.
    PostRemove,
}

impl Hook {
    fn canonical_name(self) -> &'static str {
        match self {
            Hook::PreInstall => "pre_install",
            Hook::PostInstall => "post_install",
            Hook::PreUpdate => "pre_update",
            Hook::PostUpdate => "post_update",
            Hook::PreRemove => "pre_remove",
            Hook::PostRemove => "post_remove",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Hook::PreInstall | Hook::PostInstall => "install",
            Hook::PreUpdate | Hook::PostUpdate => "update",
            Hook::PreRemove | Hook::PostRemove => "remove",
        }
    }

    fn phase(self) -> &'static str {
        match self {
            Hook::PreInstall | Hook::PreUpdate | Hook::PreRemove => "pre",
            Hook::PostInstall | Hook::PostUpdate | Hook::PostRemove => "post",
        }
    }

    fn candidate_names(self) -> [String; 6] {
        let canonical = self.canonical_name();
        let dashed = canonical.replace('_', "-");
        let compact = canonical.replace('_', "");
        [
            canonical.to_string(),
            format!("{}.sh", canonical),
            dashed.clone(),
            format!("{}.sh", dashed),
            compact.clone(),
            format!("{}.sh", compact),
        ]
    }

    fn legacy_root_candidate_names(self) -> [String; 2] {
        let compact = self.canonical_name().replace('_', "");
        [compact.clone(), format!("{}.sh", compact)]
    }
}

/// Return the staged scripts directory path inside a package staging tree.
pub fn staged_scripts_dir(destdir: &Path) -> PathBuf {
    destdir.join(STAGED_SCRIPTS_DIR)
}

/// Return the installed scripts directory path inside the root filesystem.
pub fn installed_scripts_dir(rootfs: &Path, pkg_name: &str) -> PathBuf {
    rootfs
        .join("usr/share/depot")
        .join(pkg_name)
        .join("scripts")
}

/// Copy optional scripts from `<specdir>/scripts` into `<destdir>/scripts`.
///
/// Also stages legacy root hook files (for example `postinstall.sh`) into their
/// canonical names under `<destdir>/scripts`.
///
/// Returns `true` if scripts were found and staged.
pub fn stage_scripts_from_spec_dir(spec: &PackageSpec, destdir: &Path) -> Result<bool> {
    let source_dir = spec.spec_dir.join(STAGED_SCRIPTS_DIR);
    let has_scripts_dir = if source_dir.exists() { true } else { false };

    if has_scripts_dir && !source_dir.is_dir() {
        bail!(
            "Scripts path exists but is not a directory: {}",
            source_dir.display()
        );
    }

    let legacy_hooks = collect_legacy_root_hooks(&spec.spec_dir)?;
    if !has_scripts_dir && legacy_hooks.is_empty() {
        return Ok(false);
    }

    let staged_dir = staged_scripts_dir(destdir);
    if staged_dir.exists() {
        remove_tree_or_file(&staged_dir).with_context(|| {
            format!(
                "Failed to remove existing staged scripts dir: {}",
                staged_dir.display()
            )
        })?;
    }

    if has_scripts_dir {
        copy_tree(&source_dir, &staged_dir)
            .with_context(|| format!("Failed to stage scripts from {}", source_dir.display()))?;
    }

    stage_legacy_root_hooks(&legacy_hooks, &staged_dir)?;

    Ok(true)
}

/// Synchronize staged scripts into `/usr/share/depot/<pkgname>/scripts`.
///
/// If no staged scripts are present, any previously installed scripts for the
/// package are removed.
///
/// Returns `true` when scripts are present after synchronization.
pub fn sync_staged_scripts_to_rootfs(
    staged_dir: &Path,
    rootfs: &Path,
    pkg_name: &str,
) -> Result<bool> {
    let installed_dir = installed_scripts_dir(rootfs, pkg_name);

    if staged_dir.exists() {
        if !staged_dir.is_dir() {
            bail!(
                "Staged scripts path exists but is not a directory: {}",
                staged_dir.display()
            );
        }

        if installed_dir.exists() {
            remove_tree_or_file(&installed_dir).with_context(|| {
                format!(
                    "Failed to remove existing installed scripts: {}",
                    installed_dir.display()
                )
            })?;
        }

        copy_tree(staged_dir, &installed_dir).with_context(|| {
            format!("Failed to install scripts into {}", installed_dir.display())
        })?;

        return Ok(true);
    }

    remove_installed_scripts(rootfs, pkg_name)?;
    Ok(false)
}

/// Remove installed scripts for a package and clean empty package metadata dir.
pub fn remove_installed_scripts(rootfs: &Path, pkg_name: &str) -> Result<()> {
    let installed_dir = installed_scripts_dir(rootfs, pkg_name);
    if installed_dir.exists() {
        remove_tree_or_file(&installed_dir).with_context(|| {
            format!(
                "Failed to remove installed scripts directory: {}",
                installed_dir.display()
            )
        })?;
    }

    cleanup_empty_package_dir(rootfs, pkg_name)?;
    Ok(())
}

/// Run a lifecycle hook if its script exists in `script_dir`.
///
/// Returns `true` if a hook script was found and executed.
pub fn run_hook_if_present(
    script_dir: &Path,
    hook: Hook,
    rootfs: &Path,
    pkg_name: &str,
) -> Result<bool> {
    let Some(script_path) = resolve_hook_script(script_dir, hook)? else {
        return Ok(false);
    };

    crate::log_info!(
        "Running lifecycle hook {}: {}",
        hook.canonical_name(),
        script_path.display()
    );

    let status = run_script_with_rootfs_context(&script_path, rootfs, pkg_name, hook)?;

    if !status.success() {
        bail!(
            "Lifecycle hook {} failed: {}",
            hook.canonical_name(),
            script_path.display()
        );
    }

    Ok(true)
}

fn run_script_with_rootfs_context(
    script_path: &Path,
    rootfs: &Path,
    pkg_name: &str,
    hook: Hook,
) -> Result<std::process::ExitStatus> {
    // When root and rootfs provides /bin/sh, run inside a real chroot so scripts
    // can rely on `cd /` and relative paths resolving within that rootfs.
    if fakeroot::is_root()
        && rootfs.join("bin/sh").exists()
        && let Ok(rel) = script_path.strip_prefix(rootfs)
    {
        let rel_script = format!("./{}", rel.to_string_lossy());
        return Command::new("chroot")
            .arg(rootfs)
            .arg("/bin/sh")
            .arg("-c")
            .arg("cd / && exec /bin/sh \"$1\"")
            .arg("sh")
            .arg(rel_script)
            .env("DEPOT_PACKAGE", pkg_name)
            .env("DEPOT_ROOTFS", rootfs)
            .env("DEPOT_ACTION", hook.action())
            .env("DEPOT_PHASE", hook.phase())
            .status()
            .with_context(|| {
                format!(
                    "Failed to execute lifecycle hook {} in chroot at {}",
                    hook.canonical_name(),
                    rootfs.display()
                )
            });
    }

    // Fallback (non-root / no rootfs shell / script outside rootfs):
    // execute with host /bin/sh while setting cwd to rootfs, so relative paths
    // inside scripts resolve against that rootfs root.
    Command::new("/bin/sh")
        .arg(script_path)
        .current_dir(rootfs)
        .env("DEPOT_PACKAGE", pkg_name)
        .env("DEPOT_ROOTFS", rootfs)
        .env("DEPOT_ACTION", hook.action())
        .env("DEPOT_PHASE", hook.phase())
        .status()
        .with_context(|| {
            format!(
                "Failed to execute lifecycle hook {} at {}",
                hook.canonical_name(),
                script_path.display()
            )
        })
}

fn resolve_hook_script(script_dir: &Path, hook: Hook) -> Result<Option<PathBuf>> {
    if !script_dir.exists() {
        return Ok(None);
    }

    if !script_dir.is_dir() {
        bail!(
            "Scripts path exists but is not a directory: {}",
            script_dir.display()
        );
    }

    let mut found = Vec::new();

    for candidate in hook.candidate_names() {
        let path = script_dir.join(&candidate);
        let metadata = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to inspect script path: {}", path.display()));
            }
        };

        let file_type = metadata.file_type();
        if !file_type.is_file() && !file_type.is_symlink() {
            bail!(
                "Lifecycle hook candidate exists but is not a file: {}",
                path.display()
            );
        }

        found.push(path);
    }

    if found.len() > 1 {
        let names = found
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "Ambiguous lifecycle hook '{}': multiple script candidates found: {}",
            hook.canonical_name(),
            names
        );
    }

    Ok(found.into_iter().next())
}

fn collect_legacy_root_hooks(spec_dir: &Path) -> Result<Vec<(Hook, PathBuf)>> {
    let mut found = Vec::new();

    for hook in ALL_HOOKS {
        let mut hook_matches = Vec::new();
        for candidate in hook.legacy_root_candidate_names() {
            let path = spec_dir.join(candidate);
            let metadata = match path.symlink_metadata() {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to inspect legacy hook script: {}", path.display())
                    });
                }
            };

            let file_type = metadata.file_type();
            if !file_type.is_file() && !file_type.is_symlink() {
                bail!(
                    "Legacy lifecycle hook candidate exists but is not a file: {}",
                    path.display()
                );
            }

            hook_matches.push(path);
        }

        if hook_matches.len() > 1 {
            let names = hook_matches
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "Ambiguous legacy lifecycle hook '{}': multiple script candidates found: {}",
                hook.canonical_name(),
                names
            );
        }

        if let Some(path) = hook_matches.into_iter().next() {
            found.push((hook, path));
        }
    }

    Ok(found)
}

fn stage_legacy_root_hooks(legacy_hooks: &[(Hook, PathBuf)], staged_dir: &Path) -> Result<()> {
    if legacy_hooks.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(staged_dir)
        .with_context(|| format!("Failed to create directory: {}", staged_dir.display()))?;

    for (hook, src_path) in legacy_hooks {
        let dst_path = staged_dir.join(hook.canonical_name());
        if dst_path.exists() {
            bail!(
                "Lifecycle hook '{}' is defined more than once (legacy root hook conflicts with scripts/ entry): {}",
                hook.canonical_name(),
                dst_path.display()
            );
        }

        let metadata = src_path.symlink_metadata().with_context(|| {
            format!(
                "Failed to inspect legacy hook script: {}",
                src_path.display()
            )
        })?;

        if metadata.file_type().is_symlink() {
            let target = fs::read_link(src_path)
                .with_context(|| format!("Failed to read symlink: {}", src_path.display()))?;
            std::os::unix::fs::symlink(target, &dst_path)
                .with_context(|| format!("Failed to create symlink: {}", dst_path.display()))?;
        } else {
            fs::copy(src_path, &dst_path).with_context(|| {
                format!(
                    "Failed to copy legacy hook '{}' to '{}'",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
            ensure_executable(&dst_path)?;
        }
    }

    Ok(())
}

fn safe_rel_path(rel: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(seg) => normalized.push(seg),
            Component::CurDir => {}
            _ => {
                bail!(
                    "Unsafe scripts path component encountered: {}",
                    rel.display()
                )
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        bail!("Scripts path resolves to empty path: {}", rel.display());
    }

    Ok(normalized)
}

fn copy_tree(src_root: &Path, dst_root: &Path) -> Result<()> {
    fs::create_dir_all(dst_root)
        .with_context(|| format!("Failed to create directory: {}", dst_root.display()))?;

    for entry in WalkDir::new(src_root).follow_links(false) {
        let entry = entry
            .with_context(|| format!("Failed to walk scripts directory: {}", src_root.display()))?;

        let src_path = entry.path();
        let rel = src_path
            .strip_prefix(src_root)
            .with_context(|| format!("Failed to strip script root: {}", src_path.display()))?;

        if rel.as_os_str().is_empty() {
            continue;
        }

        let rel = safe_rel_path(rel)?;
        let dst_path = dst_root.join(rel);
        let file_type = entry.file_type();

        if file_type.is_dir() {
            fs::create_dir_all(&dst_path)
                .with_context(|| format!("Failed to create directory: {}", dst_path.display()))?;
            continue;
        }

        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        if file_type.is_symlink() {
            let target = fs::read_link(src_path)
                .with_context(|| format!("Failed to read symlink: {}", src_path.display()))?;
            if dst_path.symlink_metadata().is_ok() {
                remove_tree_or_file(&dst_path).with_context(|| {
                    format!("Failed to replace existing path: {}", dst_path.display())
                })?;
            }
            std::os::unix::fs::symlink(target, &dst_path)
                .with_context(|| format!("Failed to create symlink: {}", dst_path.display()))?;
        } else if file_type.is_file() {
            fs::copy(src_path, &dst_path).with_context(|| {
                format!(
                    "Failed to copy script '{}' to '{}'",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
            ensure_executable(&dst_path)?;
        } else {
            bail!(
                "Unsupported file type in scripts directory: {}",
                src_path.display()
            );
        }
    }

    Ok(())
}

fn ensure_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path)
            .with_context(|| format!("Failed to inspect script permissions: {}", path.display()))?;
        let mut perms = metadata.permissions();
        let mode = perms.mode();
        let new_mode = mode | 0o111;
        if new_mode != mode {
            perms.set_mode(new_mode);
            fs::set_permissions(path, perms).with_context(|| {
                format!("Failed to set executable permissions: {}", path.display())
            })?;
        }
    }

    Ok(())
}

fn remove_tree_or_file(path: &Path) -> Result<()> {
    let metadata = path
        .symlink_metadata()
        .with_context(|| format!("Failed to inspect path: {}", path.display()))?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("Failed to remove directory: {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("Failed to remove file: {}", path.display()))?;
    }
    Ok(())
}

fn cleanup_empty_package_dir(rootfs: &Path, pkg_name: &str) -> Result<()> {
    let package_dir = rootfs.join("usr/share/depot").join(pkg_name);
    if !package_dir.exists() {
        return Ok(());
    }

    let is_empty = fs::read_dir(&package_dir)
        .with_context(|| format!("Failed to read directory: {}", package_dir.display()))?
        .next()
        .is_none();

    if is_empty {
        fs::remove_dir(&package_dir)
            .with_context(|| format!("Failed to remove directory: {}", package_dir.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, Source,
    };
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn mk_spec(spec_dir: &Path) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: spec_dir.to_path_buf(),
        }
    }

    #[test]
    fn stage_scripts_from_spec_dir_copies_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(spec_dir.join("scripts/lib")).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();

        std::fs::write(spec_dir.join("scripts/pre_install"), "echo pre").unwrap();
        std::fs::write(spec_dir.join("scripts/lib/common.sh"), "echo lib").unwrap();

        let spec = mk_spec(&spec_dir);
        let staged = stage_scripts_from_spec_dir(&spec, &destdir).unwrap();
        assert!(staged);
        assert!(destdir.join("scripts/pre_install").exists());
        assert!(destdir.join("scripts/lib/common.sh").exists());
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(destdir.join("scripts/pre_install"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
        }
    }

    #[test]
    fn run_hook_if_present_executes_script() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = tmp.path().join("scripts");
        let rootfs = tmp.path().join("root");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();

        std::fs::write(
            scripts.join("pre_install"),
            "echo \"$DEPOT_ACTION:$DEPOT_PHASE:$DEPOT_PACKAGE\" > \"$DEPOT_ROOTFS/hook.out\"\n",
        )
        .unwrap();

        let ran = run_hook_if_present(&scripts, Hook::PreInstall, &rootfs, "foo").unwrap();
        assert!(ran);
        assert_eq!(
            std::fs::read_to_string(rootfs.join("hook.out")).unwrap(),
            "install:pre:foo\n"
        );
    }

    #[test]
    fn run_hook_if_present_accepts_compact_script_name() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = tmp.path().join("scripts");
        let rootfs = tmp.path().join("root");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();

        std::fs::write(
            scripts.join("postinstall.sh"),
            "echo compact > \"$DEPOT_ROOTFS/hook.out\"\n",
        )
        .unwrap();

        let ran = run_hook_if_present(&scripts, Hook::PostInstall, &rootfs, "foo").unwrap();
        assert!(ran);
        assert_eq!(
            std::fs::read_to_string(rootfs.join("hook.out")).unwrap(),
            "compact\n"
        );
    }

    #[test]
    fn run_hook_if_present_rejects_ambiguous_names() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = tmp.path().join("scripts");
        let rootfs = tmp.path().join("root");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();

        std::fs::write(scripts.join("pre_update"), "echo one").unwrap();
        std::fs::write(scripts.join("pre-update"), "echo two").unwrap();

        let err = run_hook_if_present(&scripts, Hook::PreUpdate, &rootfs, "foo")
            .expect_err("expected ambiguous script names to fail");
        assert!(err.to_string().contains("Ambiguous lifecycle hook"));
    }

    #[test]
    fn sync_staged_scripts_to_rootfs_replaces_existing_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let staged = tmp.path().join("staged");
        std::fs::create_dir_all(staged.join("scripts")).unwrap();

        let installed = installed_scripts_dir(&rootfs, "foo");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("old"), "old").unwrap();

        std::fs::write(staged.join("scripts/post_install"), "echo ok").unwrap();
        let has_scripts =
            sync_staged_scripts_to_rootfs(&staged.join("scripts"), &rootfs, "foo").unwrap();

        assert!(has_scripts);
        let installed = installed_scripts_dir(&rootfs, "foo");
        assert!(!installed.join("old").exists());
        assert!(installed.join("post_install").exists());
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(installed.join("post_install"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
        }
    }

    #[test]
    fn sync_staged_scripts_to_rootfs_removes_old_when_none_staged() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("root");
        let staged = tmp.path().join("staged");
        std::fs::create_dir_all(&staged).unwrap();

        let installed = installed_scripts_dir(&rootfs, "foo");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("pre_remove"), "echo old").unwrap();

        let has_scripts =
            sync_staged_scripts_to_rootfs(&staged.join("scripts"), &rootfs, "foo").unwrap();
        assert!(!has_scripts);
        assert!(!installed_scripts_dir(&rootfs, "foo").exists());
    }

    #[test]
    fn stage_scripts_from_spec_dir_stages_legacy_root_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();

        std::fs::write(spec_dir.join("postinstall.sh"), "echo post").unwrap();

        let spec = mk_spec(&spec_dir);
        let staged = stage_scripts_from_spec_dir(&spec, &destdir).unwrap();
        assert!(staged);
        assert!(destdir.join("scripts/post_install").exists());
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(destdir.join("scripts/post_install"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0);
        }
    }
}
