use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct VersionPattern {
    pub(crate) prefix: String,
    pub(crate) suffix: String,
}

#[derive(Debug, Clone)]
pub(crate) enum CheckStatus {
    UpdateAvailable { latest: String, source: String },
    UpToDate { source: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone)]
pub(crate) struct ArchiveListingProbe {
    pub(crate) listing_url: String,
    pub(crate) patterns: Vec<VersionPattern>,
}

fn strip_known_archive_suffixes(input: &str) -> &str {
    for suffix in [
        ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tgz", ".txz", ".tbz2", ".zip", ".tar",
        ".git",
    ] {
        if let Some(stripped) = input.strip_suffix(suffix) {
            return stripped;
        }
    }
    input
}

fn version_pattern_from_template(
    template: &str,
    strip_archive_suffix: bool,
) -> Option<VersionPattern> {
    let (prefix, suffix) = template.split_once("$version")?;
    let suffix = if strip_archive_suffix {
        strip_known_archive_suffixes(suffix)
    } else {
        suffix
    };
    Some(VersionPattern {
        prefix: prefix.to_string(),
        suffix: suffix.to_string(),
    })
}

pub(crate) fn extract_version_patterns(raw: &str) -> Vec<VersionPattern> {
    let mut patterns = HashSet::new();
    let mut start = 0usize;

    while let Some(rel_idx) = raw[start..].find("$version") {
        let idx = start + rel_idx;
        let prefix_start = raw[..idx]
            .rfind(['/', '#', '?', '&', '='])
            .map(|pos| pos + 1)
            .unwrap_or(0);
        let suffix_end = raw[idx + "$version".len()..]
            .find(['/', '#', '?', '&', '='])
            .map(|pos| idx + "$version".len() + pos)
            .unwrap_or(raw.len());
        if let Some(pattern) = version_pattern_from_template(&raw[prefix_start..suffix_end], true) {
            patterns.insert(pattern);
        }
        start = idx + "$version".len();
    }

    let mut out: Vec<_> = patterns.into_iter().collect();
    out.sort_by(|a, b| {
        a.prefix
            .cmp(&b.prefix)
            .then_with(|| a.suffix.cmp(&b.suffix))
    });
    out
}

fn is_version_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-')
}

fn looks_like_version(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= 64
        && candidate.chars().all(is_version_char)
        && candidate.chars().any(|ch| ch.is_ascii_digit())
}

fn match_version_pattern<'a>(value: &'a str, pattern: &VersionPattern) -> Option<&'a str> {
    if !value.starts_with(&pattern.prefix) || !value.ends_with(&pattern.suffix) {
        return None;
    }

    let start = pattern.prefix.len();
    let end = value.len().saturating_sub(pattern.suffix.len());
    if end <= start {
        return None;
    }

    let candidate = &value[start..end];
    looks_like_version(candidate).then_some(candidate)
}

fn compare_version_fallback(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut li = 0usize;
    let mut ri = 0usize;

    while li < left.len() && ri < right.len() {
        let lch = left[li] as char;
        let rch = right[ri] as char;
        let l_digit = lch.is_ascii_digit();
        let r_digit = rch.is_ascii_digit();

        if l_digit && r_digit {
            let l_start = li;
            let r_start = ri;
            while li < left.len() && (left[li] as char).is_ascii_digit() {
                li += 1;
            }
            while ri < right.len() && (right[ri] as char).is_ascii_digit() {
                ri += 1;
            }

            let l_raw = std::str::from_utf8(&left[l_start..li]).unwrap_or_default();
            let r_raw = std::str::from_utf8(&right[r_start..ri]).unwrap_or_default();
            let l_trimmed = l_raw.trim_start_matches('0');
            let r_trimmed = r_raw.trim_start_matches('0');

            let l_cmp = if l_trimmed.is_empty() { "0" } else { l_trimmed };
            let r_cmp = if r_trimmed.is_empty() { "0" } else { r_trimmed };
            match l_cmp
                .len()
                .cmp(&r_cmp.len())
                .then_with(|| l_cmp.cmp(r_cmp))
                .then_with(|| l_raw.len().cmp(&r_raw.len()))
                .then_with(|| l_raw.cmp(r_raw))
            {
                Ordering::Equal => {}
                non_eq => return non_eq,
            }
            continue;
        }

        match lch
            .to_ascii_lowercase()
            .cmp(&rch.to_ascii_lowercase())
            .then_with(|| lch.cmp(&rch))
        {
            Ordering::Equal => {
                li += 1;
                ri += 1;
            }
            non_eq => return non_eq,
        }
    }

    left.len().cmp(&right.len())
}

