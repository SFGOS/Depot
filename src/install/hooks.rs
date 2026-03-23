//! Transaction hook loading and execution.
//!
//! Hook files live under `<rootfs>/usr/share/depot.d/hooks/*.toml` and follow a
//! Starpack-inspired format:
//!
//! ```toml
//! [hook]
//! name = "refresh cache"
//! description = "Refreshes cache after updates"
//!
//! [when]
//! phase = "post"
//! operation = ["install", "update"]
//! packages = ["glibc", "linux*"]
//! paths = ["usr/lib/*"]
//! negation = ["usr/lib/debug/*"]
//!
//! [exec]
//! command = "ldconfig"
//! needs_paths = true
//! ```
//!
//! The legacy section/key names `[Hook]`, `[When]`, and `[Exec]` are accepted.

use crate::fakeroot;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const TRANSACTION_HOOKS_DIR_REL: &str = "usr/share/depot.d/hooks";

/// Hook phase within an install/update/remove transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPhase {
    /// Before files are changed.
    Pre,
    /// After files are changed.
    Post,
}

impl HookPhase {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pre" | "pretransaction" | "pre_transaction" | "pre-transaction" => {
                Some(HookPhase::Pre)
            }
            "post" | "posttransaction" | "post_transaction" | "post-transaction" => {
                Some(HookPhase::Post)
            }
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            HookPhase::Pre => "pre",
            HookPhase::Post => "post",
        }
    }
}

/// Transaction operation for hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOperation {
    /// Package install.
    Install,
    /// Package update/upgrade.
    Update,
    /// Package removal.
    Remove,
}

impl HookOperation {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "install" => Some(HookOperation::Install),
            "update" | "upgrade" => Some(HookOperation::Update),
            "remove" | "uninstall" => Some(HookOperation::Remove),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            HookOperation::Install => "install",
            HookOperation::Update => "update",
            HookOperation::Remove => "remove",
        }
    }
}

/// Runtime context for transaction hook matching/execution.
#[derive(Debug, Clone)]
pub struct HookExecutionContext<'a> {
    /// Current phase (`pre` or `post`).
    pub phase: HookPhase,
    /// Current operation (`install`, `update`, `remove`).
    pub operation: HookOperation,
    /// Package being processed.
    pub package: &'a str,
    /// Filesystem paths affected by this package action.
    pub affected_paths: &'a [String],
}

/// Owned transaction hook context used for batched execution.
#[derive(Debug, Clone)]
pub struct HookExecutionContextOwned {
    /// Current operation (`install`, `update`, `remove`).
    pub operation: HookOperation,
    /// Package being processed.
    pub package: String,
    /// Filesystem paths affected by this package action.
    pub affected_paths: Vec<String>,
}

#[derive(Debug, Clone)]
struct TransactionHook {
    file_path: PathBuf,
    name: Option<String>,
    phase: HookPhase,
    operations: Vec<HookOperation>,
    packages: Vec<String>,
    paths: Vec<String>,
    negations: Vec<String>,
    command: String,
    needs_paths: bool,
}

impl TransactionHook {
    fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unnamed-hook>")
                .to_string()
        })
    }

    fn matches(&self, ctx: &HookExecutionContext<'_>, normalized_paths: &[String]) -> bool {
        if self.phase != ctx.phase {
            return false;
        }

        if !self.operations.is_empty() && !self.operations.contains(&ctx.operation) {
            return false;
        }

        if !self.packages.is_empty() {
            let package = ctx.package.trim();
            let package_match = self
                .packages
                .iter()
                .any(|pattern| wildcard_match(pattern.trim(), package));
            if !package_match {
                return false;
            }
        }

        if !self.paths.is_empty() {
            let positive_match = normalized_paths.iter().any(|path| {
                self.paths.iter().any(|pattern| {
                    wildcard_match(
                        &normalize_match_target(pattern),
                        &normalize_match_target(path),
                    )
                })
            });
            if !positive_match {
                return false;
            }
        }

        if !self.negations.is_empty() {
            let negated = normalized_paths.iter().any(|path| {
                self.negations.iter().any(|pattern| {
                    wildcard_match(
                        &normalize_match_target(pattern),
                        &normalize_match_target(path),
                    )
                })
            });
            if negated {
                return false;
            }
        }

        true
    }
}

