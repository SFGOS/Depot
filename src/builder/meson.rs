//! Meson build system

use crate::cross::CrossConfig;
use crate::fakeroot;
use crate::package::PackageSpec;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
) -> Result<()> {
    let flags = &spec.build.flags;

    // Determine actual source directory (support source_subdir)
    let actual_src = resolve_actual_src(spec, src_dir)?;

    let build_dir = resolve_build_dir(&actual_src, flags);

    // Create directories
    fs::create_dir_all(&build_dir)?;
    fs::create_dir_all(destdir)?;

    // Environment variables
    let env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);

    // Generate cross file if cross-compiling, or when the lib32 variant needs
    // Meson to treat the build as x86 instead of the native x86_64 host.
    let cross_file = if let Some(cc_cfg) = cross {
        Some(cc_cfg.generate_meson_cross_file(&build_dir)?)
    } else if flags.lib32_variant {
        Some(generate_lib32_meson_cross_file(flags, &build_dir)?)
    } else {
        None
    };

    use crate::builder::state::{BuildStep, StateTracker};
    let mut state = StateTracker::new_with_namespace(
        &actual_src,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;

    // Run meson setup
    if !state.is_done(BuildStep::Configured) {
        crate::log_info!("Running meson setup...");
        let mut meson_cmd = Command::new("meson");
        meson_cmd.current_dir(&actual_src);
        meson_cmd.arg("setup");
        meson_cmd.arg(&build_dir);

        for arg in meson_setup_args(flags, cross_file.as_deref(), &env_vars) {
            meson_cmd.arg(arg);
        }

        crate::builder::prepare_tool_command(&mut meson_cmd, &env_vars);

        let status = crate::interrupts::command_status(&mut meson_cmd)
            .context("Failed to run meson setup")?;
        if !status.success() {
            anyhow::bail!("meson setup failed");
        }

        crate::source::hooks::run_post_configure_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    } else {
        crate::log_info!("Skipping meson setup (already done)");
    }

    if !state.is_done(BuildStep::PostCompileDone) {
        // Run ninja build
        crate::log_info!("Running ninja...");
        let mut ninja_cmd = Command::new("ninja");
        ninja_cmd.current_dir(&build_dir);
        ninja_cmd.arg("-j").arg(num_cpus().to_string());

        crate::builder::prepare_tool_command(&mut ninja_cmd, &env_vars);

        let status = crate::interrupts::command_status(&mut ninja_cmd)
            .with_context(|| format!("Failed to run ninja for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("ninja build failed");
        }

        if flags.skip_tests {
            crate::log_info!("Skipping tests: disabled by build.flags.skip_tests");
        } else {
            let test_suites = meson_test_suites(flags);
            if test_suites.is_empty() {
                crate::log_info!("Running meson test...");
            } else {
                crate::log_info!("Running meson test suite(s): {}...", test_suites.join(" "));
            }

            let mut test_cmd = Command::new("meson");
            test_cmd.current_dir(&build_dir);
            test_cmd.arg("test");
            test_cmd.arg("-C").arg(&build_dir);
            test_cmd.arg("--num-processes").arg(num_cpus().to_string());
            test_cmd.arg("--print-errorlogs");
            for suite in &test_suites {
                test_cmd.arg("--suite").arg(suite);
            }

            crate::builder::prepare_tool_command(&mut test_cmd, &env_vars);

            let status = crate::interrupts::command_status(&mut test_cmd)
                .with_context(|| format!("Failed to run meson test for {}", spec.package.name))?;
            if !status.success() {
                anyhow::bail!("meson test failed");
            }
        }

        crate::source::hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        crate::log_info!("Skipping ninja build and post-compile hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        // Run meson install with fakeroot if not root
        crate::log_info!(
            "Running meson install{}...",
            if fakeroot::is_root() {
                ""
            } else {
                " (with fakeroot)"
            }
        );

        let mut install_cmd = fakeroot::wrap_install_command("meson", destdir);
        install_cmd.arg("install");
        install_cmd.arg("-C").arg(&build_dir);

        let mut install_env = env_vars.clone();
        install_env.push((
            "DESTDIR".to_string(),
            destdir.to_string_lossy().into_owned(),
        ));
        crate::builder::prepare_tool_command(&mut install_cmd, &install_env);

        let status = crate::interrupts::command_status(&mut install_cmd)
            .with_context(|| format!("Failed to run meson install for {}", spec.package.name))?;
        if !status.success() {
            anyhow::bail!("meson install failed");
        }

        crate::source::hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping meson install and post-install hooks (already done)");
    }

    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn resolve_build_dir(actual_src: &Path, flags: &crate::package::BuildFlags) -> PathBuf {
    if let Some(dir) = flags
        .build_dir
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        actual_src.join(dir)
    } else {
        actual_src.join("builddir")
    }
}

fn meson_test_suites(flags: &crate::package::BuildFlags) -> Vec<String> {
    let mut suites = Vec::new();
    let single = flags.make_test_target.trim();
    if !single.is_empty() {
        suites.push(single.to_string());
    }
    for suite in &flags.make_test_targets {
        let trimmed = suite.trim();
        if !trimmed.is_empty() {
            suites.push(trimmed.to_string());
        }
    }
    suites
}

fn has_option(configure: &[String], long: &str) -> bool {
    let prefix = format!("{long}=");
    for arg in configure {
        if arg == long || arg.starts_with(&prefix) {
            return true;
        }
    }
    false
}

fn has_builtin_option(configure: &[String], key: &str) -> bool {
    let prefix = format!("-D{key}=");
    configure.iter().any(|arg| arg.starts_with(&prefix))
}

fn meson_setup_args(
    flags: &crate::package::BuildFlags,
    cross_file: Option<&Path>,
    env_vars: &[(String, String)],
) -> Vec<String> {
    let mut args = Vec::new();
    let dirs = crate::builder::install_dirs(flags);

    if !has_option(&flags.configure, "--prefix") {
        args.push(format!("--prefix={}", flags.prefix));
    }
    for (option, value) in [
        ("--bindir", dirs.bindir),
        ("--sbindir", dirs.sbindir),
        ("--libdir", dirs.libdir),
        ("--libexecdir", dirs.libexecdir),
        ("--sysconfdir", dirs.sysconfdir),
        ("--localstatedir", dirs.localstatedir),
        ("--sharedstatedir", dirs.sharedstatedir),
        ("--includedir", dirs.includedir),
        ("--datadir", dirs.datadir),
        ("--mandir", dirs.mandir),
        ("--infodir", dirs.infodir),
    ] {
        if !has_option(&flags.configure, option) {
            args.push(format!("{option}={value}"));
        }
    }
    if !has_option(&flags.configure, "--buildtype") {
        args.push("--buildtype=release".to_string());
    }

    if let Some(cf) = cross_file {
        args.push(format!("--cross-file={}", cf.display()));
    }
    if !flags.ld.trim().is_empty() {
        if !has_builtin_option(&flags.configure, "c_ld") {
            args.push(format!("-Dc_ld={}", flags.ld));
        }
        if !has_builtin_option(&flags.configure, "cpp_ld") {
            args.push(format!("-Dcpp_ld={}", flags.ld));
        }
    }

    // Append user flags last so they can override defaults when Meson allows it.
    for arg in &flags.configure {
        args.push(expand_with_envs(arg, env_vars));
    }

    args
}

fn generate_lib32_meson_cross_file(
    flags: &crate::package::BuildFlags,
    build_dir: &Path,
) -> Result<PathBuf> {
    let target = lib32_target_triple(flags);
    let arch = crate::cross::target_arch_from_triple(&target);
    let cpu_family = crate::cross::cpu_family_for_arch(arch);
    let c = meson_binary_value(
        &compiler_command_with_lib32_target(&flags.cc, &target),
        "C compiler",
    )?;
    let cpp = meson_binary_value(
        &compiler_command_with_lib32_target(&flags.cxx, &target),
        "C++ compiler",
    )?;
    let ar = meson_binary_value(&command_words(&flags.ar), "archiver")?;

    let mut content = format!(
        "# Meson cross file for lib32 builds\n# Generated by depot for target: {target}\n\n[binaries]\nc = {c}\ncpp = {cpp}\nar = {ar}\n"
    );
    if !flags.ld.trim().is_empty() {
        let ld = meson_binary_value(&command_words(&flags.ld), "linker")?;
        content.push_str(&format!("ld = {ld}\n"));
    }
    content.push_str(&format!(
        "\n[host_machine]\nsystem = 'linux'\ncpu_family = '{cpu_family}'\ncpu = '{arch}'\nendian = 'little'\n"
    ));

    fs::create_dir_all(build_dir)?;
    let cross_path = build_dir.join("lib32-cross-file.ini");
    fs::write(&cross_path, content)
        .with_context(|| format!("Failed to write {}", cross_path.display()))?;
    Ok(cross_path)
}

fn lib32_target_triple(flags: &crate::package::BuildFlags) -> String {
    let host = if !flags.chost.trim().is_empty() {
        flags.chost.trim().to_string()
    } else {
        match CrossConfig::build_triple() {
            Ok(triple) => triple,
            Err(err) => {
                crate::log_warn!(
                    "Failed to detect native build triple for lib32 Meson target file: {}",
                    err
                );
                "x86_64-unknown-linux-gnu".to_string()
            }
        }
    };
    crate::cross::lib32_target_triple(&host)
}

fn compiler_command_with_lib32_target(command: &str, target: &str) -> Vec<String> {
    let mut parts = command_words(command);
    if compiler_command_supports_target(&parts) && !compiler_command_has_target(&parts) {
        parts.push(format!("--target={target}"));
    }
    parts
}

fn command_words(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn compiler_command_supports_target(parts: &[String]) -> bool {
    parts.first().is_some_and(|tool| {
        Path::new(tool)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("clang"))
    })
}

fn compiler_command_has_target(parts: &[String]) -> bool {
    parts.iter().any(|part| {
        part == "--target"
            || part == "-target"
            || part.starts_with("--target=")
            || part.starts_with("-target=")
    })
}

fn meson_binary_value(parts: &[String], label: &str) -> Result<String> {
    if parts.is_empty() {
        anyhow::bail!("Missing {} command for lib32 Meson cross file", label);
    }

    let rendered = parts
        .iter()
        .map(|part| format!("'{}'", part.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect::<Vec<_>>();
    if rendered.len() == 1 {
        Ok(rendered[0].clone())
    } else {
        Ok(format!("[{}]", rendered.join(", ")))
    }
}

/// Expand environment variables in a string (e.g., $DEPOT_SYSROOT)
fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    // Simple expansion for $VAR and ${VAR} patterns using process environment only
    for (key, value) in std::env::vars() {
        result = result.replace(&format!("${key}"), &value);
        result = result.replace(&format!("${{{key}}}"), &value);
    }
    result
}

/// Expand using a provided set of env vars (used to expand flags before spawning child).
fn expand_with_envs(input: &str, envs: &[(String, String)]) -> String {
    let mut result = input.to_string();
    for (k, v) in envs {
        result = result.replace(&format!("${k}"), v);
        result = result.replace(&format!("${{{k}}}"), v);
    }
    expand_env_vars(&result)
}

/// Resolve `source_subdir` with multiple fallbacks:
/// - empty -> use `src_dir`
/// - absolute path -> use if exists
/// - `src_dir/<sub>` -> use if exists
/// - `spec.spec_dir/<sub>` -> use if exists
/// - bare relative path (cwd)
fn resolve_actual_src(spec: &crate::package::PackageSpec, src_dir: &Path) -> Result<PathBuf> {
    let source_subdir = spec.expand_vars(&spec.build.flags.source_subdir);
    if source_subdir.is_empty() {
        return Ok(src_dir.to_path_buf());
    }

    let candidate = Path::new(&source_subdir);
    if candidate.is_absolute() {
        if candidate.exists() {
            return Ok(candidate.to_path_buf());
        }
        bail!(
            "Source directory not found: {} (source_subdir: {} -> {})",
            candidate.display(),
            spec.build.flags.source_subdir,
            source_subdir
        );
    }

    let under_src = src_dir.join(&source_subdir);
    if under_src.exists() {
        return Ok(under_src);
    }

    let under_spec = spec.spec_dir.join(&source_subdir);
    if under_spec.exists() {
        return Ok(under_spec);
    }

    if candidate.exists() {
        return Ok(candidate.to_path_buf());
    }

    bail!(
        "Source directory not found: {} (expanded from '{}'; tried src_dir, spec_dir, and absolute path)",
        source_subdir,
        spec.build.flags.source_subdir
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, Source};
    use tempfile::tempdir;

    #[test]
    fn test_num_cpus_at_least_one() {
        let n = num_cpus();
        assert!(n >= 1);
    }

    #[test]
    fn test_meson_setup_args_include_configure_flags() {
        let mut flags = BuildFlags {
            prefix: "/usr".to_string(),
            ..BuildFlags::default()
        };
        flags.configure = vec!["-Dmanpages=false".to_string()];

        let args = meson_setup_args(&flags, None, &[]);
        assert!(args.iter().any(|a| a == "-Dmanpages=false"));
        assert!(args.iter().any(|a| a == "--prefix=/usr"));
        assert!(args.iter().any(|a| a == "--buildtype=release"));
    }

    #[test]
    fn test_meson_setup_args_include_install_dirs() {
        let args = meson_setup_args(&BuildFlags::default(), None, &[]);
        assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
        assert!(args.iter().any(|a| a == "--sbindir=/usr/bin"));
        assert!(args.iter().any(|a| a == "--libdir=/usr/lib"));
        assert!(args.iter().any(|a| a == "--libexecdir=/usr/lib"));
        assert!(args.iter().any(|a| a == "--sysconfdir=/etc"));
        assert!(args.iter().any(|a| a == "--localstatedir=/var"));
        assert!(args.iter().any(|a| a == "--sharedstatedir=/var/lib"));
        assert!(args.iter().any(|a| a == "--includedir=/usr/include"));
        assert!(args.iter().any(|a| a == "--datadir=/usr/share"));
        assert!(args.iter().any(|a| a == "--mandir=/usr/share/man"));
        assert!(args.iter().any(|a| a == "--infodir=/usr/share/info"));
    }

    #[test]
    fn test_meson_setup_args_derive_dirs_from_datarootdir() {
        let flags = BuildFlags {
            datarootdir: "/opt/share-root".to_string(),
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert!(!args.iter().any(|a| a.starts_with("--datarootdir=")));
        assert!(args.iter().any(|a| a == "--datadir=/opt/share-root"));
        assert!(args.iter().any(|a| a == "--mandir=/opt/share-root/man"));
        assert!(args.iter().any(|a| a == "--infodir=/opt/share-root/info"));
    }

    #[test]
    fn test_meson_setup_args_honor_explicit_prefix() {
        let flags = BuildFlags {
            prefix: "/usr".to_string(),
            configure: vec!["--prefix=/opt".to_string()],
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert_eq!(args.iter().filter(|a| a.starts_with("--prefix")).count(), 1);
        assert!(args.iter().any(|a| a == "--prefix=/opt"));
    }

    #[test]
    fn test_meson_setup_args_honor_explicit_install_dirs() {
        let flags = BuildFlags {
            configure: vec![
                "--sbindir=/sbin".to_string(),
                "--libdir=/custom/lib".to_string(),
                "--datadir=/custom/share".to_string(),
            ],
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert!(!args.iter().any(|a| a == "--sbindir=/usr/bin"));
        assert!(!args.iter().any(|a| a == "--libdir=/usr/lib"));
        assert!(!args.iter().any(|a| a == "--datadir=/usr/share"));
        assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
    }

    #[test]
    fn test_meson_setup_args_include_linker_override() {
        let flags = BuildFlags {
            ld: "ld.lld".to_string(),
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert!(args.iter().any(|a| a == "-Dc_ld=ld.lld"));
        assert!(args.iter().any(|a| a == "-Dcpp_ld=ld.lld"));
    }

    #[test]
    fn test_meson_setup_args_honor_explicit_linker_override() {
        let flags = BuildFlags {
            ld: "ld.lld".to_string(),
            configure: vec!["-Dc_ld=gold".to_string(), "-Dcpp_ld=gold".to_string()],
            ..BuildFlags::default()
        };

        let args = meson_setup_args(&flags, None, &[]);
        assert_eq!(args.iter().filter(|a| *a == "-Dc_ld=gold").count(), 1);
        assert_eq!(args.iter().filter(|a| *a == "-Dcpp_ld=gold").count(), 1);
        assert!(!args.iter().any(|a| a == "-Dc_ld=ld.lld"));
        assert!(!args.iter().any(|a| a == "-Dcpp_ld=ld.lld"));
    }

    #[test]
    fn test_compiler_command_with_lib32_target_adds_clang_target() {
        let parts = compiler_command_with_lib32_target("clang -m32", "i686-sfg-linux-gnu");
        assert_eq!(
            parts,
            vec![
                "clang".to_string(),
                "-m32".to_string(),
                "--target=i686-sfg-linux-gnu".to_string()
            ]
        );
    }

    #[test]
    fn test_compiler_command_with_lib32_target_skips_non_clang_compilers() {
        let parts = compiler_command_with_lib32_target("gcc -m32", "i686-sfg-linux-gnu");
        assert_eq!(parts, vec!["gcc".to_string(), "-m32".to_string()]);
    }

    #[test]
    fn test_generate_lib32_meson_cross_file_sets_x86_host_machine() -> Result<()> {
        let tmp = tempdir()?;
        let flags = BuildFlags {
            lib32_variant: true,
            chost: "x86_64-sfg-linux-gnu".to_string(),
            cc: "clang -m32".to_string(),
            cxx: "clang++ -m32".to_string(),
            ar: "llvm-ar".to_string(),
            ld: "ld.lld".to_string(),
            ..BuildFlags::default()
        };

        let path = generate_lib32_meson_cross_file(&flags, tmp.path())?;
        let content = fs::read_to_string(path)?;
        assert!(content.contains("Generated by depot for target: i686-sfg-linux-gnu"));
        assert!(content.contains("c = ['clang', '-m32', '--target=i686-sfg-linux-gnu']"));
        assert!(content.contains("cpp = ['clang++', '-m32', '--target=i686-sfg-linux-gnu']"));
        assert!(content.contains("cpu_family = 'x86'"));
        assert!(content.contains("cpu = 'i686'"));
        Ok(())
    }

    #[test]
    fn test_resolve_build_dir_uses_flag() {
        let flags = BuildFlags {
            build_dir: Some("build".to_string()),
            ..BuildFlags::default()
        };
        let src = Path::new("/tmp/src");
        assert_eq!(
            resolve_build_dir(src, &flags),
            PathBuf::from("/tmp/src/build")
        );
    }

    #[test]
    fn test_meson_test_suites_uses_single_and_multiple_targets() {
        let flags = BuildFlags {
            make_test_target: "unit".to_string(),
            make_test_targets: vec!["integration".to_string(), " smoke ".to_string()],
            ..BuildFlags::default()
        };
        assert_eq!(
            meson_test_suites(&flags),
            vec![
                "unit".to_string(),
                "integration".to_string(),
                "smoke".to_string()
            ]
        );
    }

    #[test]
    fn test_meson_test_suites_empty_without_targets() {
        assert!(meson_test_suites(&BuildFlags::default()).is_empty());
    }

    #[test]
    fn test_resolve_actual_src_uses_source_subdir_under_source() -> Result<()> {
        let src = tempdir()?;
        let spec_dir = tempdir()?;
        fs::create_dir_all(src.path().join("sub"))?;

        let spec = PackageSpec {
            package: PackageInfo {
                name: "pkg".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "u".into(),
                sha256: "s".into(),
                extract_dir: "e".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Meson,
                flags: BuildFlags {
                    source_subdir: "sub".into(),
                    ..BuildFlags::default()
                },
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: spec_dir.path().to_path_buf(),
        };

        let resolved = resolve_actual_src(&spec, src.path())?;
        assert_eq!(resolved, src.path().join("sub"));
        Ok(())
    }
}
