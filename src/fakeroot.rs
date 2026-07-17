//! Rootless install command support backed by Linux user namespaces.

use anyhow::{Context, Result};
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const UID_MAP_PATH: &[u8] = b"/proc/self/uid_map\0";
const GID_MAP_PATH: &[u8] = b"/proc/self/gid_map\0";
const SETGROUPS_PATH: &[u8] = b"/proc/self/setgroups\0";
const UID_XATTR: &[u8] = b"user.depot.fakeroot.uid\0";
const GID_XATTR: &[u8] = b"user.depot.fakeroot.gid\0";
const PRELOAD_LIBRARY_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/libdepot_fakeroot.so"));
static PRELOAD_LIBRARY_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Check if the current process is running as root.
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Build an install command that runs as root inside a private user namespace.
///
/// The namespace maps UID/GID 0 to the invoking user, allowing install scripts
/// to perform root-owned staged installs without changing host ownership or
/// requiring an external fakeroot implementation.
pub fn wrap_install_command(program: &str, destdir: &Path) -> Result<Command> {
    let mut command = build_command(program, shell_script_path(program));
    command.env("DESTDIR", destdir);

    if !is_root() {
        configure_ownership_preload(&mut command)?;
        configure_rootless_namespace(&mut command);
    }

    Ok(command)
}

fn configure_ownership_preload(command: &mut Command) -> Result<()> {
    let library_path = materialize_preload_library()?;
    let mut preload = OsString::from(&library_path);
    if let Some(existing) = std::env::var_os("LD_PRELOAD").filter(|value| !value.is_empty()) {
        preload.push(":");
        preload.push(existing);
    }
    command.env("LD_PRELOAD", preload);
    Ok(())
}

fn materialize_preload_library() -> Result<PathBuf> {
    if let Some(dir) = PRELOAD_LIBRARY_DIR.get() {
        return Ok(dir.path().join("libdepot_fakeroot.so"));
    }

    let candidate = tempfile::Builder::new()
        .prefix("depot-fakeroot-")
        .tempdir()
        .context("Failed to create private fakeroot library dir")?;
    let candidate_path = candidate.path().join("libdepot_fakeroot.so");
    fs::write(&candidate_path, PRELOAD_LIBRARY_BYTES).with_context(|| {
        format!(
            "Failed to write fakeroot library {}",
            candidate_path.display()
        )
    })?;
    fs::set_permissions(&candidate_path, fs::Permissions::from_mode(0o500)).with_context(|| {
        format!(
            "Failed to set permissions on fakeroot library {}",
            candidate_path.display()
        )
    })?;
    let candidate_path = candidate_path.canonicalize().with_context(|| {
        format!(
            "Failed to resolve fakeroot library {}",
            candidate_path.display()
        )
    })?;

    if PRELOAD_LIBRARY_DIR.set(candidate).is_ok() {
        Ok(candidate_path)
    } else {
        Ok(PRELOAD_LIBRARY_DIR
            .get()
            .expect("fakeroot library dir must be initialized")
            .path()
            .join("libdepot_fakeroot.so"))
    }
}

pub(crate) fn archive_ownership(
    path: &Path,
    metadata: &fs::Metadata,
    symlink: bool,
) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let (default_uid, default_gid) = if is_root() {
        (u64::from(metadata.uid()), u64::from(metadata.gid()))
    } else {
        (0, 0)
    };
    let uid = read_id_xattr(path, UID_XATTR, symlink)?.unwrap_or(default_uid);
    let gid = read_id_xattr(path, GID_XATTR, symlink)?.unwrap_or(default_gid);
    Ok((uid, gid))
}

fn read_id_xattr(path: &Path, name: &[u8], symlink: bool) -> Result<Option<u64>> {
    let path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("Path contains an embedded NUL: {}", path.display()))?;
    let mut value = [0_u8; 16];
    let length = unsafe {
        if symlink {
            nix::libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr().cast(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        } else {
            nix::libc::getxattr(
                path.as_ptr(),
                name.as_ptr().cast(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        }
    };
    if length < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if code == nix::libc::ENODATA || code == nix::libc::ENOTSUP => Ok(None),
            _ => Err(error).with_context(|| {
                format!(
                    "Failed to read fakeroot ownership from {}",
                    path.to_string_lossy()
                )
            }),
        };
    }

    let value = std::str::from_utf8(&value[..length as usize]).with_context(|| {
        format!(
            "Invalid fakeroot ownership metadata on {}",
            path.to_string_lossy()
        )
    })?;
    let id = value.parse::<u64>().with_context(|| {
        format!(
            "Invalid fakeroot ownership value {value:?} on {}",
            path.to_string_lossy()
        )
    })?;
    Ok(Some(id))
}

