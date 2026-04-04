//! Post-extraction hooks: apply patches and run commands

use crate::package::{PackageSpec, Source};
use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::builder::state::{BuildStep, StateTracker};

fn hook_env_vars(
    spec: &PackageSpec,
    shell_helpers: &crate::shell_helpers::ShellHelpers,
    source_dir: &Path,
) -> Result<crate::builder::EnvVars> {
    let mut env_vars = crate::builder::standard_build_env(spec, None, true, true);
    shell_helpers.apply_to_env_vars(&mut env_vars);
    crate::builder::apply_build_helper_context_env(&mut env_vars, spec)?;
    crate::builder::apply_build_helper_dirs_env(&mut env_vars, Some(source_dir), None);
    Ok(env_vars)
}

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

    crate::log_info!("Applying {} patch(es)...", source.patches.len());

    for p in &source.patches {
        let p = spec.expand_vars(p);
        let patch_path = resolve_patch_path(spec, &p, &patch_cache_dir)?;

        crate::log_info!("  patch: {}", patch_path.display());

        // Apply with patch(1). Keep it simple: -p1 is the common case.
        let mut patch_cmd = Command::new("patch");
        patch_cmd.current_dir(src_dir);
        patch_cmd.env("PATH", crate::runtime_env::safe_script_path());
        patch_cmd.arg("-p1");
        patch_cmd.arg("-i");
        patch_cmd.arg(&patch_path);
        let status = crate::interrupts::command_status(&mut patch_cmd)
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

    crate::log_info!(
        "Running {} post-extract command(s)...",
        source.post_extract.len()
    );
    let helper_root = tempfile::tempdir().context("Failed to create post-extract helper root")?;
    let shell_helpers = crate::shell_helpers::ShellHelpers::new(helper_root.path())?;
    let env_vars = hook_env_vars(spec, &shell_helpers, src_dir)?;

    for cmd in &source.post_extract {
        let cmd_str = spec.expand_vars(cmd);
        crate::log_info!("  post_extract: {}", cmd_str);
        let wrapped_cmd = crate::shell_helpers::wrap_shell_command(&cmd_str);

        // Use a shell for convenience; this is a package manager, so specs are trusted input.
        let mut shell_cmd = Command::new("sh");
        shell_cmd.current_dir(src_dir);
        crate::builder::prepare_command(&mut shell_cmd, &env_vars);
        shell_cmd.arg("-c").arg(&wrapped_cmd);
        let status = crate::interrupts::command_status(&mut shell_cmd)
            .with_context(|| format!("Failed to run post_extract command: {}", cmd_str))?;

        if !status.success() {
            bail!("post_extract command failed: {}", cmd_str);
        }
    }

    Ok(())
}

