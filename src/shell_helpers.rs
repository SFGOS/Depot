//! Shell helper commands exposed to package build/post-install scripts.

use crate::builder::{EnvVars, set_env_var};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Internal staging namespace reserved inside `DESTDIR`.
pub const INTERNAL_DEPOT_DIR: &str = ".depot";
/// Internal split-output staging root inside `DESTDIR`.
pub const INTERNAL_OUTPUTS_DIR: &str = ".depot/outputs";
const DEPOT_HAUL_HELPER_ENV: &str = "DEPOT_HAUL_HELPER";
const DEPOT_SUBDESTDIR_HELPER_ENV: &str = "DEPOT_SUBDESTDIR_HELPER";
const DEPOT_PYTHON_BUILD_HELPER_ENV: &str = "DEPOT_PYTHON_BUILD_HELPER";
const DEPOT_PYTHON_INSTALL_HELPER_ENV: &str = "DEPOT_PYTHON_INSTALL_HELPER";
const DEPOT_EXECUTABLE_ENV: &str = "DEPOT_EXECUTABLE";

/// Ephemeral helper command directory to prepend to PATH while running scripts.
pub struct ShellHelpers {
    _tempdir: TempDir,
    path_value: String,
    outputs_dir: PathBuf,
    depot_executable: PathBuf,
    haul_path: PathBuf,
    subdestdir_path: PathBuf,
    python_build_path: PathBuf,
    python_install_path: PathBuf,
}

