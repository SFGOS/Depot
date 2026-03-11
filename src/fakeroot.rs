//! Fakeroot support for running commands as pseudo-root
//! Uses the system fakeroot command when not running as root

use std::fs;
use std::path::Path;
use std::process::Command;

/// Check if we're running as root
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Wrap a command for fakeroot execution
/// For make install, we use the system fakeroot command
pub fn wrap_install_command(program: &str, destdir: &Path) -> Command {
    let script_path = shell_script_path(program);
    if is_root() {
        build_command(program, script_path)
    } else {
        // Use system fakeroot command which handles LD_PRELOAD internally
        let mut cmd = Command::new("fakeroot");
        cmd.arg("--");
        if let Some(script_path) = script_path {
            cmd.arg("sh");
            cmd.arg(script_path);
        } else {
            cmd.arg(program);
        }
        // Fakeroot will ensure file ownership appears as root
        cmd.env("DESTDIR", destdir);
        cmd
    }
}

fn build_command(program: &str, script_path: Option<&Path>) -> Command {
    if let Some(script_path) = script_path {
        let mut cmd = Command::new("sh");
        cmd.arg(script_path);
        cmd
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