/// Run post-compile commands (after make, before make install).
pub fn run_post_configure_commands(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
) -> Result<()> {
    let commands = &spec.build.flags.post_configure;
    if commands.is_empty() {
        return Ok(());
    }

    crate::log_info!("Running {} post-configure command(s)...", commands.len());
    let shell_helpers = crate::shell_helpers::ShellHelpers::new(destdir)?;
    let mut env_vars = hook_env_vars(spec, &shell_helpers, src_dir)?;
    crate::builder::set_env_var(
        &mut env_vars,
        "DESTDIR",
        destdir.to_string_lossy().into_owned(),
    );

    for cmd in commands {
        let cmd_str = spec.expand_vars(cmd);
        crate::log_info!("  post_configure: {}", cmd_str);
        let wrapped_cmd = crate::shell_helpers::wrap_shell_command(&cmd_str);

        let mut shell_cmd = Command::new("sh");
        shell_cmd.current_dir(src_dir);
        crate::builder::prepare_command(&mut shell_cmd, &env_vars);
        shell_cmd.arg("-c").arg(&wrapped_cmd);
        let status = crate::interrupts::command_status(&mut shell_cmd)
            .with_context(|| format!("Failed to run post_configure command: {}", cmd_str))?;

        if !status.success() {
            bail!("post_configure command failed: {}", cmd_str);
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

    crate::log_info!("Running {} post-compile command(s)...", commands.len());
    let shell_helpers = crate::shell_helpers::ShellHelpers::new(destdir)?;
    let mut env_vars = hook_env_vars(spec, &shell_helpers, src_dir)?;
    crate::builder::set_env_var(
        &mut env_vars,
        "DESTDIR",
        destdir.to_string_lossy().into_owned(),
    );

    for cmd in commands {
        let cmd_str = spec.expand_vars(cmd);
        crate::log_info!("  post_compile: {}", cmd_str);
        let wrapped_cmd = crate::shell_helpers::wrap_shell_command(&cmd_str);

        let mut shell_cmd = Command::new("sh");
        shell_cmd.current_dir(src_dir);
        crate::builder::prepare_command(&mut shell_cmd, &env_vars);
        shell_cmd.arg("-c").arg(&wrapped_cmd);
        let status = crate::interrupts::command_status(&mut shell_cmd)
            .with_context(|| format!("Failed to run post_compile command: {}", cmd_str))?;

        if !status.success() {
            bail!("post_compile command failed: {}", cmd_str);
        }
    }

    Ok(())
}

/// Run post-install commands (after make install) from the provided working directory.
pub fn run_post_install_commands_in_dir(
    spec: &PackageSpec,
    work_dir: &Path,
    destdir: &Path,
) -> Result<()> {
    let commands = &spec.build.flags.post_install;
    if commands.is_empty() {
        return Ok(());
    }

    crate::log_info!("Running {} post-install command(s)...", commands.len());
    let shell_helpers = crate::shell_helpers::ShellHelpers::new(destdir)?;
    let mut env_vars = hook_env_vars(spec, &shell_helpers, work_dir)?;
    crate::builder::set_env_var(
        &mut env_vars,
        "DESTDIR",
        destdir.to_string_lossy().into_owned(),
    );

    for cmd in commands {
        let cmd_str = spec.expand_vars(cmd);
        crate::log_info!("  post_install: {}", cmd_str);
        let wrapped_cmd = crate::shell_helpers::wrap_shell_command(&cmd_str);

        let mut shell_cmd = Command::new("sh");
        shell_cmd.current_dir(work_dir);
        crate::builder::prepare_command(&mut shell_cmd, &env_vars);
        shell_cmd.arg("-c").arg(&wrapped_cmd);
        let status = crate::interrupts::command_status(&mut shell_cmd)
            .with_context(|| format!("Failed to run post_install command: {}", cmd_str))?;

        if !status.success() {
            bail!("post_install command failed: {}", cmd_str);
        }
    }

    Ok(())
}

/// Run post-install commands (after make install).
pub fn run_post_install_commands(spec: &PackageSpec, src_dir: &Path, destdir: &Path) -> Result<()> {
    run_post_install_commands_in_dir(spec, src_dir, destdir)
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
    use super::{
        resolve_patch_path, run_post_extract_commands, run_post_install_commands,
        run_post_install_commands_in_dir,
    };
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
    };

    fn dummy_spec(spec_dir: &std::path::Path) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
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
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
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

    #[test]
    fn post_install_commands_can_haul_into_output_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let src_dir = tmp.path().join("src");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(destdir.join("usr/lib")).unwrap();
        std::fs::write(destdir.join("usr/lib/libLLVM.so.1"), "llvm").unwrap();

        let mut spec = dummy_spec(&spec_dir);
        spec.build.flags.post_install = vec!["haul llvm-libs 'usr/lib/libLLVM*.so*'".into()];

        run_post_install_commands(&spec, &src_dir, &destdir).unwrap();

        assert!(!destdir.join("usr/lib/libLLVM.so.1").exists());
        assert!(
            destdir
                .join(".depot/outputs/llvm-libs/usr/lib/libLLVM.so.1")
                .exists()
        );
    }

    #[test]
    fn post_install_commands_can_run_from_build_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let src_dir = tmp.path().join("src");
        let build_dir = tmp.path().join("build");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(build_dir.join("destdir/usr/include/gnu")).unwrap();
        std::fs::create_dir_all(destdir.join("usr/include/gnu")).unwrap();
        std::fs::write(
            build_dir.join("destdir/usr/include/gnu/lib-names-32.h"),
            "lib32",
        )
        .unwrap();

        let mut spec = dummy_spec(&spec_dir);
        spec.build.flags.post_install = vec![
            "install -m644 destdir/usr/include/gnu/lib-names-32.h \"$DESTDIR/usr/include/gnu/\""
                .into(),
        ];

        run_post_install_commands_in_dir(&spec, &build_dir, &destdir).unwrap();

        assert!(destdir.join("usr/include/gnu/lib-names-32.h").exists());
        assert!(
            !src_dir
                .join("destdir/usr/include/gnu/lib-names-32.h")
                .exists()
        );
    }

    #[test]
    fn post_extract_commands_can_call_python_build_helper() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_dir = tmp.path().join("spec");
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::create_dir_all(&src_dir).unwrap();

        let fake_depot = tmp.path().join("fake-depot");
        let log_path = tmp.path().join("python-build.log");
        std::fs::write(
            &fake_depot,
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > '{}'\n",
                log_path.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake_depot).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_depot, perms).unwrap();
        }

        let spec = dummy_spec(&spec_dir);
        let source = Source {
            url: "https://example.com/foo.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo".into(),
            patches: Vec::new(),
            post_extract: vec![format!(
                "DEPOT_EXECUTABLE='{}' python_build --src-dir . --dist-dir dist",
                fake_depot.display()
            )],
            cherry_pick: Vec::new(),
        };

        run_post_extract_commands(&spec, &source, &src_dir).unwrap();

        let logged = std::fs::read_to_string(&log_path).unwrap();
        assert!(logged.contains("internal"));
        assert!(logged.contains("python-build"));
        assert!(logged.contains("--src-dir"));
        assert!(logged.contains("--dist-dir"));
    }
}