/// Return the transaction hook directory for a rootfs.
pub fn transaction_hooks_dir(rootfs: &Path) -> PathBuf {
    rootfs.join(TRANSACTION_HOOKS_DIR_REL)
}

/// Load and execute transaction hooks that match `ctx`.
///
/// Returns the number of hooks that were executed.
pub fn run_transaction_hooks(rootfs: &Path, ctx: &HookExecutionContext<'_>) -> Result<usize> {
    let hook_dir = transaction_hooks_dir(rootfs);
    let hook_files = discover_hook_files(&hook_dir)?;
    if hook_files.is_empty() {
        return Ok(0);
    }

    let normalized_paths = normalize_affected_paths(ctx.affected_paths);

    let mut executed = 0usize;
    for hook_file in hook_files {
        let hook = parse_hook_file(&hook_file)?;
        if !hook.matches(ctx, &normalized_paths) {
            continue;
        }

        run_hook_command(rootfs, &hook, ctx, &normalized_paths, None)?;
        executed += 1;
    }

    Ok(executed)
}

/// Load and execute hooks for a batch of transaction contexts in stable order.
///
/// Hooks are discovered and parsed once, then matched against each context in
/// input order. Matching hooks run in hook-file order for each context.
///
/// Returns the number of hook commands executed.
pub fn run_transaction_hooks_batch(
    rootfs: &Path,
    phase: HookPhase,
    contexts: &[HookExecutionContextOwned],
) -> Result<usize> {
    if contexts.is_empty() {
        return Ok(0);
    }

    let hook_dir = transaction_hooks_dir(rootfs);
    let hook_files = discover_hook_files(&hook_dir)?;
    if hook_files.is_empty() {
        return Ok(0);
    }

    let hooks = hook_files
        .iter()
        .map(|hook_file| parse_hook_file(hook_file))
        .collect::<Result<Vec<_>>>()?;

    #[derive(Clone, Copy)]
    struct ScheduledHookRun {
        hook_idx: usize,
        ctx_idx: usize,
    }

    let mut scheduled = Vec::new();
    for (ctx_idx, ctx_owned) in contexts.iter().enumerate() {
        let normalized_paths = normalize_affected_paths(&ctx_owned.affected_paths);
        let ctx = HookExecutionContext {
            phase,
            operation: ctx_owned.operation,
            package: &ctx_owned.package,
            affected_paths: &ctx_owned.affected_paths,
        };
        for (hook_idx, hook) in hooks.iter().enumerate() {
            if hook.matches(&ctx, &normalized_paths) {
                scheduled.push(ScheduledHookRun { hook_idx, ctx_idx });
            }
        }
    }

    let total = scheduled.len();
    for (run_idx, run) in scheduled.into_iter().enumerate() {
        let ctx_owned = &contexts[run.ctx_idx];
        let normalized_paths = normalize_affected_paths(&ctx_owned.affected_paths);
        let ctx = HookExecutionContext {
            phase,
            operation: ctx_owned.operation,
            package: &ctx_owned.package,
            affected_paths: &ctx_owned.affected_paths,
        };
        run_hook_command(
            rootfs,
            &hooks[run.hook_idx],
            &ctx,
            &normalized_paths,
            Some((run_idx + 1, total)),
        )?;
    }

    Ok(total)
}

fn discover_hook_files(hook_dir: &Path) -> Result<Vec<PathBuf>> {
    if !hook_dir.exists() {
        return Ok(Vec::new());
    }
    if !hook_dir.is_dir() {
        bail!(
            "Transaction hooks path exists but is not a directory: {}",
            hook_dir.display()
        );
    }

    let mut files = Vec::new();
    for entry in
        fs::read_dir(hook_dir).with_context(|| format!("Failed to read {}", hook_dir.display()))?
    {
        let path = entry
            .with_context(|| format!("Failed to list {}", hook_dir.display()))?
            .path();
        if !path.is_file() {
            continue;
        }
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("toml"))
            .unwrap_or(false);
        if is_toml {
            files.push(path);
        }
    }

    files.sort();
    Ok(files)
}