fn canonical_update_version(raw: &str) -> &str {
    raw.strip_prefix('v')
        .filter(|rest| rest.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(raw)
}

fn collapse_date_like_components(value: &str) -> String {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() < 3 {
        return value.to_string();
    }
    if !parts[0].chars().all(|ch| ch.is_ascii_digit())
        || !parts[1].chars().all(|ch| ch.is_ascii_digit())
        || !parts[2].chars().all(|ch| ch.is_ascii_digit())
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts[2].len() != 2
    {
        return value.to_string();
    }

    let mut collapsed = vec![format!("{}{}{}", parts[0], parts[1], parts[2])];
    collapsed.extend(parts.into_iter().skip(3).map(str::to_string));
    collapsed.join(".")
}

fn normalize_comparable_version(raw: &str) -> String {
    let mut value = canonical_update_version(raw).trim().to_ascii_lowercase();
    if let Some(first_digit) = value.find(|ch: char| ch.is_ascii_digit())
        && first_digit > 0
        && value[..first_digit].chars().all(|ch| !ch.is_ascii_digit())
    {
        value = value[first_digit..].to_string();
    }

    let mut normalized = String::with_capacity(value.len());
    let mut last_was_dot = false;
    for ch in value.chars() {
        let mapped = match ch {
            '_' | '-' | '+' => '.',
            ch if ch.is_ascii_alphanumeric() || ch == '.' => ch,
            _ => continue,
        };
        if mapped == '.' {
            if normalized.is_empty() || last_was_dot {
                continue;
            }
            last_was_dot = true;
        } else {
            last_was_dot = false;
        }
        normalized.push(mapped);
    }
    while normalized.ends_with('.') {
        normalized.pop();
    }

    collapse_date_like_components(&normalized)
}

fn numeric_component_count(value: &str) -> usize {
    let mut count = 0usize;
    let mut in_digits = false;
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                count += 1;
                in_digits = true;
            }
        } else {
            in_digits = false;
        }
    }
    count
}

fn first_numeric_component_len(value: &str) -> Option<usize> {
    let start = value.find(|ch: char| ch.is_ascii_digit())?;
    Some(
        value[start..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .count(),
    )
}

fn is_prerelease_version(value: &str) -> bool {
    let normalized = normalize_comparable_version(value);
    [
        "alpha", "beta", "rc", "pre", "preview", "snapshot", "nightly", "dev",
    ]
    .into_iter()
    .any(|marker| normalized.contains(marker))
}

fn normalize_candidate_version(current: &str, candidate: &str) -> Option<String> {
    let normalized = normalize_comparable_version(candidate);
    if !looks_like_version(&normalized) {
        return None;
    }
    if !is_prerelease_version(current) && is_prerelease_version(candidate) {
        return None;
    }

    let current_normalized = normalize_comparable_version(current);
    if numeric_component_count(&current_normalized) >= 2
        && numeric_component_count(&normalized) == 1
        && first_numeric_component_len(&current_normalized).is_some_and(|len| len <= 4)
    {
        return None;
    }

    Some(normalized)
}

pub(crate) fn compare_versions_for_updates(left: &str, right: &str) -> Ordering {
    let left = normalize_comparable_version(left);
    let right = normalize_comparable_version(right);
    if let (Ok(left), Ok(right)) = (
        semver::Version::parse(&left),
        semver::Version::parse(&right),
    ) {
        match left.cmp(&right) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
    }

    if left.len() == 8
        && right.len() == 8
        && left.chars().all(|ch| ch.is_ascii_digit())
        && right.chars().all(|ch| ch.is_ascii_digit())
    {
        return left.cmp(&right);
    }

    compare_version_fallback(&left, &right)
}

pub(crate) fn best_newer_version<'a>(
    current: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let mut best: Option<String> = None;
    for candidate in candidates {
        let Some(candidate) = normalize_candidate_version(current, candidate) else {
            continue;
        };
        if compare_versions_for_updates(&candidate, current) != Ordering::Greater {
            continue;
        }
        if let Some(existing) = best.as_deref()
            && compare_versions_for_updates(&candidate, existing) != Ordering::Greater
        {
            continue;
        }
        best = Some(candidate);
    }
    best
}

