use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub(crate) fn current_utc_timestamp_string() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("Failed to format UTC timestamp")
}

pub(crate) fn parse_completed_at_value(metadata: &toml::Value) -> Option<i64> {
    metadata
        .get("completed_at")
        .and_then(|value| completed_at_value_to_unix(value).ok().flatten())
}

pub(crate) fn read_completed_at_from_metadata_path(path: &Path) -> Result<Option<i64>> {
    if !path.exists() {
        return Ok(None);
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let metadata: toml::Value =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(parse_completed_at_value(&metadata))
}

pub(crate) fn system_time_to_unix(time: SystemTime) -> Result<i64> {
    let duration = time
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow::anyhow!("System clock is before UNIX_EPOCH: {}", err))?;
    Ok(duration.as_secs() as i64)
}

fn completed_at_value_to_unix(value: &toml::Value) -> Result<Option<i64>> {
    if let Some(raw) = value.as_integer() {
        return Ok(Some(raw));
    }

    let Some(raw) = value.as_str() else {
        return Ok(None);
    };
    let parsed = OffsetDateTime::parse(raw, &Rfc3339)
        .with_context(|| format!("Invalid UTC timestamp '{}'", raw))?;
    Ok(Some(parsed.unix_timestamp()))
}