fn parse_hook_file(path: &Path) -> Result<TransactionHook> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let root: toml::Value =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;

    let root_table = root
        .as_table()
        .with_context(|| format!("Hook file is not a TOML table: {}", path.display()))?;
    let hook_table = select_table(root_table, &["hook", "Hook"]);
    let when_table = select_table(root_table, &["when", "When"])
        .with_context(|| format!("Missing [when] table in {}", path.display()))?;
    let exec_table = select_table(root_table, &["exec", "Exec"])
        .with_context(|| format!("Missing [exec] table in {}", path.display()))?;

    let phase = get_required_string(when_table, &["phase", "Phase"])
        .with_context(|| format!("Missing when.phase in {}", path.display()))
        .and_then(|raw| {
            HookPhase::parse(&raw)
                .with_context(|| format!("Invalid when.phase '{}' in {}", raw, path.display()))
        })?;

    let operations = get_string_list(when_table, &["operation", "operations", "Operation"])
        .with_context(|| format!("Invalid when.operation in {}", path.display()))?
        .into_iter()
        .map(|raw| {
            HookOperation::parse(&raw)
                .with_context(|| format!("Invalid operation '{}' in {}", raw, path.display()))
        })
        .collect::<Result<Vec<_>>>()?;

    let packages = get_string_list(when_table, &["package", "packages", "Package", "Packages"])
        .with_context(|| format!("Invalid when.packages in {}", path.display()))?;
    let paths = get_string_list(when_table, &["path", "paths", "Paths"])
        .with_context(|| format!("Invalid when.paths in {}", path.display()))?;
    let negations = get_string_list(
        when_table,
        &[
            "negation",
            "negations",
            "Negation",
            "exclude_paths",
            "not_paths",
        ],
    )
    .with_context(|| format!("Invalid when.negation in {}", path.display()))?;

    let command = get_required_string(exec_table, &["command", "Command"])
        .with_context(|| format!("Missing exec.command in {}", path.display()))?;
    let needs_paths = get_optional_bool(exec_table, &["needs_paths", "NeedsPaths"])
        .with_context(|| format!("Invalid exec.needs_paths in {}", path.display()))?
        .unwrap_or(false);

    Ok(TransactionHook {
        file_path: path.to_path_buf(),
        name: hook_table.and_then(|t| get_optional_string(t, &["name", "Name"])),
        phase,
        operations,
        packages,
        paths,
        negations,
        command,
        needs_paths,
    })
}

fn select_table<'a>(
    root: &'a toml::map::Map<String, toml::Value>,
    keys: &[&str],
) -> Option<&'a toml::map::Map<String, toml::Value>> {
    keys.iter()
        .find_map(|key| root.get(*key).and_then(toml::Value::as_table))
}

fn get_optional_string(
    table: &toml::map::Map<String, toml::Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        table
            .get(*key)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    })
}

fn get_required_string(
    table: &toml::map::Map<String, toml::Value>,
    keys: &[&str],
) -> Result<String> {
    get_optional_string(table, keys)
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("Missing required key '{}'", keys[0]))
}

fn get_string_list(
    table: &toml::map::Map<String, toml::Value>,
    keys: &[&str],
) -> Result<Vec<String>> {
    for key in keys {
        let Some(value) = table.get(*key) else {
            continue;
        };
        return match value {
            toml::Value::String(s) => Ok(vec![s.to_string()]),
            toml::Value::Array(arr) => {
                let mut out = Vec::new();
                for item in arr {
                    let value = item
                        .as_str()
                        .with_context(|| format!("Expected string elements for key '{}'", key))?;
                    out.push(value.to_string());
                }
                Ok(out)
            }
            _ => bail!("Expected string or string array for key '{}'", key),
        };
    }
    Ok(Vec::new())
}

fn get_optional_bool(
    table: &toml::map::Map<String, toml::Value>,
    keys: &[&str],
) -> Result<Option<bool>> {
    for key in keys {
        let Some(value) = table.get(*key) else {
            continue;
        };
        return value
            .as_bool()
            .map(Some)
            .with_context(|| format!("Expected boolean for key '{}'", key));
    }
    Ok(None)
}

