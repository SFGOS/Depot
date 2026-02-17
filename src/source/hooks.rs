//! Post-extraction hooks: apply patches and run commands

use crate::package::{PackageSpec, Source};
use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::builder::state::{BuildStep, StateTracker};

/// Apply patches and run `post_extract` commands in the extracted source tree.
pub fn post_extract(
    spec: &PackageSpec,
    source: &Source,
    src_dir: &Path,
    cache_dir: &Path,
) -> Result<()> {
    let mut state = StateTracker::new(src_dir)?;

    if !state.is_done(BuildStep::PatchesApplied) {
        apply_patches(spec, source, src_dir, cache_dir)?;
        state.mark_done(BuildStep::PatchesApplied)?;
    }

    if !state.is_done(BuildStep::PostExtractDone) {
        run_post_extract_commands(spec, source, src_dir)?;
        state.mark_done(BuildStep::PostExtractDone)?;
    }

    Ok(())
}

fn apply_patches(
    spec: &PackageSpec,
    source: &Source,
    src_dir: &Path,
    cache_dir: &Path,
) -> Result<()> {
    if source.patches.is_empty() {
        return Ok(());
    }

    let patch_cache_dir = cache_dir.join("patches").join(&spec.package.name);
    fs::create_dir_all(&patch_cache_dir).with_context(|| {
        format!(
            "Failed to create patch cache dir: {}",
            patch_cache_dir.display()
        )
    })?;

    println!("Applying {} patch(es)...", source.patches.len());

    for p in &source.patches {
        let p = spec.expand_vars(p);
        let patch_path = resolve_patch_path(spec, &p, &patch_cache_dir)?;

        println!("  patch: {}", patch_path.display());

        // Apply with patch(1). Keep it simple: -p1 is the common case.
        let status = Command::new("patch")
            .current_dir(src_dir)
            .arg("-p1")
            .arg("-i")
            .arg(&patch_path)
            .status()
            .with_context(|| format!("Failed to execute patch for {}", patch_path.display()))?;

        if !status.success() {
            bail!("Patch failed: {}", patch_path.display());
        }
    }

    Ok(())
}

fn run_post_extract_commands(spec: &PackageSpec, source: &Source, src_dir: &Path) -> Result<()> {
    if source.post_extract.is_empty() {
        return Ok(());
    }

    println!(
        "Running {} post-extract command(s)...",
        source.post_extract.len()
    );

    for cmd in &source.post_extract {
        let cmd = spec.expand_vars(cmd);
        println!("  post_extract: {}", cmd);

        // Use a shell for convenience; this is a package manager, so specs are trusted input.
        let status = Command::new("sh")
            .current_dir(src_dir)
            .env("DEPOT_SPECDIR", &spec.spec_dir)
            .arg("-c")
            .arg(&cmd)
            .status()
            .with_context(|| format!("Failed to run post_extract command: {}", cmd))?;

        if !status.success() {
            bail!("post_extract command failed: {}", cmd);
        }
    }

    Ok(())
}

/// Run post-compile commands (after make, before make install).
pub fn run_post_compile_commands(spec: &PackageSpec, src_dir: &Path, destdir: &Path) -> Result<()> {
    let commands = &spec.build.flags.post_compile;
    if commands.is_empty() {
        return Ok(());
    }

    println!("Running {} post-compile command(s)...", commands.len());

    for cmd in commands {
        let cmd = spec.expand_vars(cmd);
        println!("  post_compile: {}", cmd);

        let status = Command::new("sh")
            .current_dir(src_dir)
            .env("DEPOT_SPECDIR", &spec.spec_dir)
            .env("DESTDIR", destdir)
            .env("DEPOT_ROOTFS", &spec.build.flags.rootfs)
            .env("CC", &spec.build.flags.cc)
            .env("AR", &spec.build.flags.ar)
            .arg("-c")
            .arg(&cmd)
            .status()
            .with_context(|| format!("Failed to run post_compile command: {}", cmd))?;

        if !status.success() {
            bail!("post_compile command failed: {}", cmd);
        }
    }

    Ok(())
}

