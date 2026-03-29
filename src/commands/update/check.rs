use super::*;
use crate::commands::update::versions::{
    CheckStatus, archive_listing_probe, best_newer_version, candidate_versions_from_refs,
    compare_versions_for_updates, extract_version_patterns, list_archive_versions,
    remote_git_repository_from_source_url,
};

fn source_check_status(
    spec: &package::PackageSpec,
    source: &package::Source,
) -> Result<CheckStatus> {
    let patterns = extract_version_patterns(&source.url);
    if patterns.is_empty() {
        anyhow::bail!("source URL does not contain $version");
    }

    let expanded_url = spec.expand_vars(&source.url);
    let mut reasons = Vec::new();

    let (candidates, source_label) =
        if let Some(repo_url) = remote_git_repository_from_source_url(&expanded_url) {
            match super::versions::list_remote_refs(&repo_url)
                .map(|refs| candidate_versions_from_refs(&refs, &patterns))
            {
                Ok(candidates) if !candidates.is_empty() => {
                    (candidates, format!("git tags {}", repo_url))
                }
                Ok(_) => {
                    reasons.push(format!("no matching git tags found in {}", repo_url));
                    if let Some(probe) = archive_listing_probe(&source.url, &expanded_url) {
                        let candidates = list_archive_versions(&probe)?;
                        (candidates, format!("archive index {}", probe.listing_url))
                    } else {
                        anyhow::bail!("{}", reasons.remove(0));
                    }
                }
                Err(err) => {
                    reasons.push(err.to_string());
                    if let Some(probe) = archive_listing_probe(&source.url, &expanded_url) {
                        match list_archive_versions(&probe) {
                            Ok(candidates) => {
                                (candidates, format!("archive index {}", probe.listing_url))
                            }
                            Err(archive_err) => {
                                reasons.push(archive_err.to_string());
                                anyhow::bail!("{}", reasons.join("; "));
                            }
                        }
                    } else {
                        anyhow::bail!("{}", reasons.remove(0));
                    }
                }
            }
        } else if let Some(probe) = archive_listing_probe(&source.url, &expanded_url) {
            let candidates = list_archive_versions(&probe)?;
            (candidates, format!("archive index {}", probe.listing_url))
        } else {
            anyhow::bail!(
                "could not derive a git remote or archive index from {}",
                expanded_url
            );
        };

    if let Some(latest) =
        best_newer_version(&spec.package.version, candidates.iter().map(String::as_str))
    {
        Ok(CheckStatus::UpdateAvailable {
            latest,
            source: source_label,
        })
    } else {
        Ok(CheckStatus::UpToDate {
            source: source_label,
        })
    }
}

fn check_package_spec(spec_path: &Path) -> CheckStatus {
    let spec = match package::PackageSpec::from_file(spec_path) {
        Ok(spec) => spec,
        Err(err) => {
            return CheckStatus::Unknown {
                reason: err.to_string(),
            };
        }
    };

    let mut best_update: Option<(String, String)> = None;
    let mut last_up_to_date_source: Option<String> = None;
    let mut reasons = Vec::new();

    for source in spec.sources() {
        if let Err(err) = crate::interrupts::check() {
            return CheckStatus::Unknown {
                reason: err.to_string(),
            };
        }
        match source_check_status(&spec, source) {
            Ok(CheckStatus::UpdateAvailable { latest, source }) => {
                let replace = match &best_update {
                    Some((current_best, _)) => {
                        compare_versions_for_updates(&latest, current_best) == Ordering::Greater
                    }
                    None => true,
                };
                if replace {
                    best_update = Some((latest, source));
                }
            }
            Ok(CheckStatus::UpToDate { source }) => {
                if last_up_to_date_source.is_none() {
                    last_up_to_date_source = Some(source);
                }
            }
            Ok(CheckStatus::Unknown { reason }) => reasons.push(reason),
            Err(err) => reasons.push(err.to_string()),
        }
    }

    if let Some((latest, source)) = best_update {
        return CheckStatus::UpdateAvailable { latest, source };
    }
    if let Some(source) = last_up_to_date_source {
        return CheckStatus::UpToDate { source };
    }

    CheckStatus::Unknown {
        reason: reasons
            .into_iter()
            .next()
            .unwrap_or_else(|| "no versioned sources found".to_string()),
    }
}

pub(crate) fn run_check_command(dir: &Path) -> Result<()> {
    let scan_root = dir
        .canonicalize()
        .with_context(|| format!("Failed to resolve check root {}", dir.display()))?;
    let specs = crate::commands::repo::groups::scan_package_specs(&scan_root)?;
    if specs.is_empty() {
        ui::info(format!(
            "No depot package specs found under {}",
            scan_root.display()
        ));
        return Ok(());
    }

    let verbose = std::env::var_os("DEPOT_CHECK_VERBOSE").is_some();
    let mut updates = 0usize;
    let mut up_to_date = 0usize;
    let mut skipped = Vec::new();

    for spec_path in specs {
        crate::interrupts::check()?;
        let spec = match package::PackageSpec::from_file(&spec_path) {
            Ok(spec) => spec,
            Err(err) => {
                skipped.push(format!(
                    "{} could not be loaded: {}",
                    spec_path.display(),
                    err
                ));
                continue;
            }
        };
        match check_package_spec(&spec_path) {
            CheckStatus::UpdateAvailable { latest, source } => {
                updates += 1;
                ui::warn(format!(
                    "{} {} -> {} [{}] ({})",
                    spec.package.name,
                    spec.package.version,
                    latest,
                    source,
                    spec_path.display()
                ));
            }
            CheckStatus::UpToDate { source } => {
                up_to_date += 1;
                if verbose {
                    ui::info(format!(
                        "{} {} is up to date [{}] ({})",
                        spec.package.name,
                        spec.package.version,
                        source,
                        spec_path.display()
                    ));
                }
            }
            CheckStatus::Unknown { reason } => {
                skipped.push(format!(
                    "{} {} could not be checked: {} ({})",
                    spec.package.name,
                    spec.package.version,
                    reason,
                    spec_path.display()
                ));
            }
        }
    }

    if verbose {
        for entry in &skipped {
            ui::warn(entry);
        }
    } else if !skipped.is_empty() {
        ui::warn(format!(
            "Skipped {} package(s) that could not be checked; set DEPOT_CHECK_VERBOSE=1 for per-package reasons.",
            skipped.len()
        ));
    }

    ui::info(format!(
        "Check summary: {} package(s), {} update(s), {} up to date, {} skipped",
        updates + up_to_date + skipped.len(),
        updates,
        up_to_date,
        skipped.len()
    ));

    Ok(())
}