fn build_command(program: &str, script_path: Option<&Path>) -> Command {
    if let Some(script_path) = script_path {
        let mut command = Command::new("sh");
        command.arg(script_path);
        command
    } else {
        Command::new(program)
    }
}

fn shell_script_path(program: &str) -> Option<&Path> {
    let path = Path::new(program);
    if !path.is_absolute() && path.components().count() <= 1 {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    bytes.starts_with(b"#!").then_some(path)
}

fn configure_rootless_namespace(command: &mut Command) {
    let uid_map = format!("0 {} 1\n", nix::unistd::geteuid().as_raw()).into_bytes();
    let gid_map = format!("0 {} 1\n", nix::unistd::getegid().as_raw()).into_bytes();

    // Only async-signal-safe libc calls are made in the post-fork child.
    unsafe {
        command.pre_exec(move || {
            if nix::libc::unshare(nix::libc::CLONE_NEWUSER) != 0 {
                return Err(io::Error::last_os_error());
            }

            write_proc_file(SETGROUPS_PATH, b"deny\n", true)?;
            write_proc_file(UID_MAP_PATH, &uid_map, false)?;
            write_proc_file(GID_MAP_PATH, &gid_map, false)?;

            if nix::libc::setresgid(0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if nix::libc::setresuid(0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(())
        });
    }
}

fn write_proc_file(path: &[u8], contents: &[u8], allow_missing: bool) -> io::Result<()> {
    let fd = unsafe {
        nix::libc::open(
            path.as_ptr().cast(),
            nix::libc::O_WRONLY | nix::libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if allow_missing && error.raw_os_error() == Some(nix::libc::ENOENT) {
            return Ok(());
        }
        return Err(error);
    }

    let result = write_all(fd, contents);
    let close_result = unsafe { nix::libc::close(fd) };
    if result.is_ok() && close_result != 0 {
        return Err(io::Error::last_os_error());
    }
    result
}

fn write_all(fd: i32, mut contents: &[u8]) -> io::Result<()> {
    while !contents.is_empty() {
        let written = unsafe { nix::libc::write(fd, contents.as_ptr().cast(), contents.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(nix::libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::from_raw_os_error(nix::libc::EIO));
        }
        contents = &contents[written as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn rootless_command_runs_as_namespace_root() -> Result<()> {
        if is_root() {
            return Ok(());
        }
        let destdir = tempfile::tempdir()?;
        let mut command = wrap_install_command("/bin/sh", destdir.path())?;
        command
            .arg("-c")
            .arg("test \"$(/usr/bin/id -u)\" = 0; test \"$(/usr/bin/id -g)\" = 0; /usr/bin/touch \"$DESTDIR/root-owned\"; if test -x /usr/bin/python3; then /usr/bin/python3 -c 'import os; os.chown(os.environ[\"DESTDIR\"] + \"/root-owned\", 0, 81)'; else /usr/bin/chown 0:81 \"$DESTDIR/root-owned\"; fi; /usr/bin/chmod 4755 \"$DESTDIR/root-owned\"");

        let status = command
            .status()
            .context("Failed to launch internal fakeroot command")?;
        assert!(status.success());

        let metadata = std::fs::metadata(destdir.path().join("root-owned"))?;
        assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
        assert_eq!(metadata.gid(), nix::unistd::getegid().as_raw());
        assert_eq!(metadata.mode() & 0o7777, 0o4755);
        assert_eq!(
            archive_ownership(&destdir.path().join("root-owned"), &metadata, false)?,
            (0, 81)
        );
        Ok(())
    }

    #[test]
    fn wrapped_command_preserves_program_and_sets_destdir() -> Result<()> {
        let destdir = tempfile::tempdir()?;
        let command = wrap_install_command("make", destdir.path())?;

        assert_eq!(command.get_program(), "make");
        assert!(command.get_envs().any(|(key, value)| {
            key == "DESTDIR" && value.is_some_and(|value| value == destdir.path())
        }));
        Ok(())
    }
}