fn run_hook_command(
    rootfs: &Path,
    hook: &TransactionHook,
    ctx: &HookExecutionContext<'_>,
    normalized_paths: &[String],
    sequence: Option<(usize, usize)>,
) -> Result<()> {
    let hook_name = hook.display_name();
    if let Some((index, total)) = sequence {
        crate::log_info!(
            "Running transaction hook ({}/{}) '{}' for {}:{}:{}",
            index,
            total,
            hook_name,
            ctx.operation.as_str(),
            ctx.phase.as_str(),
            ctx.package
        );
    } else {
        crate::log_info!(
            "Running transaction hook '{}' for {}:{}:{}",
            hook_name,
            ctx.operation.as_str(),
            ctx.phase.as_str(),
            ctx.package
        );
    }

    let stdin_payload = if hook.needs_paths {
        let mut payload = normalized_paths.join("\n");
        if !payload.is_empty() {
            payload.push('\n');
        }
        Some(payload)
    } else {
        None
    };

    let mut command = if fakeroot::is_root() && rootfs.join("bin/sh").exists() {
        let mut cmd = Command::new("chroot");
        cmd.arg(rootfs).arg("/bin/sh").arg("-lc").arg(&hook.command);
        cmd
    } else {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-lc").arg(&hook.command).current_dir(rootfs);
        cmd
    };

    command
        .env("DEPOT_ACTION", ctx.operation.as_str())
        .env("DEPOT_PHASE", ctx.phase.as_str())
        .env("DEPOT_PACKAGE", ctx.package)
        .env("DEPOT_ROOTFS", rootfs)
        .env("DEPOT_HOOK_FILE", &hook.file_path)
        .env("DEPOT_HOOK_NAME", &hook_name)
        .env("PATH", crate::runtime_env::safe_script_path());

    let status = run_command_with_optional_stdin(&mut command, stdin_payload.as_deref())
        .with_context(|| {
            format!(
                "Failed to execute transaction hook '{}' ({})",
                hook_name,
                hook.file_path.display()
            )
        })?;

    if !status.success() {
        bail!(
            "Transaction hook '{}' failed with status {}",
            hook_name,
            status
        );
    }

    Ok(())
}

