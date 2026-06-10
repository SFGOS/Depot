//! Generation helpers for CLI assets.

use crate::cli::Cli;
use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;
use std::fs;
use std::path::Path;

const BIN_NAME: &str = "depot";
const MAN_PAGE: &str = include_str!("../man/depot.1");

/// Generate all supported shell completion scripts and the manual page into `out_dir`.
pub fn generate_cli_assets(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create output directory {}", out_dir.display()))?;

    generate_completion(out_dir, Shell::Bash, "depot.bash")?;
    generate_completion(out_dir, Shell::Zsh, "_depot")?;
    generate_completion(out_dir, Shell::Fish, "depot.fish")?;
    write_man_page(out_dir)?;
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

fn write_man_page(out_dir: &Path) -> Result<()> {
    remove_old_depot_man_pages(out_dir)?;
    let output_path = out_dir.join("depot.1");
    fs::write(&output_path, MAN_PAGE)
        .with_context(|| format!("Failed to write man page to {}", output_path.display()))?;
    Ok(())
}

fn remove_old_depot_man_pages(out_dir: &Path) -> Result<()> {
    for entry in fs::read_dir(out_dir)
        .with_context(|| format!("Failed to read output directory {}", out_dir.display()))?
    {
        let path = entry
            .with_context(|| format!("Failed to inspect output directory {}", out_dir.display()))?
            .path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name == "depot.1" || file_name.starts_with("depot-") && file_name.ends_with(".1") {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove stale man page {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Command;

    fn visible_command_paths(command: &Command) -> Vec<String> {
        command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .flat_map(|subcommand| {
                let name = subcommand.get_name();
                let nested = visible_command_paths(subcommand);
                if nested.is_empty() {
                    vec![name.to_string()]
                } else {
                    let mut paths = vec![name.to_string()];
                    paths.extend(nested.into_iter().map(|nested| format!("{name} {nested}")));
                    paths
                }
            })
            .collect()
    }

    #[test]
    fn generate_cli_assets_writes_expected_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("depot-install.1"), "stale").unwrap();
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
        assert_eq!(man_pages, 1);
        assert!(!std::fs::read_to_string(&man).unwrap().is_empty());
        assert!(!temp.path().join("depot-install.1").exists());
    }

    #[test]
    fn manual_page_documents_visible_command_paths() {
        let command = command_for_generation();
        for path in visible_command_paths(&command) {
            assert!(
                MAN_PAGE.contains(&format!("depot {path}")),
                "manual page does not document `depot {path}`"
            );
        }
    }
}
