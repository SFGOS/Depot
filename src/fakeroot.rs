//! Rootless install command support backed by Linux user namespaces.

use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

const UID_MAP_PATH: &[u8] = b"/proc/self/uid_map\0";
const GID_MAP_PATH: &[u8] = b"/proc/self/gid_map\0";
const SETGROUPS_PATH: &[u8] = b"/proc/self/setgroups\0";

/// Check if the current process is running as root.
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Build an install command that runs as root inside a private user namespace.
///
/// The namespace maps UID/GID 0 to the invoking user, allowing install scripts
/// to perform root-owned staged installs without changing host ownership or
/// requiring an external fakeroot implementation.
pub fn wrap_install_command(program: &str, destdir: &Path) -> Command {
    let mut command = build_command(program, shell_script_path(program));
    command.env("DESTDIR", destdir);

    if !is_root() {
        configure_rootless_namespace(&mut command);
    }

    command
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
        let destdir = tempfile::tempdir()?;
        let mut command = wrap_install_command("/bin/sh", destdir.path());
        command
            .arg("-c")
            .arg("test \"$(/usr/bin/id -u)\" = 0; test \"$(/usr/bin/id -g)\" = 0; /usr/bin/touch \"$DESTDIR/root-owned\"; /usr/bin/chown 0:0 \"$DESTDIR/root-owned\"; /usr/bin/chmod 4755 \"$DESTDIR/root-owned\"");

        let status = command
            .status()
            .context("Failed to launch internal fakeroot command")?;
        assert!(status.success());

        let metadata = std::fs::metadata(destdir.path().join("root-owned"))?;
        assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
        assert_eq!(metadata.gid(), nix::unistd::getegid().as_raw());
        assert_eq!(metadata.mode() & 0o7777, 0o4755);
        Ok(())
    }

    #[test]
    fn wrapped_command_preserves_program_and_sets_destdir() {
        let command = wrap_install_command("make", Path::new("/tmp/package-root"));

        assert_eq!(command.get_program(), "make");
        assert!(command.get_envs().any(|(key, value)| {
            key == "DESTDIR" && value.is_some_and(|value| value == "/tmp/package-root")
        }));
    }
}
