//! Fakeroot support for running commands as pseudo-root
//! Uses the system fakeroot command when not running as root

use std::path::Path;
use std::process::Command;

/// Check if we're running as root
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Wrap a command for fakeroot execution
/// For make install, we use the system fakeroot command
pub fn wrap_install_command(program: &str, destdir: &Path) -> Command {
    if is_root() {
        Command::new(program)
    } else {
        // Use system fakeroot command which handles LD_PRELOAD internally
        let mut cmd = Command::new("fakeroot");
        cmd.arg("--");
        cmd.arg(program);
        // Fakeroot will ensure file ownership appears as root
        cmd.env("DESTDIR", destdir);
        cmd
    }
}