fn run_command_with_optional_stdin(
    command: &mut Command,
    stdin_payload: Option<&str>,
) -> Result<ExitStatus> {
    if let Some(payload) = stdin_payload {
        command.stdin(Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            match stdin.write_all(payload.as_bytes()) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(child.wait()?)
    } else {
        Ok(command.status()?)
    }
}

fn normalize_affected_paths(paths: &[String]) -> Vec<String> {
    paths.iter().map(|p| normalize_match_target(p)).collect()
}

fn normalize_match_target(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;

    for i in 1..=p.len() {
        if p[i - 1] == b'*' {
            dp[i][0] = dp[i - 1][0];
        }
    }

    for i in 1..=p.len() {
        for j in 1..=t.len() {
            dp[i][j] = match p[i - 1] {
                b'*' => dp[i - 1][j] || dp[i][j - 1],
                b'?' => dp[i - 1][j - 1],
                ch => dp[i - 1][j - 1] && ch == t[j - 1],
            };
        }
    }

    dp[p.len()][t.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_hook(rootfs: &Path, name: &str, content: &str) {
        let dir = transaction_hooks_dir(rootfs);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn wildcard_match_supports_star_and_question() {
        assert!(wildcard_match("usr/lib/*", "usr/lib/libc.so"));
        assert!(wildcard_match("lib??", "lib32"));
        assert!(!wildcard_match("usr/bin/*", "usr/lib/libc.so"));
    }

    #[test]
    fn parse_hook_accepts_starpack_style_sections() {
        let tmp = tempfile::tempdir().unwrap();
        write_hook(
            tmp.path(),
            "demo.toml",
            r#"
[Hook]
Name = "demo"

[When]
Phase = "PreTransaction"
Operation = "Install"
Package = "foo"

[Exec]
Command = "true"
NeedsPaths = false
"#,
        );

        let path = transaction_hooks_dir(tmp.path()).join("demo.toml");
        let hook = parse_hook_file(&path).unwrap();
        assert_eq!(hook.name.as_deref(), Some("demo"));
        assert_eq!(hook.phase, HookPhase::Pre);
        assert_eq!(hook.operations, vec![HookOperation::Install]);
        assert_eq!(hook.packages, vec!["foo".to_string()]);
    }

    #[test]
    fn run_transaction_hooks_executes_matching_hook() {
        let tmp = tempfile::tempdir().unwrap();
        write_hook(
            tmp.path(),
            "record.toml",
            r#"
[hook]
name = "record"

[when]
phase = "pre"
operation = ["install"]
packages = ["foo*"]
paths = ["usr/bin/*"]

[exec]
command = "printf '%s:%s:%s' \"$DEPOT_ACTION\" \"$DEPOT_PHASE\" \"$DEPOT_PACKAGE\" > \"$DEPOT_ROOTFS/hook.out\""
needs_paths = true
"#,
        );

        let affected = vec!["usr/bin/foo".to_string()];
        let ctx = HookExecutionContext {
            phase: HookPhase::Pre,
            operation: HookOperation::Install,
            package: "foo",
            affected_paths: &affected,
        };
        let ran = run_transaction_hooks(tmp.path(), &ctx).unwrap();
        assert_eq!(ran, 1);
        let out = std::fs::read_to_string(tmp.path().join("hook.out")).unwrap();
        assert_eq!(out, "install:pre:foo");
    }

    #[test]
    fn run_transaction_hooks_ignores_broken_pipe_for_needs_paths_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        write_hook(
            tmp.path(),
            "noop.toml",
            r#"
[hook]
name = "noop"

[when]
phase = "pre"
operation = ["install"]
packages = ["foo"]

[exec]
command = "true"
needs_paths = true
"#,
        );

        let affected = vec!["usr/bin/foo".to_string()];
        let ctx = HookExecutionContext {
            phase: HookPhase::Pre,
            operation: HookOperation::Install,
            package: "foo",
            affected_paths: &affected,
        };
        let ran = run_transaction_hooks(tmp.path(), &ctx).unwrap();
        assert_eq!(ran, 1);
    }

    #[test]
    fn run_transaction_hooks_respects_negation_and_filters_out() {
        let tmp = tempfile::tempdir().unwrap();
        write_hook(
            tmp.path(),
            "skip.toml",
            r#"
[hook]
name = "skip"

[when]
phase = "pre"
operation = "remove"
paths = ["usr/lib/*"]
negation = ["usr/lib/debug/*"]

[exec]
command = "touch \"$DEPOT_ROOTFS/should_not_exist\""
"#,
        );

        let affected = vec!["usr/lib/debug/foo".to_string()];
        let ctx = HookExecutionContext {
            phase: HookPhase::Pre,
            operation: HookOperation::Remove,
            package: "foo",
            affected_paths: &affected,
        };
        let ran = run_transaction_hooks(tmp.path(), &ctx).unwrap();
        assert_eq!(ran, 0);
        assert!(!tmp.path().join("should_not_exist").exists());
    }

    #[test]
    fn run_transaction_hooks_batch_executes_in_context_order() {
        let tmp = tempfile::tempdir().unwrap();
        write_hook(
            tmp.path(),
            "batch.toml",
            r#"
[hook]
name = "batch"

[when]
phase = "post"
operation = ["install"]

[exec]
command = "printf '%s\n' \"$DEPOT_PACKAGE\" >> \"$DEPOT_ROOTFS/batch.out\""
"#,
        );

        let contexts = vec![
            HookExecutionContextOwned {
                operation: HookOperation::Install,
                package: "foo".to_string(),
                affected_paths: Vec::new(),
            },
            HookExecutionContextOwned {
                operation: HookOperation::Install,
                package: "bar".to_string(),
                affected_paths: Vec::new(),
            },
        ];

        let ran = run_transaction_hooks_batch(tmp.path(), HookPhase::Post, &contexts).unwrap();
        assert_eq!(ran, 2);
        let out = std::fs::read_to_string(tmp.path().join("batch.out")).unwrap();
        assert_eq!(out, "foo\nbar\n");
    }
}