pub(crate) fn remote_git_repository_from_source_url(expanded_url: &str) -> Option<String> {
    if let Some((base, _)) = source_git_url_parts(expanded_url) {
        return Some(base);
    }

    let parsed = Url::parse(expanded_url).ok()?;
    let host = parsed.host_str()?;
    let segments: Vec<_> = parsed.path_segments()?.collect();
    if segments.len() < 3 {
        return None;
    }

    let keyword_idx = segments
        .iter()
        .position(|segment| matches!(*segment, "releases" | "archive"))?;
    let repo_segments = if keyword_idx > 0 && segments.get(keyword_idx - 1) == Some(&"-") {
        &segments[..keyword_idx - 1]
    } else {
        &segments[..keyword_idx]
    };
    if repo_segments.len() < 2 {
        return None;
    }

    Some(format!(
        "{}://{}/{}.git",
        parsed.scheme(),
        host,
        repo_segments
            .iter()
            .enumerate()
            .map(|(idx, segment)| {
                if idx + 1 == repo_segments.len() {
                    segment.strip_suffix(".git").unwrap_or(segment)
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    ))
}

fn source_git_url_parts(url: &str) -> Option<(String, String)> {
    if let Some((base, rev)) = url.split_once('#') {
        let lower = base.to_ascii_lowercase();
        let is_archive = lower.ends_with(".tar.gz")
            || lower.ends_with(".tgz")
            || lower.ends_with(".tar.xz")
            || lower.ends_with(".txz")
            || lower.ends_with(".tar.bz2")
            || lower.ends_with(".tbz2")
            || lower.ends_with(".zip")
            || lower.ends_with(".tar");
        if is_archive {
            return None;
        }
        let resolved_rev = if rev.trim().is_empty() { "HEAD" } else { rev };
        return Some((base.to_string(), resolved_rev.to_string()));
    }

    url.to_ascii_lowercase()
        .ends_with(".git")
        .then(|| (url.to_string(), "HEAD".to_string()))
}

pub(super) fn list_remote_refs(url: &str) -> Result<Vec<String>> {
    crate::interrupts::check()?;
    let mut remote = git2::Remote::create_detached(url)
        .with_context(|| format!("Failed to create detached git remote for {}", url))?;
    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.sideband_progress(|_| !crate::interrupts::was_interrupted());
    callbacks.transfer_progress(|_| !crate::interrupts::was_interrupted());

    let refs = {
        let connection = match remote.connect_auth(Direction::Fetch, Some(callbacks), None) {
            Ok(connection) => connection,
            Err(_err) if crate::interrupts::was_interrupted() => {
                anyhow::bail!("Interrupted by Ctrl-C while checking {}", url);
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to connect to git remote {}", url));
            }
        };
        crate::interrupts::check()?;
        let refs = connection
            .list()
            .with_context(|| format!("Failed to list refs for git remote {}", url))?
            .iter()
            .map(|head| head.name().trim_end_matches("^{}").to_string())
            .collect();
        drop(connection);
        refs
    };
    remote.disconnect().ok();
    Ok(refs)
}

pub(crate) fn candidate_versions_from_refs(
    refs: &[String],
    patterns: &[VersionPattern],
) -> Vec<String> {
    let mut versions = HashSet::new();

    for name in refs {
        let Some(short) = name.strip_prefix("refs/tags/") else {
            continue;
        };

        for pattern in patterns {
            if let Some(candidate) = match_version_pattern(short, pattern) {
                versions.insert(candidate.to_string());
            }
        }
    }

    let mut out: Vec<_> = versions.into_iter().collect();
    out.sort_by(|a, b| compare_versions_for_updates(a, b));
    out
}

pub(crate) fn archive_listing_probe(
    raw_url: &str,
    expanded_url: &str,
) -> Option<ArchiveListingProbe> {
    let raw = Url::parse(raw_url).ok()?;
    let expanded = Url::parse(expanded_url).ok()?;
    if !matches!(expanded.scheme(), "http" | "https") {
        return None;
    }

    let raw_segments: Vec<_> = raw.path_segments()?.collect();
    let expanded_segments: Vec<_> = expanded.path_segments()?.collect();
    if raw_segments.len() != expanded_segments.len() {
        return None;
    }

    let first_version_idx = raw_segments
        .iter()
        .position(|segment| segment.contains("$version"))?;
    let pattern = version_pattern_from_template(raw_segments[first_version_idx], false)?;

    let mut listing_url = expanded.clone();
    let listing_path = if first_version_idx == 0 {
        "/".to_string()
    } else {
        format!("/{}/", expanded_segments[..first_version_idx].join("/"))
    };
    listing_url.set_path(&listing_path);
    listing_url.set_query(None);
    listing_url.set_fragment(None);

    Some(ArchiveListingProbe {
        listing_url: listing_url.to_string(),
        patterns: vec![pattern],
    })
}

fn archive_listing_tokens(body: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    for token in body.split(|ch: char| {
        ch.is_ascii_whitespace() || matches!(ch, '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']')
    }) {
        let token = token
            .split_once('?')
            .map(|(value, _)| value)
            .unwrap_or(token)
            .split_once('#')
            .map(|(value, _)| value)
            .unwrap_or(token)
            .trim_matches(|ch: char| matches!(ch, ',' | ';' | '='))
            .trim_end_matches('/');
        if token.is_empty() {
            continue;
        }
        let basename = token.rsplit('/').next().unwrap_or(token);
        if !basename.is_empty() {
            tokens.insert(basename.to_string());
        }
    }
    tokens
}

pub(crate) fn candidate_versions_from_listing(
    body: &str,
    patterns: &[VersionPattern],
) -> Vec<String> {
    let mut versions = HashSet::new();
    for token in archive_listing_tokens(body) {
        for pattern in patterns {
            if let Some(candidate) = match_version_pattern(&token, pattern) {
                versions.insert(candidate.to_string());
            }
        }
    }

    let mut out: Vec<_> = versions.into_iter().collect();
    out.sort_by(|a, b| compare_versions_for_updates(a, b));
    out
}

pub(crate) fn list_archive_versions(probe: &ArchiveListingProbe) -> Result<Vec<String>> {
    crate::interrupts::check()?;
    let client = source::build_blocking_client(
        &format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        Some(Duration::from_secs(20)),
    )?;
    let response = client
        .get(&probe.listing_url)
        .send()
        .with_context(|| format!("Failed to fetch archive index {}", probe.listing_url))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "archive index {} returned {}",
            probe.listing_url,
            response.status()
        );
    }
    let body = response
        .text()
        .with_context(|| format!("Failed to read archive index {}", probe.listing_url))?;
    crate::interrupts::check()?;

    let candidates = candidate_versions_from_listing(&body, &probe.patterns);
    if candidates.is_empty() {
        anyhow::bail!("no matching archive entries found in {}", probe.listing_url);
    }
    Ok(candidates)
}