/// Run post-install commands (after make install).
pub fn run_post_install_commands(spec: &PackageSpec, src_dir: &Path, destdir: &Path) -> Result<()> {
    let commands = &spec.build.flags.post_install;
    if commands.is_empty() {
        return Ok(());
    }

    println!("Running {} post-install command(s)...", commands.len());

    for cmd in commands {
        let cmd = spec.expand_vars(cmd);
        println!("  post_install: {}", cmd);

        let status = Command::new("sh")
            .current_dir(src_dir)
            .env("DEPOT_SPECDIR", &spec.spec_dir)
            .env("DESTDIR", destdir)
            .env("DEPOT_ROOTFS", &spec.build.flags.rootfs)
            .env("CC", &spec.build.flags.cc)
            .env("AR", &spec.build.flags.ar)
            .arg("-c")
            .arg(&cmd)
            .status()
            .with_context(|| format!("Failed to run post_install command: {}", cmd))?;

        if !status.success() {
            bail!("post_install command failed: {}", cmd);
        }
    }

    Ok(())
}

fn resolve_patch_path(spec: &PackageSpec, patch: &str, patch_cache_dir: &Path) -> Result<PathBuf> {
    if patch.starts_with("http://") || patch.starts_with("https://") {
        let filename = patch
            .split('/')
            .rfind(|s| !s.is_empty())
            .unwrap_or("patch.patch");
        let dest = patch_cache_dir.join(filename);
        if dest.exists() {
            return Ok(dest);
        }
        download(patch, &dest).with_context(|| format!("Failed to download patch: {}", patch))?;
        return Ok(dest);
    }

    // Treat as local path, relative to the spec file.
    let local = spec.spec_dir.join(patch);
    if !local.exists() {
        bail!(
            "Patch not found: {} (resolved to {})",
            patch,
            local.display()
        );
    }
    Ok(local)
}

fn download(url: &str, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let client = reqwest::blocking::Client::new();
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("Failed to fetch: {}", url))?;

    let total_size = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
            .unwrap()
            .progress_chars("#>-")
    );

    let mut file =
        fs::File::create(dest).with_context(|| format!("Failed to create: {}", dest.display()))?;

    let mut buffer = [0u8; 8192];
    let mut downloaded = 0u64;

    loop {
        let bytes_read = response.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        file.write_all(&buffer[..bytes_read])?;
        downloaded += bytes_read as u64;
        pb.set_position(downloaded);
    }

    pb.finish_with_message("Patch download complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_patch_path;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
    };

    fn dummy_spec(spec_dir: &std::path::Path) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: "MIT".into(),
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            spec_dir: spec_dir.to_path_buf(),
        }
    }

    #[test]
    fn resolve_patch_relative_to_spec_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        std::fs::create_dir_all(&spec_dir).unwrap();
        let patch_path = spec_dir.join("fix.patch");
        std::fs::write(&patch_path, "diff --git a/a b/a\n").unwrap();

        let spec = dummy_spec(&spec_dir);
        let cache = tmp.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let resolved = resolve_patch_path(&spec, "fix.patch", &cache).unwrap();
        assert_eq!(resolved, patch_path);
    }

    #[test]
    fn resolve_patch_url_uses_cached_file_if_present() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        std::fs::create_dir_all(&spec_dir).unwrap();
        let spec = dummy_spec(&spec_dir);

        let cache = tmp.path().join("cache");
        let patch_cache_dir = cache.join("patches").join("foo");
        std::fs::create_dir_all(&patch_cache_dir).unwrap();

        // URL-derived filename is the last segment.
        let cached = patch_cache_dir.join("fix.patch");
        std::fs::write(&cached, "dummy").unwrap();

        let url = "https://example.com/fix.patch";
        let resolved = resolve_patch_path(&spec, url, &patch_cache_dir).unwrap();
        assert_eq!(resolved, cached);
    }
}