impl ShellHelpers {
    /// Create helper commands for a given staging tree (`DESTDIR`).
    pub fn new(destdir: &Path) -> Result<Self> {
        let helper_root = destdir.join(INTERNAL_DEPOT_DIR).join("helpers");
        fs::create_dir_all(&helper_root).with_context(|| {
            format!(
                "Failed to create shell helper root dir: {}",
                helper_root.display()
            )
        })?;
        let tempdir =
            tempfile::tempdir_in(&helper_root).context("Failed to create shell helper tempdir")?;
        let bin_dir = tempdir.path().join("bin");
        fs::create_dir_all(&bin_dir)
            .with_context(|| format!("Failed to create helper bin dir: {}", bin_dir.display()))?;
        let depot_executable = std::env::current_exe()
            .context("Failed to locate depot executable for shell helpers")?;

        let haul_path = bin_dir.join("haul");
        fs::write(&haul_path, HAUL_SCRIPT).with_context(|| {
            format!(
                "Failed to write shell helper command: {}",
                haul_path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&haul_path)
                .with_context(|| format!("Failed to stat helper: {}", haul_path.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&haul_path, perms)
                .with_context(|| format!("Failed to chmod helper: {}", haul_path.display()))?;
        }

        let subdestdir_path = bin_dir.join("subdestdir");
        fs::write(&subdestdir_path, SUBDESTDIR_SCRIPT).with_context(|| {
            format!(
                "Failed to write shell helper command: {}",
                subdestdir_path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&subdestdir_path)
                .with_context(|| format!("Failed to stat helper: {}", subdestdir_path.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&subdestdir_path, perms).with_context(|| {
                format!("Failed to chmod helper: {}", subdestdir_path.display())
            })?;
        }

        let python_build_path = bin_dir.join("python_build");
        fs::write(&python_build_path, PYTHON_BUILD_SCRIPT).with_context(|| {
            format!(
                "Failed to write shell helper command: {}",
                python_build_path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&python_build_path)
                .with_context(|| format!("Failed to stat helper: {}", python_build_path.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&python_build_path, perms).with_context(|| {
                format!("Failed to chmod helper: {}", python_build_path.display())
            })?;
        }

        let python_install_path = bin_dir.join("python_install");
        fs::write(&python_install_path, PYTHON_INSTALL_SCRIPT).with_context(|| {
            format!(
                "Failed to write shell helper command: {}",
                python_install_path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&python_install_path)
                .with_context(|| {
                    format!("Failed to stat helper: {}", python_install_path.display())
                })?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&python_install_path, perms).with_context(|| {
                format!("Failed to chmod helper: {}", python_install_path.display())
            })?;
        }

        let path_value = crate::runtime_env::prepend_helper_to_safe_path(&bin_dir);

        Ok(Self {
            _tempdir: tempdir,
            path_value,
            outputs_dir: destdir.join(INTERNAL_OUTPUTS_DIR),
            depot_executable,
            haul_path,
            subdestdir_path,
            python_build_path,
            python_install_path,
        })
    }

    /// Apply helper-related variables to an environment vector used with `prepare_command`.
    pub fn apply_to_env_vars(&self, env_vars: &mut EnvVars) {
        set_env_var(env_vars, "PATH", self.path_value.clone());
        set_env_var(
            env_vars,
            "DEPOT_OUTPUTS_DIR",
            self.outputs_dir.to_string_lossy().into_owned(),
        );
        set_env_var(env_vars, "DEPOT_INTERNAL_DIR", INTERNAL_DEPOT_DIR);
        set_env_var(
            env_vars,
            DEPOT_HAUL_HELPER_ENV,
            self.haul_path.to_string_lossy().into_owned(),
        );
        set_env_var(
            env_vars,
            DEPOT_SUBDESTDIR_HELPER_ENV,
            self.subdestdir_path.to_string_lossy().into_owned(),
        );
        set_env_var(
            env_vars,
            DEPOT_PYTHON_BUILD_HELPER_ENV,
            self.python_build_path.to_string_lossy().into_owned(),
        );
        set_env_var(
            env_vars,
            DEPOT_PYTHON_INSTALL_HELPER_ENV,
            self.python_install_path.to_string_lossy().into_owned(),
        );
        set_env_var(
            env_vars,
            DEPOT_EXECUTABLE_ENV,
            self.depot_executable.to_string_lossy().into_owned(),
        );
    }
}

/// Wrap a shell command with helper functions that invoke the helper scripts
/// through `/bin/sh`, avoiding direct execution from mounts that may be `noexec`.
pub fn wrap_shell_command(command: &str) -> String {
    format!(
        "haul() {{ /bin/sh \"${{{DEPOT_HAUL_HELPER_ENV}:?}}\" \"$@\"; }}\nsubdestdir() {{ /bin/sh \"${{{DEPOT_SUBDESTDIR_HELPER_ENV}:?}}\" \"$@\"; }}\npython_build() {{ /bin/sh \"${{{DEPOT_PYTHON_BUILD_HELPER_ENV}:?}}\" \"$@\"; }}\npython_install() {{ /bin/sh \"${{{DEPOT_PYTHON_INSTALL_HELPER_ENV}:?}}\" \"$@\"; }}\n{command}"
    )
}

/// Convert a package name into a safe shell identifier suffix.
pub fn shell_ident_suffix(pkg_name: &str) -> String {
    let mut out = String::with_capacity(pkg_name.len().max(1));
    for ch in pkg_name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    if out.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

const HAUL_SCRIPT: &str = r#"#!/bin/sh
set -eu

usage() {
    echo "Usage: haul <output-package> <path-pattern> [path-pattern ...]" >&2
    exit 2
}

fail() {
    echo "haul: $*" >&2
    exit 1
}

[ "$#" -ge 2 ] || usage

out_pkg=$1
shift

case "$out_pkg" in
    ""|.|..|*/*) fail "invalid output package name: $out_pkg" ;;
esac

[ "${DESTDIR:-}" != "" ] || fail "DESTDIR is not set"
[ "${DEPOT_OUTPUTS_DIR:-}" != "" ] || fail "DEPOT_OUTPUTS_DIR is not set"

src_root=$DESTDIR
primary_root=${DEPOT_PRIMARY_DESTDIR:-$DESTDIR}
out_root=$DEPOT_OUTPUTS_DIR/$out_pkg

case "$src_root" in
    */) src_root=${src_root%/} ;;
esac
case "$primary_root" in
    */) primary_root=${primary_root%/} ;;
esac
case "$out_root" in
    */) out_root=${out_root%/} ;;
esac

mkdir -p "$out_root"

tmp_matches=$(mktemp "${TMPDIR:-/tmp}/depot-haul.XXXXXX")
trap 'rm -f "$tmp_matches"' EXIT HUP INT TERM
: > "$tmp_matches"

safe_move() {
    from=$1
    to=$2

    mkdir -p "$(dirname "$to")"

    if mv "$from" "$to" 2>/dev/null; then
        return 0
    fi

    if [ -d "$from" ] && [ ! -L "$from" ]; then
        cp -a "$from" "$to" && rm -rf "$from"
    elif [ -L "$from" ]; then
        cp -P "$from" "$to" && rm -f "$from"
    else
        cp -p "$from" "$to" 2>/dev/null || cp "$from" "$to"
        rm -f "$from"
    fi
}

cleanup_empty_dirs() {
    dir=$1
    stop_at=$2

    while [ "$dir" != "$stop_at" ] && [ "$dir" != "/" ] && [ "$dir" != "." ]; do
        if [ -d "$dir" ] && [ -z "$(ls -A "$dir" 2>/dev/null)" ]; then
            rmdir "$dir" 2>/dev/null || break
            dir=$(dirname "$dir")
        else
            break
        fi
    done
}

scan_root_for_pattern() {
    root=$1
    pattern=$2

    [ -d "$root" ] || return 0

    find "$root" -mindepth 1 -print | LC_ALL=C sort |
    while IFS= read -r abs_path; do
        rel_path=${abs_path#"$root"/}
        [ "$rel_path" != "$abs_path" ] || continue

        case "$root:$rel_path" in
            "$src_root":.depot|"$src_root":.depot/*)
                continue
                ;;
        esac

        case "$rel_path" in
            $pattern)
                printf '%s\t%s\n' "$root" "$rel_path" >> "$tmp_matches"
                ;;
        esac
    done
}

collect_matches_for_pattern() {
    pattern=$1
    before_count=$(wc -l < "$tmp_matches" | tr -d '[:space:]')

    case "$pattern" in
        ""|/|/*|../*|*/../*|..)
            fail "unsafe pattern: $pattern"
            ;;
        .depot|.depot/*)
            fail "refusing to haul internal staging paths: $pattern"
            ;;
    esac

    if [ ! -d "$src_root" ]; then
        fail "DESTDIR does not exist: $src_root"
    fi

    scan_root_for_pattern "$src_root" "$pattern"

    if [ "$primary_root" != "$src_root" ]; then
        scan_root_for_pattern "$primary_root" "$pattern"
    fi

    for candidate_root in "$DEPOT_OUTPUTS_DIR"/*; do
        [ -d "$candidate_root" ] || continue
        [ "$candidate_root" = "$out_root" ] && continue
        scan_root_for_pattern "$candidate_root" "$pattern"
    done

    after_count=$(wc -l < "$tmp_matches" | tr -d '[:space:]')
    if [ "$before_count" = "$after_count" ]; then
        fail "no matches for pattern: $pattern"
    fi
}

for pattern in "$@"; do
    collect_matches_for_pattern "$pattern"
done

LC_ALL=C sort -u "$tmp_matches" | while IFS='	' read -r match_root rel_path; do
    [ -n "$rel_path" ] || continue
    src_path=$match_root/$rel_path

    if [ ! -e "$src_path" ] && [ ! -L "$src_path" ]; then
        # Path may already have been moved because an ancestor directory matched.
        continue
    fi

    dst_path=$out_root/$rel_path
    if [ "$src_path" = "$dst_path" ]; then
        continue
    fi

    if [ -d "$src_path" ] && [ ! -L "$src_path" ] && [ -d "$dst_path" ]; then
        for child in "$src_path"/* "$src_path"/.[!.]* "$src_path"/..?*; do
            [ -e "$child" ] || [ -L "$child" ] || continue
            child_name=$(basename "$child")
            child_dst=$dst_path/$child_name
            if [ -e "$child_dst" ] || [ -L "$child_dst" ]; then
                fail "destination already exists: $child_dst"
            fi
            safe_move "$child" "$child_dst" || fail "failed to move: $child -> $child_dst"
        done
        rmdir "$src_path" 2>/dev/null || true
    else
        if [ -e "$dst_path" ] || [ -L "$dst_path" ]; then
            fail "destination already exists: $dst_path"
        fi
        safe_move "$src_path" "$dst_path" || fail "failed to move: $src_path -> $dst_path"
    fi

    cleanup_empty_dirs "$(dirname "$src_path")" "$match_root"
done

# Clean empty directories left behind in the primary staging tree, but never
# touch the internal depot namespace.
find "$src_root" -depth -type d \
    ! -path "$src_root" \
    ! -path "$src_root/.depot" ! -path "$src_root/.depot/*" \
    -empty -exec rmdir {} + 2>/dev/null || true
"#;

const SUBDESTDIR_SCRIPT: &str = r#"#!/bin/sh
set -eu

fail() {
    echo "subdestdir: $*" >&2
    exit 1
}

[ "$#" -eq 1 ] || fail "usage: subdestdir <output-package>"
[ "${DEPOT_OUTPUTS_DIR:-}" != "" ] || fail "DEPOT_OUTPUTS_DIR is not set"

pkg=$1
case "$pkg" in
    ""|.|..|*/*) fail "invalid output package name: $pkg" ;;
esac

path=$DEPOT_OUTPUTS_DIR/$pkg
mkdir -p "$path"
printf '%s\n' "$path"
"#;

const PYTHON_BUILD_SCRIPT: &str = r#"#!/bin/sh
set -eu

fail() {
    echo "python_build: $*" >&2
    exit 1
}

[ "${DEPOT_EXECUTABLE:-}" != "" ] || fail "DEPOT_EXECUTABLE is not set"

exec "$DEPOT_EXECUTABLE" internal python-build "$@"
"#;

const PYTHON_INSTALL_SCRIPT: &str = r#"#!/bin/sh
set -eu

fail() {
    echo "python_install: $*" >&2
    exit 1
}

[ "${DEPOT_EXECUTABLE:-}" != "" ] || fail "DEPOT_EXECUTABLE is not set"
[ "${DESTDIR:-}" != "" ] || fail "DESTDIR is not set"

exec "$DEPOT_EXECUTABLE" internal python-install "$@"
"#;

#[cfg(test)]
mod tests {
    use super::{INTERNAL_DEPOT_DIR, ShellHelpers, shell_ident_suffix, wrap_shell_command};
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn run_haul(helpers: &ShellHelpers, destdir: &Path, args: &[&str]) {
        run_haul_with_env(helpers, destdir, &[], args);
    }

    fn run_haul_with_env(
        helpers: &ShellHelpers,
        destdir: &Path,
        extra_envs: &[(&str, &Path)],
        args: &[&str],
    ) {
        let mut envs = Vec::new();
        helpers.apply_to_env_vars(&mut envs);
        envs.push(("DESTDIR".into(), destdir.to_string_lossy().into_owned()));
        for (key, value) in extra_envs {
            envs.push(((*key).into(), value.to_string_lossy().into_owned()));
        }

        let mut cmd = Command::new("sh");
        cmd.arg(&helpers.haul_path);
        for arg in args {
            cmd.arg(arg);
        }
        for (key, value) in &envs {
            cmd.env(key, value);
        }

        let status = cmd.status().unwrap();
        assert!(status.success(), "haul failed for args: {args:?}");
    }

    #[test]
    fn shell_ident_suffix_normalizes_package_names() {
        assert_eq!(shell_ident_suffix("clang"), "CLANG");
        assert_eq!(shell_ident_suffix("llvm-libgcc"), "LLVM_LIBGCC");
        assert_eq!(shell_ident_suffix("3foo"), "_3FOO");
        assert_eq!(shell_ident_suffix("foo.bar+baz"), "FOO_BAR_BAZ");
    }

    #[test]
    fn shell_helpers_use_destdir_internal_helper_dir() {
        let destdir = tempdir().unwrap();
        let helpers = ShellHelpers::new(destdir.path()).unwrap();
        let mut envs = Vec::new();
        helpers.apply_to_env_vars(&mut envs);

        let path = envs
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value)
            .unwrap();

        let helper_prefix = destdir
            .path()
            .join(INTERNAL_DEPOT_DIR)
            .join("helpers")
            .to_string_lossy()
            .into_owned();
        assert!(path.starts_with(&helper_prefix));
    }

    #[test]
    fn shell_helpers_export_python_helper_paths() {
        let destdir = tempdir().unwrap();
        let helpers = ShellHelpers::new(destdir.path()).unwrap();
        let mut envs = Vec::new();
        helpers.apply_to_env_vars(&mut envs);

        assert!(
            envs.iter()
                .any(|(key, _)| key == "DEPOT_PYTHON_BUILD_HELPER")
        );
        assert!(
            envs.iter()
                .any(|(key, _)| key == "DEPOT_PYTHON_INSTALL_HELPER")
        );
        assert!(envs.iter().any(|(key, _)| key == "DEPOT_EXECUTABLE"));
    }

    #[test]
    fn wrap_shell_command_exposes_python_helpers() {
        let wrapped = wrap_shell_command("python_build\npython_install");
        assert!(wrapped.contains("python_build()"));
        assert!(wrapped.contains("python_install()"));
    }

    #[test]
    fn haul_can_move_paths_from_existing_output_staging() {
        let temp = tempdir().unwrap();
        let destdir = temp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/share/vulkan/implicit_layer.d")).unwrap();
        std::fs::write(
            destdir.join("usr/share/vulkan/implicit_layer.d/mesa.json"),
            "mesa",
        )
        .unwrap();

        let helpers = ShellHelpers::new(&destdir).unwrap();

        run_haul(
            &helpers,
            &destdir,
            &["vulkan-a", "usr/share/vulkan/implicit_layer.d"],
        );
        run_haul(
            &helpers,
            &destdir,
            &["vulkan-b", "usr/share/vulkan/implicit_layer.d"],
        );

        assert!(
            !destdir
                .join(".depot/outputs/vulkan-a/usr/share/vulkan/implicit_layer.d")
                .exists()
        );
        assert!(
            destdir
                .join(".depot/outputs/vulkan-b/usr/share/vulkan/implicit_layer.d/mesa.json")
                .exists()
        );
    }

    #[test]
    fn haul_merges_directory_into_existing_output_dir() {
        let temp = tempdir().unwrap();
        let destdir = temp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/share/vulkan/explicit_layer.d")).unwrap();
        std::fs::write(
            destdir.join("usr/share/vulkan/explicit_layer.d/new-layer.json"),
            "new",
        )
        .unwrap();
        std::fs::create_dir_all(
            destdir.join(".depot/outputs/vulkan-layers/usr/share/vulkan/explicit_layer.d"),
        )
        .unwrap();
        std::fs::write(
            destdir.join(
                ".depot/outputs/vulkan-layers/usr/share/vulkan/explicit_layer.d/existing.json",
            ),
            "existing",
        )
        .unwrap();

        let helpers = ShellHelpers::new(&destdir).unwrap();
        run_haul(
            &helpers,
            &destdir,
            &["vulkan-layers", "usr/share/vulkan/explicit_layer.d"],
        );

        assert!(!destdir.join("usr/share/vulkan/explicit_layer.d").exists());
        assert!(
            destdir
                .join(
                    ".depot/outputs/vulkan-layers/usr/share/vulkan/explicit_layer.d/existing.json"
                )
                .exists()
        );
        assert!(
            destdir
                .join(
                    ".depot/outputs/vulkan-layers/usr/share/vulkan/explicit_layer.d/new-layer.json"
                )
                .exists()
        );
    }

    #[test]
    fn haul_scans_primary_staging_when_running_in_split_output_destdir() {
        let temp = tempdir().unwrap();
        let primary_destdir = temp.path().join("dest");
        std::fs::create_dir_all(primary_destdir.join("usr/lib")).unwrap();
        std::fs::write(
            primary_destdir.join("usr/lib/libRusticlOpenCL.so.1.0.0"),
            "rusticl",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "libRusticlOpenCL.so.1.0.0",
            primary_destdir.join("usr/lib/libRusticlOpenCL.so.1"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "libRusticlOpenCL.so.1",
            primary_destdir.join("usr/lib/libRusticlOpenCL.so"),
        )
        .unwrap();

        let helpers = ShellHelpers::new(&primary_destdir).unwrap();
        let split_destdir = primary_destdir.join(".depot/outputs/opencl-mesa");
        std::fs::create_dir_all(&split_destdir).unwrap();

        run_haul_with_env(
            &helpers,
            &split_destdir,
            &[("DEPOT_PRIMARY_DESTDIR", &primary_destdir)],
            &["opencl-mesa", "usr/lib/libRusticlOpenCL*"],
        );

        assert!(
            !primary_destdir
                .join("usr/lib/libRusticlOpenCL.so.1.0.0")
                .exists()
        );
        assert!(
            split_destdir
                .join("usr/lib/libRusticlOpenCL.so.1.0.0")
                .exists()
        );
        let link_meta =
            std::fs::symlink_metadata(split_destdir.join("usr/lib/libRusticlOpenCL.so")).unwrap();
        assert!(link_meta.file_type().is_symlink());
    }
}
