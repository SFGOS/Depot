//! Build state tracking to allow resuming interrupted builds.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BuildStep {
    PatchesApplied,
    PostExtractDone,
    Configured,
    PostCompileDone,
    PostInstallDone,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    completed_steps: HashSet<BuildStep>,
}

pub struct StateTracker {
    state_file: PathBuf,
    state: State,
}

impl StateTracker {
    pub fn new(source_dir: &Path) -> Result<Self> {
        Self::new_with_namespace(source_dir, None)
    }

    pub fn new_with_namespace(source_dir: &Path, namespace: Option<&str>) -> Result<Self> {
        let state_file = if let Some(ns) = namespace.and_then(normalize_state_namespace) {
            source_dir.join(format!(".depot_state_{}", ns))
        } else {
            source_dir.join(".depot_state")
        };
        let state = if state_file.exists() {
            let content = fs::read_to_string(&state_file)?;
            toml::from_str(&content).unwrap_or_default()
        } else {
            State::default()
        };

        Ok(Self { state_file, state })
    }

    pub fn is_done(&self, step: BuildStep) -> bool {
        self.state.completed_steps.contains(&step)
    }

    pub fn mark_done(&mut self, step: BuildStep) -> Result<()> {
        self.state.completed_steps.insert(step);
        self.save()
    }

    fn save(&self) -> Result<()> {
        let content = toml::to_string_pretty(&self.state)?;
        fs::write(&self.state_file, content)?;
        Ok(())
    }
}

fn normalize_state_namespace(namespace: &str) -> Option<String> {
    let normalized: String = namespace
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = normalized.trim_matches('_');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_state_tracker_init() -> Result<()> {
        let dir = tempdir()?;
        let tracker = StateTracker::new(dir.path())?;
        assert!(!tracker.state_file.exists());
        Ok(())
    }

    #[test]
    fn test_mark_done_and_persistence() -> Result<()> {
        let dir = tempdir()?;
        let mut tracker = StateTracker::new(dir.path())?;

        assert!(!tracker.is_done(BuildStep::Configured));
        tracker.mark_done(BuildStep::Configured)?;
        assert!(tracker.is_done(BuildStep::Configured));

        // Check file exists
        assert!(tracker.state_file.exists());

        // Reload
        let tracker2 = StateTracker::new(dir.path())?;
        assert!(tracker2.is_done(BuildStep::Configured));
        assert!(!tracker2.is_done(BuildStep::PostCompileDone));

        Ok(())
    }
}
