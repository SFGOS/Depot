//! Shared runtime environment defaults for script and hook execution.

use std::path::Path;

/// Deterministic command search path for shell scripts and hook commands.
pub const SAFE_SCRIPT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Return the deterministic command search path used for script execution.
pub fn safe_script_path() -> &'static str {
    SAFE_SCRIPT_PATH
}

/// Prepend a helper binary directory before the deterministic script path.
pub fn prepend_helper_to_safe_path(helper_bin_dir: &Path) -> String {
    format!("{}:{}", helper_bin_dir.display(), SAFE_SCRIPT_PATH)
}
