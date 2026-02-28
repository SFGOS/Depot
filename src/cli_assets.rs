//! Generation helpers for CLI man pages and shell completions.

use crate::cli::Cli;
use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;
use std::fs;
use std::path::Path;

const BIN_NAME: &str = "depot";

/// Generate all supported shell completion scripts and a man page into `out_dir`.
pub fn generate_cli_assets(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create output directory {}", out_dir.display()))?;

    generate_completion(out_dir, Shell::Bash, "depot.bash")?;
    generate_completion(out_dir, Shell::Zsh, "_depot")?;
    generate_completion(out_dir, Shell::Fish, "depot.fish")?;
    generate_man_pages(out_dir)?;
    Ok(())
}

fn command_for_generation() -> clap::Command {
    Cli::command().name(BIN_NAME)
}

fn generate_completion(out_dir: &Path, shell: Shell, filename: &str) -> Result<()> {
    let mut command = command_for_generation();
    let output_path = out_dir.join(filename);
    let mut buffer = Vec::new();
    clap_complete::generate(shell, &mut command, BIN_NAME, &mut buffer);
    fs::write(&output_path, buffer).with_context(|| {
        format!(
            "Failed to write {} completion to {}",
            shell,
            output_path.display()
        )
    })?;
    Ok(())
}

fn generate_man_pages(out_dir: &Path) -> Result<()> {
    clap_mangen::generate_to(command_for_generation(), out_dir)
        .context("Failed to generate clap man pages")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_cli_assets_writes_expected_files() {
        let temp = tempfile::tempdir().unwrap();
        generate_cli_assets(temp.path()).unwrap();

        let bash = temp.path().join("depot.bash");
        let zsh = temp.path().join("_depot");
        let fish = temp.path().join("depot.fish");
        let man = temp.path().join("depot.1");
        let man_pages = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "1"))
            .count();

        assert!(bash.exists());
        assert!(zsh.exists());
        assert!(fish.exists());
        assert!(man.exists());
        assert!(man_pages > 1);
        assert!(!std::fs::read_to_string(&man).unwrap().is_empty());
    }
}
