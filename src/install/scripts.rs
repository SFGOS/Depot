//! Lifecycle script staging, installation, and execution.

use crate::fakeroot;
use crate::package::PackageSpec;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use sys_mount::{Mount, MountFlags, Unmount, UnmountFlags};
use walkdir::WalkDir;

const STAGED_SCRIPTS_DIR: &str = "scripts";
const DEFERRED_HOOKS_FILE_REL: &str = "var/lib/depot/deferred-hooks.tsv";
const BOOTSTRAP_BIN_DIR_REL: &str = "var/lib/depot/bootstrap/bin";
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

    fn from_canonical_name(name: &str) -> Option<Self> {
        match name {
            "pre_install" => Some(Hook::PreInstall),
            "post_install" => Some(Hook::PostInstall),
            "pre_update" => Some(Hook::PreUpdate),
            "post_update" => Some(Hook::PostUpdate),
            "pre_remove" => Some(Hook::PreRemove),
            "post_remove" => Some(Hook::PostRemove),
            _ => None,
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

    fn lib32_candidate_names(self) -> Vec<String> {
        let canonical = self.canonical_name();
        let dashed = canonical.replace('_', "-");
        let compact = canonical.replace('_', "");
        vec![
            format!("{canonical}-lib32"),
            format!("{canonical}-lib32.sh"),
            format!("{dashed}-lib32"),
            format!("{dashed}-lib32.sh"),
            format!("{compact}-lib32"),
            format!("{compact}-lib32.sh"),
            format!("lib32-{canonical}"),
            format!("lib32-{canonical}.sh"),
            format!("lib32-{dashed}"),
            format!("lib32-{dashed}.sh"),
            format!("lib32-{compact}"),
            format!("lib32-{compact}.sh"),
        ]
    }

    fn legacy_root_candidate_names(self) -> [String; 2] {
        let compact = self.canonical_name().replace('_', "");
        [compact.clone(), format!("{}.sh", compact)]
    }

    fn legacy_root_lib32_candidate_names(self) -> [String; 2] {
        let compact = self.canonical_name().replace('_', "");
        [format!("lib32-{compact}"), format!("lib32-{compact}.sh")]
    }
}

#[derive(Debug, Clone)]
struct DeferredHook {
    pkg_name: String,
    hook: Hook,
    script_rel: PathBuf,
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
    let has_scripts_dir = source_dir.exists();

    if has_scripts_dir && !source_dir.is_dir() {
        bail!(
            "Scripts path exists but is not a directory: {}",
            source_dir.display()
        );
    }

    let legacy_hooks = collect_legacy_root_hooks(&spec.spec_dir, &spec.package.name)?;
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
    Ok(matches!(
        dispatch_hook_if_present(script_dir, hook, rootfs, pkg_name, false)?,
        HookDispatch::Ran
    ))
}

/// Run a lifecycle hook if present, or queue it for later replay when the
/// target rootfs cannot yet execute scripts inside a real chroot.
///
/// Returns `true` when a hook script was found and either executed or queued.
pub fn run_hook_if_present_or_defer(
    script_dir: &Path,
    hook: Hook,
    rootfs: &Path,
    pkg_name: &str,
) -> Result<bool> {
    Ok(matches!(
        dispatch_hook_if_present(script_dir, hook, rootfs, pkg_name, true)?,
        HookDispatch::Ran | HookDispatch::Deferred
    ))
}

enum HookDispatch {
    Missing,
    Ran,
    Deferred,
}

fn dispatch_hook_if_present(
    script_dir: &Path,
    hook: Hook,
    rootfs: &Path,
    pkg_name: &str,
    allow_defer: bool,
) -> Result<HookDispatch> {
    let Some(script_path) = resolve_hook_script(script_dir, hook, pkg_name)? else {
        return Ok(HookDispatch::Missing);
    };

    crate::log_info!(
        "Running lifecycle hook {}: {}",
        hook.canonical_name(),
        script_path.display()
    );

    match run_script_with_rootfs_context(&script_path, rootfs, pkg_name, hook, allow_defer)? {
        HookRunOutcome::Ran(status) => {
            if !status.success() {
                bail!(
                    "Lifecycle hook {} failed: {}",
                    hook.canonical_name(),
                    script_path.display()
                );
            }
            Ok(HookDispatch::Ran)
        }
        HookRunOutcome::Deferred(script_rel) => {
            queue_deferred_hook(rootfs, pkg_name, hook, &script_rel)?;
            Ok(HookDispatch::Deferred)
        }
    }
}

enum HookRunOutcome {
    Ran(std::process::ExitStatus),
    Deferred(PathBuf),
}

#[derive(Default)]
struct ChrootMountGuard {
    mounted: Vec<Mount>,
}

impl ChrootMountGuard {
    fn mount_path(
        &mut self,
        source: &Path,
        target: &Path,
        fstype: Option<&str>,
        flags: MountFlags,
        data: Option<&str>,
    ) -> Result<()> {
        let mut builder = Mount::builder().flags(flags);
        if let Some(fs) = fstype {
            builder = builder.fstype(fs);
        }
        if let Some(options) = data {
            builder = builder.data(options);
        }
        let mount = builder
            .mount(source, target)
            .with_context(|| format!("Failed to mount {}", target.display()))?;
        self.mounted.push(mount);
        Ok(())
    }
}

impl Drop for ChrootMountGuard {
    fn drop(&mut self) {
        for mount in self.mounted.iter().rev() {
            if mount.unmount(UnmountFlags::empty()).is_ok() {
                continue;
            }
            let _ = mount.unmount(UnmountFlags::DETACH);
        }
    }
}

fn should_use_chroot(rootfs: &Path) -> bool {
    let canonical_root = fs::canonicalize("/").ok();
    let canonical_rootfs = fs::canonicalize(rootfs).ok();
    match (canonical_rootfs, canonical_root) {
        (Some(target), Some(root)) => target != root,
        _ => rootfs != Path::new("/"),
    }
}

fn should_bootstrap_host_shell(should_chroot: bool, is_root: bool, shell_exists: bool) -> bool {
    should_chroot && is_root && !shell_exists
}

fn bootstrap_hook_path_env() -> String {
    format!(
        "/{}:{}",
        BOOTSTRAP_BIN_DIR_REL,
        crate::runtime_env::safe_script_path()
    )
}

fn mount_chroot_filesystems(
    rootfs: &Path,
    bootstrap_script_path: Option<&Path>,
) -> Result<ChrootMountGuard> {
    let proc_dir = rootfs.join("proc");
    let dev_dir = rootfs.join("dev");
    let dev_pts_dir = dev_dir.join("pts");
    let sys_dir = rootfs.join("sys");

    fs::create_dir_all(&proc_dir)
        .with_context(|| format!("Failed to create {}", proc_dir.display()))?;
    fs::create_dir_all(&dev_dir)
        .with_context(|| format!("Failed to create {}", dev_dir.display()))?;
    fs::create_dir_all(&dev_pts_dir)
        .with_context(|| format!("Failed to create {}", dev_pts_dir.display()))?;
    fs::create_dir_all(&sys_dir)
        .with_context(|| format!("Failed to create {}", sys_dir.display()))?;

    let mut guard = ChrootMountGuard::default();
    guard.mount_path(
        Path::new("proc"),
        &proc_dir,
        Some("proc"),
        MountFlags::NODEV | MountFlags::NOEXEC | MountFlags::NOSUID,
        None,
    )?;
    guard.mount_path(Path::new("/dev"), &dev_dir, None, MountFlags::BIND, None)?;
    guard.mount_path(
        Path::new("sysfs"),
        &sys_dir,
        Some("sysfs"),
        MountFlags::NODEV | MountFlags::NOEXEC | MountFlags::NOSUID,
        None,
    )?;
    if let Err(_e) = guard.mount_path(
        Path::new("devpts"),
        &dev_pts_dir,
        Some("devpts"),
        MountFlags::NOSUID | MountFlags::NOEXEC,
        Some("gid=5,mode=620"),
    ) {
        guard.mount_path(
            Path::new("devpts"),
            &dev_pts_dir,
            Some("devpts"),
            MountFlags::NOSUID | MountFlags::NOEXEC,
            None,
        )?;
    }

    if let Some(script_path) = bootstrap_script_path {
        bind_host_shell_into_chroot(&mut guard, rootfs, script_path)?;
    }

    maybe_bind_host_file_into_chroot(&mut guard, rootfs, Path::new("/etc/resolv.conf"))?;
    Ok(guard)
}

fn maybe_bind_host_file_into_chroot(
    guard: &mut ChrootMountGuard,
    rootfs: &Path,
    host_path: &Path,
) -> Result<()> {
    let rel = host_path
        .strip_prefix(Path::new("/"))
        .with_context(|| format!("Expected absolute host path: {}", host_path.display()))?;
    bind_host_file_into_chroot_at(guard, rootfs, host_path, rel)
}

fn bind_host_file_into_chroot_at(
    guard: &mut ChrootMountGuard,
    rootfs: &Path,
    host_path: &Path,
    target_rel: &Path,
) -> Result<()> {
    if !host_path.exists() {
        return Ok(());
    }
    prepare_host_file_bind_target(rootfs, target_rel).map(|target| {
        if let Err(err) = guard.mount_path(host_path, &target, None, MountFlags::BIND, None) {
            crate::log_warn!(
                "Failed to bind-mount {} into chroot at {}: {}",
                host_path.display(),
                target.display(),
                err
            );
        }
    })
}

fn bind_host_file_into_chroot_at_required(
    guard: &mut ChrootMountGuard,
    rootfs: &Path,
    host_path: &Path,
    target_rel: &Path,
) -> Result<()> {
    let target = prepare_host_file_bind_target(rootfs, target_rel)?;
    guard
        .mount_path(host_path, &target, None, MountFlags::BIND, None)
        .with_context(|| {
            format!(
                "Failed to bind-mount {} into chroot at {}",
                host_path.display(),
                target.display()
            )
        })?;
    Ok(())
}

fn prepare_host_file_bind_target(rootfs: &Path, target_rel: &Path) -> Result<PathBuf> {
    let target = rootfs.join(target_rel);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if !target.exists() {
        fs::File::create(&target)
            .with_context(|| format!("Failed to create {}", target.display()))?;
    }

    Ok(target)
}

fn bind_host_shell_into_chroot(
    guard: &mut ChrootMountGuard,
    rootfs: &Path,
    script_path: &Path,
) -> Result<()> {
    let shell_path =
        fs::canonicalize("/bin/sh").context("Failed to resolve host /bin/sh for hook bootstrap")?;
    bind_host_file_into_chroot_at_required(guard, rootfs, &shell_path, Path::new("bin/sh"))?;

    let mut dependency_paths = BTreeSet::new();
    for dependency in collect_host_binary_dependencies(&shell_path)? {
        dependency_paths.insert(dependency);
    }

    for tool in collect_bootstrap_tool_bindings(script_path)? {
        bind_host_file_into_chroot_at_required(guard, rootfs, &tool.host_path, &tool.target_rel)?;
        for dependency in collect_host_binary_dependencies(&tool.host_path)? {
            dependency_paths.insert(dependency);
        }
    }

    for dependency in dependency_paths {
        let target_rel = dependency
            .strip_prefix(Path::new("/"))
            .with_context(|| format!("Expected absolute host path: {}", dependency.display()))?;
        bind_host_file_into_chroot_at_required(guard, rootfs, &dependency, target_rel)?;
    }

    Ok(())
}

fn collect_host_binary_dependencies(binary_path: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("ldd")
        .arg(binary_path)
        .output()
        .with_context(|| {
            format!(
                "Failed to inspect shared-library dependencies for {}",
                binary_path.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ldd failed for {}: {}",
            binary_path.display(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8(output.stdout).with_context(|| {
        format!(
            "ldd returned non-UTF-8 output for {}",
            binary_path.display()
        )
    })?;
    parse_ldd_dependency_paths(&stdout)
}

fn parse_ldd_dependency_paths(output: &str) -> Result<Vec<PathBuf>> {
    let mut deps = Vec::new();

    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line == "statically linked" {
            continue;
        }

        let candidate = if let Some((_, rest)) = line.split_once("=>") {
            let token = rest.split_whitespace().next().unwrap_or_default();
            if token == "not" {
                bail!(
                    "Missing shared-library dependency reported by ldd: {}",
                    line
                );
            }
            token
        } else {
            line.split_whitespace().next().unwrap_or_default()
        };

        if candidate.starts_with('/') {
            let path = PathBuf::from(candidate);
            if !deps.iter().any(|existing| existing == &path) {
                deps.push(path);
            }
        }
    }

    Ok(deps)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BootstrapToolBinding {
    host_path: PathBuf,
    target_rel: PathBuf,
}

fn collect_bootstrap_tool_bindings(script_path: &Path) -> Result<Vec<BootstrapToolBinding>> {
    let script = fs::read_to_string(script_path).with_context(|| {
        format!(
            "Failed to read lifecycle hook for bootstrap tool detection: {}",
            script_path.display()
        )
    })?;

    let mut bindings = BTreeSet::new();
    for command in parse_hook_command_candidates(&script) {
        if is_shell_builtin(&command) {
            continue;
        }

        if command.starts_with('/') {
            let host_path = fs::canonicalize(&command).with_context(|| {
                format!(
                    "Failed to resolve hook command path for bootstrap: {}",
                    command
                )
            })?;
            let target_rel = Path::new(&command)
                .strip_prefix(Path::new("/"))
                .with_context(|| format!("Expected absolute hook command path: {}", command))?
                .to_path_buf();
            bindings.insert(BootstrapToolBinding {
                host_path,
                target_rel,
            });
            continue;
        }

        if let Some(host_path) = resolve_host_tool_path(&command)? {
            bindings.insert(BootstrapToolBinding {
                host_path,
                target_rel: Path::new(BOOTSTRAP_BIN_DIR_REL).join(&command),
            });
        }
    }

    Ok(bindings.into_iter().collect())
}

fn parse_hook_command_candidates(script: &str) -> Vec<String> {
    let mut commands = BTreeSet::new();

    for line in script.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let tokens = tokenize_hook_script_line(trimmed);
        let mut expect_command = true;
        for token in tokens {
            if token.is_empty() {
                continue;
            }

            if matches!(token.as_str(), "&&" | "||" | ";" | "|") {
                expect_command = true;
                continue;
            }

            if !expect_command {
                continue;
            }

            if looks_like_env_assignment(&token) {
                continue;
            }

            if is_shell_reserved_word(&token) {
                continue;
            }

            if is_shell_builtin(&token) {
                expect_command = false;
                continue;
            }

            commands.insert(token);
            expect_command = false;
        }
    }

    commands.into_iter().collect()
}

fn tokenize_hook_script_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(ch) = chars.next() {
        if let Some(active_quote) = quote {
            current.push(ch);
            if ch == active_quote {
                quote = None;
            } else if ch == '\\'
                && active_quote == '"'
                && let Some(next) = chars.next()
            {
                current.push(next);
            }
            continue;
        }

        match ch {
            '\'' | '"' => {
                current.push(ch);
                quote = Some(ch);
            }
            '\\' => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '#' => break,
            '&' | '|' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                if chars.peek() == Some(&ch) {
                    let _ = chars.next();
                    tokens.push(format!("{ch}{ch}"));
                } else {
                    tokens.push(ch.to_string());
                }
            }
            ';' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(ch.to_string());
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn is_shell_reserved_word(token: &str) -> bool {
    matches!(
        token,
        "!" | "{"
            | "}"
            | "("
            | ")"
            | "then"
            | "do"
            | "done"
            | "else"
            | "elif"
            | "fi"
            | "if"
            | "case"
            | "esac"
            | "for"
            | "in"
            | "while"
            | "until"
            | "function"
            | "select"
    )
}

fn looks_like_env_assignment(token: &str) -> bool {
    let Some((key, _value)) = token.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && !key.chars().next().is_some_and(|ch| ch.is_ascii_digit())
}

fn is_shell_builtin(token: &str) -> bool {
    matches!(
        token,
        "." | ":"
            | "["
            | "alias"
            | "bg"
            | "break"
            | "cd"
            | "command"
            | "continue"
            | "echo"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "false"
            | "fg"
            | "getopts"
            | "hash"
            | "jobs"
            | "printf"
            | "pwd"
            | "read"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "test"
            | "times"
            | "trap"
            | "true"
            | "type"
            | "ulimit"
            | "umask"
            | "unalias"
            | "unset"
            | "wait"
    )
}

fn resolve_host_tool_path(command: &str) -> Result<Option<PathBuf>> {
    for dir in crate::runtime_env::safe_script_path().split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(command);
        if !candidate.is_file() {
            continue;
        }
        let metadata = candidate.metadata().with_context(|| {
            format!(
                "Failed to inspect host tool candidate {}",
                candidate.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(Some(fs::canonicalize(&candidate).with_context(|| {
            format!("Failed to resolve host tool path {}", candidate.display())
        })?));
    }

    Ok(None)
}

fn run_script_with_rootfs_context(
    script_path: &Path,
    rootfs: &Path,
    pkg_name: &str,
    hook: Hook,
    allow_defer: bool,
) -> Result<HookRunOutcome> {
    let should_chroot = should_use_chroot(rootfs);
    let is_root = fakeroot::is_root();
    let shell_exists = rootfs.join("bin/sh").exists();
    let bootstrap_host_shell = should_bootstrap_host_shell(should_chroot, is_root, shell_exists);

    if should_chroot && is_root {
        if bootstrap_host_shell {
            crate::log_info!(
                "Temporarily binding host /bin/sh into {} for lifecycle hook {}",
                rootfs.display(),
                hook.canonical_name()
            );
        }

        let run_result = if let Ok(rel) = script_path.strip_prefix(rootfs) {
            run_hook_script_in_chroot(
                rootfs,
                rel,
                pkg_name,
                hook,
                false,
                bootstrap_host_shell.then_some(script_path),
            )
        } else {
            run_hook_script_contents_in_chroot(
                rootfs,
                script_path,
                pkg_name,
                hook,
                false,
                bootstrap_host_shell.then_some(script_path),
            )
        };

        match run_result {
            Ok(status) => return Ok(HookRunOutcome::Ran(status)),
            Err(err) if allow_defer && bootstrap_host_shell => {
                let rel = script_path.strip_prefix(rootfs).with_context(|| {
                    format!(
                        "Cannot defer lifecycle hook {} for {} because {} is outside {}",
                        hook.canonical_name(),
                        pkg_name,
                        script_path.display(),
                        rootfs.display()
                    )
                })?;
                crate::log_warn!(
                    "Deferring lifecycle hook {} for {} because host shell bootstrap failed: {}",
                    hook.canonical_name(),
                    pkg_name,
                    err
                );
                return Ok(HookRunOutcome::Deferred(rel.to_path_buf()));
            }
            Err(err) => return Err(err),
        }
    }

    // Live-root installs can execute directly with the host shell.
    let script_arg = if let Ok(rel) = script_path.strip_prefix(rootfs) {
        PathBuf::from(format!("./{}", rel.to_string_lossy()))
    } else {
        script_path.to_path_buf()
    };
    let rootfs_env = if rootfs.is_absolute() {
        rootfs.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Failed to resolve current working directory for DEPOT_ROOTFS")?
            .join(rootfs)
    };

    let status = Command::new("/bin/sh")
        .arg(script_arg)
        .current_dir(rootfs)
        .env("DEPOT_PACKAGE", pkg_name)
        .env("DEPOT_ROOTFS", &rootfs_env)
        .env("DEPOT_ACTION", hook.action())
        .env("DEPOT_PHASE", hook.phase())
        .env("PATH", crate::runtime_env::safe_script_path())
        .status()
        .with_context(|| {
            format!(
                "Failed to execute lifecycle hook {} at {}",
                hook.canonical_name(),
                script_path.display()
            )
        })?;
    Ok(HookRunOutcome::Ran(status))
}

fn run_hook_script_in_chroot(
    rootfs: &Path,
    rel_script: &Path,
    pkg_name: &str,
    hook: Hook,
    quiet: bool,
    bootstrap_script_path: Option<&Path>,
) -> Result<std::process::ExitStatus> {
    let _mounts = mount_chroot_filesystems(rootfs, bootstrap_script_path)?;
    let rel_script = format!("./{}", rel_script.to_string_lossy());
    let path_env = if bootstrap_script_path.is_some() {
        bootstrap_hook_path_env()
    } else {
        crate::runtime_env::safe_script_path().to_string()
    };
    let mut cmd = Command::new("chroot");
    cmd.arg(rootfs)
        .arg("/bin/sh")
        .arg("-c")
        .arg("export DEPOT_PACKAGE DEPOT_ROOTFS DEPOT_ACTION DEPOT_PHASE; cd / && exec /bin/sh \"$1\"")
        .arg("sh")
        .arg(rel_script)
        .env("DEPOT_PACKAGE", pkg_name)
        .env("DEPOT_ROOTFS", "/")
        .env("DEPOT_ACTION", hook.action())
        .env("DEPOT_PHASE", hook.phase())
        .env("PATH", &path_env);
    if quiet {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    cmd.status().with_context(|| {
        format!(
            "Failed to execute lifecycle hook {} in chroot at {}",
            hook.canonical_name(),
            rootfs.display()
        )
    })
}

fn run_hook_script_contents_in_chroot(
    rootfs: &Path,
    script_path: &Path,
    pkg_name: &str,
    hook: Hook,
    quiet: bool,
    bootstrap_script_path: Option<&Path>,
) -> Result<std::process::ExitStatus> {
    let _mounts = mount_chroot_filesystems(rootfs, bootstrap_script_path)?;
    let path_env = if bootstrap_script_path.is_some() {
        bootstrap_hook_path_env()
    } else {
        crate::runtime_env::safe_script_path().to_string()
    };
    let mut cmd = Command::new("chroot");
    cmd.arg(rootfs)
        .arg("/bin/sh")
        .arg("-s")
        .env("DEPOT_PACKAGE", pkg_name)
        .env("DEPOT_ROOTFS", "/")
        .env("DEPOT_ACTION", hook.action())
        .env("DEPOT_PHASE", hook.phase())
        .env("PATH", &path_env)
        .stdin(Stdio::piped());
    if quiet {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "Failed to execute lifecycle hook {} in chroot at {}",
            hook.canonical_name(),
            rootfs.display()
        )
    })?;
    let script_bytes = fs::read(script_path).with_context(|| {
        format!(
            "Failed to read lifecycle hook {} at {}",
            hook.canonical_name(),
            script_path.display()
        )
    })?;
    let mut stdin = child.stdin.take().with_context(|| {
        format!(
            "Failed to open stdin for lifecycle hook {} in chroot at {}",
            hook.canonical_name(),
            rootfs.display()
        )
    })?;
    stdin.write_all(&script_bytes).with_context(|| {
        format!(
            "Failed to stream lifecycle hook {} into chroot at {}",
            hook.canonical_name(),
            rootfs.display()
        )
    })?;
    drop(stdin);
    child.wait().with_context(|| {
        format!(
            "Failed to wait for lifecycle hook {} in chroot at {}",
            hook.canonical_name(),
            rootfs.display()
        )
    })
}

fn queue_deferred_hook(rootfs: &Path, pkg_name: &str, hook: Hook, script_rel: &Path) -> Result<()> {
    let path = deferred_hooks_file(rootfs);
    let mut hooks = read_deferred_hooks(&path)?;
    let item = DeferredHook {
        pkg_name: pkg_name.to_string(),
        hook,
        script_rel: script_rel.to_path_buf(),
    };
    if !hooks.iter().any(|existing| {
        existing.pkg_name == item.pkg_name
            && existing.hook == item.hook
            && existing.script_rel == item.script_rel
    }) {
        hooks.push(item);
        write_deferred_hooks(&path, &hooks)?;
    }
    Ok(())
}

fn deferred_hooks_file(rootfs: &Path) -> PathBuf {
    rootfs.join(DEFERRED_HOOKS_FILE_REL)
}

fn read_deferred_hooks(path: &Path) -> Result<Vec<DeferredHook>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut hooks = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("Failed to read deferred hook line {}", idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let pkg_name = parts
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("Malformed deferred hook line {} (package)", idx + 1))?;
        let hook_name = parts
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("Malformed deferred hook line {} (hook)", idx + 1))?;
        let script_rel = parts
            .next()
            .filter(|s| !s.is_empty())
            .with_context(|| format!("Malformed deferred hook line {} (script)", idx + 1))?;
        let hook = Hook::from_canonical_name(hook_name).with_context(|| {
            format!("Unknown deferred hook '{}' on line {}", hook_name, idx + 1)
        })?;

        hooks.push(DeferredHook {
            pkg_name: pkg_name.to_string(),
            hook,
            script_rel: PathBuf::from(script_rel),
        });
    }

    Ok(hooks)
}

fn write_deferred_hooks(path: &Path, hooks: &[DeferredHook]) -> Result<()> {
    if hooks.is_empty() {
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("Failed to remove {}", path.display()))?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let mut f =
        fs::File::create(path).with_context(|| format!("Failed to create {}", path.display()))?;
    for item in hooks {
        writeln!(
            f,
            "{}\t{}\t{}",
            item.pkg_name,
            item.hook.canonical_name(),
            item.script_rel.display()
        )
        .with_context(|| format!("Failed to write {}", path.display()))?;
    }
    f.flush()
        .with_context(|| format!("Failed to flush {}", path.display()))?;
    Ok(())
}

/// Attempt to run deferred lifecycle hooks for this rootfs once.
///
/// Deferred hooks are best-effort: failures are kept in queue for later retry.
pub fn run_deferred_hooks_if_possible(rootfs: &Path) -> Result<()> {
    if !fakeroot::is_root() || !should_use_chroot(rootfs) || !rootfs.join("bin/sh").exists() {
        return Ok(());
    }

    let path = deferred_hooks_file(rootfs);
    let hooks = read_deferred_hooks(&path)?;
    if hooks.is_empty() {
        return Ok(());
    }

    let mut remaining = Vec::new();
    for item in hooks {
        let script_path = rootfs.join(&item.script_rel);
        if !script_path.exists() {
            crate::log_info!(
                "Dropping deferred lifecycle hook {} for {} because script is missing: {}",
                item.hook.canonical_name(),
                item.pkg_name,
                script_path.display()
            );
            continue;
        }

        match run_hook_script_in_chroot(
            rootfs,
            &item.script_rel,
            &item.pkg_name,
            item.hook,
            true,
            None,
        ) {
            Ok(status) if status.success() => {}
            Ok(status) => {
                crate::log_info!(
                    "Deferred lifecycle hook {} for {} failed with status {} (kept queued)",
                    item.hook.canonical_name(),
                    item.pkg_name,
                    status
                );
                remaining.push(item);
            }
            Err(err) => {
                crate::log_info!(
                    "Deferred lifecycle hook {} for {} failed with error (kept queued): {}",
                    item.hook.canonical_name(),
                    item.pkg_name,
                    err
                );
                remaining.push(item);
            }
        }
    }

    write_deferred_hooks(&path, &remaining)?;
    if !remaining.is_empty() {
        crate::log_info!(
            "{} deferred lifecycle hook(s) remain queued in {}",
            remaining.len(),
            path.display()
        );
    }
    Ok(())
}

fn resolve_hook_script(script_dir: &Path, hook: Hook, pkg_name: &str) -> Result<Option<PathBuf>> {
    if !script_dir.exists() {
        return Ok(None);
    }

    if !script_dir.is_dir() {
        bail!(
            "Scripts path exists but is not a directory: {}",
            script_dir.display()
        );
    }

    if is_lib32_package(pkg_name) {
        let label = format!("{} (lib32)", hook.canonical_name());
        return resolve_hook_from_candidates(script_dir, &label, hook.lib32_candidate_names());
    }

    resolve_hook_from_candidates(script_dir, hook.canonical_name(), hook.candidate_names())
}

fn resolve_hook_from_candidates<I>(
    script_dir: &Path,
    hook_label: &str,
    candidates: I,
) -> Result<Option<PathBuf>>
where
    I: IntoIterator<Item = String>,
{
    let mut found = Vec::new();

    for candidate in candidates {
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
            hook_label,
            names
        );
    }

    Ok(found.into_iter().next())
}

fn is_lib32_package(pkg_name: &str) -> bool {
    pkg_name.starts_with("lib32-")
}

fn collect_legacy_root_hook_candidates<I>(
    spec_dir: &Path,
    hook_label: &str,
    candidates: I,
) -> Result<Option<PathBuf>>
where
    I: IntoIterator<Item = String>,
{
    let mut matches = Vec::new();
    for candidate in candidates {
        let path = spec_dir.join(&candidate);
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

        matches.push(path);
    }

    if matches.len() > 1 {
        let names = matches
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "Ambiguous legacy lifecycle hook '{}': multiple script candidates found: {}",
            hook_label,
            names
        );
    }

    Ok(matches.into_iter().next())
}

fn collect_legacy_root_hooks(spec_dir: &Path, pkg_name: &str) -> Result<Vec<(Hook, PathBuf)>> {
    let mut found = Vec::new();

    for hook in ALL_HOOKS {
        let matched_path = if is_lib32_package(pkg_name) {
            let lib32_label = format!("{} (lib32)", hook.canonical_name());
            collect_legacy_root_hook_candidates(
                spec_dir,
                &lib32_label,
                hook.legacy_root_lib32_candidate_names(),
            )?
        } else {
            collect_legacy_root_hook_candidates(
                spec_dir,
                hook.canonical_name(),
                hook.legacy_root_candidate_names(),
            )?
        };

        if let Some(path) = matched_path {
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
            source: vec![Source {
                url: "https://example.com/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
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
    fn run_hook_if_present_uses_safe_script_path() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = tmp.path().join("scripts");
        let rootfs = tmp.path().join("root");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();

        std::fs::write(
            scripts.join("pre_install"),
            "echo \"$PATH\" > \"$DEPOT_ROOTFS/path.out\"\n",
        )
        .unwrap();

        let ran = run_hook_if_present(&scripts, Hook::PreInstall, &rootfs, "foo").unwrap();
        assert!(ran);
        assert_eq!(
            std::fs::read_to_string(rootfs.join("path.out"))
                .unwrap()
                .trim_end(),
            crate::runtime_env::safe_script_path()
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
    fn run_hook_if_present_prefers_lib32_specific_script_name() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = tmp.path().join("scripts");
        let rootfs = tmp.path().join("root");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();

        std::fs::write(
            scripts.join("post_install"),
            "echo generic > \"$DEPOT_ROOTFS/hook.out\"\n",
        )
        .unwrap();
        std::fs::write(
            scripts.join("post_install-lib32"),
            "echo lib32 > \"$DEPOT_ROOTFS/hook.out\"\n",
        )
        .unwrap();

        let ran = run_hook_if_present(&scripts, Hook::PostInstall, &rootfs, "lib32-foo").unwrap();
        assert!(ran);
        assert_eq!(
            std::fs::read_to_string(rootfs.join("hook.out")).unwrap(),
            "lib32\n"
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
    fn run_hook_if_present_with_relative_rootfs_uses_correct_script_and_env_paths() {
        let cwd = std::env::current_dir().unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("depot-hook-rel-rootfs-")
            .tempdir_in(&cwd)
            .unwrap();
        let rootfs_abs = tmp.path().join("root");
        std::fs::create_dir_all(&rootfs_abs).unwrap();
        let rootfs_rel = rootfs_abs.strip_prefix(&cwd).unwrap().to_path_buf();
        let scripts = rootfs_rel.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();

        std::fs::write(
            scripts.join("pre_install"),
            "echo ok > \"$DEPOT_ROOTFS/hook.out\"\n",
        )
        .unwrap();

        let ran = run_hook_if_present(&scripts, Hook::PreInstall, &rootfs_rel, "foo").unwrap();
        assert!(ran);
        assert_eq!(
            std::fs::read_to_string(rootfs_abs.join("hook.out")).unwrap(),
            "ok\n"
        );
    }

    #[test]
    fn should_bootstrap_host_shell_only_for_chrooted_root_installs_without_shell() {
        assert!(should_bootstrap_host_shell(true, true, false));
        assert!(!should_bootstrap_host_shell(true, true, true));
        assert!(!should_bootstrap_host_shell(true, false, false));
        assert!(!should_bootstrap_host_shell(false, true, false));
    }

    #[test]
    fn parse_ldd_dependency_paths_extracts_absolute_paths() {
        let parsed = parse_ldd_dependency_paths(
            "linux-vdso.so.1 (0x0000)\nlibc.so.6 => /lib/libc.so.6 (0x0000)\n/lib64/ld-linux-x86-64.so.2 (0x0000)\nlibc.so.6 => /lib/libc.so.6 (0x0001)\n",
        )
        .unwrap();
        assert_eq!(
            parsed,
            vec![
                PathBuf::from("/lib/libc.so.6"),
                PathBuf::from("/lib64/ld-linux-x86-64.so.2")
            ]
        );
    }

    #[test]
    fn parse_ldd_dependency_paths_rejects_missing_dependencies() {
        let err = parse_ldd_dependency_paths("libedit.so.0 => not found\n")
            .expect_err("expected ldd parse to fail when a dependency is missing");
        assert!(
            err.to_string()
                .contains("Missing shared-library dependency")
        );
    }

    #[test]
    fn parse_hook_command_candidates_finds_commands_after_assignments_and_operators() {
        let commands = parse_hook_command_candidates(
            "PATH=/tmp:$PATH grep -q foo etc/shells || echo foo >> etc/shells\ncat \"$DEPOT_ROOTFS/usr/bin/find\" | sed 's/x/y/'\n",
        );
        assert_eq!(
            commands,
            vec!["cat".to_string(), "grep".to_string(), "sed".to_string()]
        );
    }

    #[test]
    fn parse_hook_command_candidates_ignores_builtins_and_control_words() {
        let commands = parse_hook_command_candidates("if true; then export FOO=bar; echo hi; fi\n");
        assert!(commands.is_empty());
    }

    #[test]
    fn deferred_hooks_file_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hooks.tsv");
        let hooks = vec![
            DeferredHook {
                pkg_name: "foo".into(),
                hook: Hook::PostInstall,
                script_rel: PathBuf::from("usr/share/depot/foo/scripts/post_install"),
            },
            DeferredHook {
                pkg_name: "bar".into(),
                hook: Hook::PostUpdate,
                script_rel: PathBuf::from("usr/share/depot/bar/scripts/post_update"),
            },
        ];
        write_deferred_hooks(&path, &hooks).unwrap();
        let loaded = read_deferred_hooks(&path).unwrap();
        assert_eq!(loaded.len(), hooks.len());
        assert_eq!(loaded[0].pkg_name, hooks[0].pkg_name);
        assert_eq!(loaded[0].hook, hooks[0].hook);
        assert_eq!(loaded[0].script_rel, hooks[0].script_rel);
        assert_eq!(loaded[1].pkg_name, hooks[1].pkg_name);
        assert_eq!(loaded[1].hook, hooks[1].hook);
        assert_eq!(loaded[1].script_rel, hooks[1].script_rel);
    }

    #[test]
    fn queue_deferred_hook_dedupes_entries() {
        let tmp = tempfile::tempdir().unwrap();
        queue_deferred_hook(
            tmp.path(),
            "foo",
            Hook::PostInstall,
            Path::new("usr/share/depot/foo/scripts/post_install"),
        )
        .unwrap();
        queue_deferred_hook(
            tmp.path(),
            "foo",
            Hook::PostInstall,
            Path::new("usr/share/depot/foo/scripts/post_install"),
        )
        .unwrap();

        let loaded = read_deferred_hooks(&deferred_hooks_file(tmp.path())).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].pkg_name, "foo");
        assert_eq!(loaded[0].hook, Hook::PostInstall);
        assert_eq!(
            loaded[0].script_rel,
            PathBuf::from("usr/share/depot/foo/scripts/post_install")
        );
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

    #[test]
    fn stage_scripts_from_spec_dir_stages_lib32_prefixed_legacy_root_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();

        std::fs::write(spec_dir.join("lib32-postinstall.sh"), "echo lib32-post").unwrap();

        let mut spec = mk_spec(&spec_dir);
        spec.package.name = "lib32-foo".into();
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

    #[test]
    fn stage_scripts_from_spec_dir_lib32_ignores_generic_legacy_root_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::create_dir_all(&destdir).unwrap();

        // No lib32-prefixed hook; native-only scripts must NOT be staged for lib32 packages.
        std::fs::write(spec_dir.join("postinstall.sh"), "echo fallback").unwrap();

        let mut spec = mk_spec(&spec_dir);
        spec.package.name = "lib32-foo".into();
        let staged = stage_scripts_from_spec_dir(&spec, &destdir).unwrap();
        assert!(!staged);
        assert!(!destdir.join("scripts/post_install").exists());
    }

    #[test]
    fn run_hook_if_present_lib32_ignores_generic_script() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = tmp.path().join("scripts");
        let rootfs = tmp.path().join("root");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();

        // Only a generic script exists; lib32 package must NOT execute it.
        std::fs::write(
            scripts.join("post_install"),
            "echo generic > \"$DEPOT_ROOTFS/hook.out\"\n",
        )
        .unwrap();

        let ran = run_hook_if_present(&scripts, Hook::PostInstall, &rootfs, "lib32-foo").unwrap();
        assert!(!ran);
        assert!(!rootfs.join("hook.out").exists());
    }
}
