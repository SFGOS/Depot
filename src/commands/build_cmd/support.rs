use super::*;

pub(crate) fn build_type_runs_automatic_tests(spec: &package::PackageSpec) -> bool {
    matches!(
        spec.build.build_type,
        package::BuildType::Autotools
            | package::BuildType::CMake
            | package::BuildType::Meson
            | package::BuildType::Perl
    )
}

pub(crate) fn automatic_tests_disabled_for_outputs(
    pkg_spec: &package::PackageSpec,
    requested_outputs: deps::RequestedOutputs,
) -> bool {
    pkg_spec.should_skip_automatic_tests() || requested_outputs.includes_lib32()
}

pub(crate) fn maybe_disable_tests_for_missing_deps(
    pkg_spec: &mut package::PackageSpec,
    db_path: &Path,
    requested_outputs: deps::RequestedOutputs,
) -> Result<()> {
    if automatic_tests_disabled_for_outputs(pkg_spec, requested_outputs)
        || !build_type_runs_automatic_tests(pkg_spec)
        || deps::declared_test_deps(pkg_spec, requested_outputs).is_empty()
    {
        return Ok(());
    }

    let missing_test = deps::check_test_deps_for_outputs(pkg_spec, db_path, requested_outputs)?;
    if !missing_test.is_empty() {
        ui::warn(format!(
            "Missing test dependencies: {}. Tests will be skipped.",
            missing_test.join(", ")
        ));
        pkg_spec.build.flags.skip_tests = true;
    }

    Ok(())
}

pub(crate) fn maybe_prompt_to_skip_tests_for_missing_requested_deps(
    pkg_spec: &mut package::PackageSpec,
    missing_test: &[String],
    reason: &str,
) -> Result<bool> {
    if pkg_spec.should_skip_automatic_tests()
        || !build_type_runs_automatic_tests(pkg_spec)
        || missing_test.is_empty()
    {
        return Ok(false);
    }

    ui::warn(format!("{reason}: {}", missing_test.join(", ")));
    if ui::prompt_yes_no("Continue without tests?", false)? {
        pkg_spec.build.flags.skip_tests = true;
        ui::warn("Tests will be skipped for this build.");
        return Ok(true);
    }

    Ok(false)
}

pub(crate) fn requested_outputs(
    pkg_spec: &package::PackageSpec,
    lib32_only: bool,
) -> deps::RequestedOutputs {
    if effective_lib32_only(pkg_spec, lib32_only) {
        deps::RequestedOutputs::Lib32Only
    } else if pkg_spec.builds_lib32_output() {
        deps::RequestedOutputs::PrimaryAndLib32
    } else {
        deps::RequestedOutputs::PrimaryOnly
    }
}

pub(crate) fn effective_lib32_only(pkg_spec: &package::PackageSpec, cli_lib32_only: bool) -> bool {
    cli_lib32_only || pkg_spec.builds_only_lib32_output()
}

pub(crate) fn should_install_test_deps(
    pkg_spec: &package::PackageSpec,
    install_test_deps: bool,
    requested_outputs: deps::RequestedOutputs,
) -> bool {
    install_test_deps
        && !automatic_tests_disabled_for_outputs(pkg_spec, requested_outputs)
        && !deps::declared_test_deps(pkg_spec, requested_outputs).is_empty()
}

pub(crate) fn clean_build_workspace(config: &config::Config) -> Result<()> {
    if config.build_dir.exists() {
        fs::remove_dir_all(&config.build_dir).with_context(|| {
            format!("Failed to clean build dir: {}", config.build_dir.display())
        })?;
        ui::success(format!(
            "Cleaned build workspace: {}",
            config.build_dir.display()
        ));
    }
    if config.cache_dir.exists() {
        fs::remove_dir_all(&config.cache_dir).with_context(|| {
            format!(
                "Failed to clean source cache dir: {}",
                config.cache_dir.display()
            )
        })?;
        ui::success(format!(
            "Cleaned source cache: {}",
            config.cache_dir.display()
        ));
    }
    Ok(())
}

pub(crate) fn clean_build_source_dirs(config: &config::Config) -> Result<()> {
    if config.build_dir.exists() {
        fs::remove_dir_all(&config.build_dir).with_context(|| {
            format!(
                "Failed to clean build source dirs: {}",
                config.build_dir.display()
            )
        })?;
        ui::success(format!(
            "Cleaned build source dirs: {}",
            config.build_dir.display()
        ));
    }
    Ok(())
}

pub(crate) fn warn_if_running_as_root_for_build(command: &str, rootfs: &Path) {
    if crate::fakeroot::is_root() {
        ui::warn(format!("Running '{}' as root is discouraged.", command));
        ui::warn(
            "A misconfigured build environment or malicious/buggy build file can overwrite or delete critical system files.",
        );
        ui::warn("Recommendation: use a non-root build user and only install as root.");
        ui::warn(format!("Current rootfs target: {}", rootfs.display()));
    }
}

pub(crate) fn merge_missing_dependencies(mut base: Vec<String>, extra: Vec<String>) -> Vec<String> {
    for dep in extra {
        if !base.contains(&dep) {
            base.push(dep);
        }
    }
    base
}

pub(crate) fn make_lib32_build_spec(base: &package::PackageSpec) -> package::PackageSpec {
    let mut spec = base.clone();
    let flags = &mut spec.build.flags;
    flags.lib32_variant = true;
    flags.cflags = flags.cflags_lib32.clone();
    flags.replace_cflags = flags.replace_cflags_lib32.clone();
    flags.cxxflags = flags.cxxflags_lib32.clone();
    flags.replace_cxxflags = flags.replace_cxxflags_lib32.clone();
    if !flags.configure_lib32.is_empty() {
        flags.configure = flags.configure_lib32.clone();
    }
    if !flags.post_configure_lib32.is_empty() {
        flags.post_configure = flags.post_configure_lib32.clone();
    }
    if !flags.post_compile_lib32.is_empty() {
        flags.post_compile = flags.post_compile_lib32.clone();
    }
    if !flags.post_install_lib32.is_empty() {
        flags.post_install = flags.post_install_lib32.clone();
    }

    flags.cc = format!("{} -m32", flags.cc.trim());
    flags.cxx = format!("{} -m32", flags.cxx.trim());
    flags.build_dir = Some(match flags.build_dir.as_deref() {
        Some(dir) if !dir.trim().is_empty() => format!("{}-lib32", dir.trim()),
        _ => "build-lib32".to_string(),
    });

    spec
}

pub(crate) fn make_lib32_package_spec(base: &package::PackageSpec) -> package::PackageSpec {
    let mut spec = base.clone();
    let lib32_name = format!("lib32-{}", base.package.name);
    spec.package.name = lib32_name.clone();
    spec.packages.clear();
    spec.alternatives = base.alternatives_for_output(&lib32_name);
    spec.dependencies = base.dependencies_for_output(&lib32_name);
    spec
}

pub(crate) struct RequestedBuildToolPackageInstall<'a> {
    pub(crate) build_type: package::BuildType,
    pub(crate) rootfs: &'a Path,
    pub(crate) config: &'a config::Config,
    pub(crate) db_path: &'a Path,
    pub(crate) spec_path: &'a Path,
    pub(crate) execution: InstallPlanExecutionOptions<'a>,
    pub(crate) assume_yes: bool,
}

pub(crate) fn ensure_requested_build_tool_package_installed(
    request: RequestedBuildToolPackageInstall<'_>,
) -> Result<()> {
    let Some(package_name) = builder::requested_build_tool_package(request.build_type) else {
        return Ok(());
    };
    let build_tool_option = builder::build_tool_package_option(request.build_type);

    if deps::is_dep_satisfied_in_db(&package_name, request.db_path)? {
        return Ok(());
    }

    ui::warn(format!(
        "Missing required build tool package for {:?} builds{}: {}",
        request.build_type,
        build_tool_option
            .map(|option| format!(" ({option})"))
            .unwrap_or_default(),
        package_name
    ));

    let local_sibling_root = request
        .spec_path
        .parent()
        .and_then(|path| path.parent())
        .map(Path::to_path_buf);
    let plan = planner::build_dependency_install_plan(
        request.config,
        request.rootfs,
        std::slice::from_ref(&package_name),
        planner::PlannerOptions {
            assume_yes: request.assume_yes,
            prefer_binary: request.config.repo_settings.prefer_binary,
            local_sibling_root,
            include_test_deps: request.execution.install_test_deps,
            lib32_only_requested_specs: false,
        },
    )
    .with_context(|| {
        format!(
            "Failed to resolve required build tool package '{}'",
            package_name
        )
    })?;

    if plan.steps.is_empty() {
        anyhow::bail!(
            "Required build tool package '{}' is not installed and no install plan could be created",
            package_name
        );
    }

    super::print_plan_summary(&plan);
    super::execute_install_plan_with_child_commands(
        &plan,
        request.rootfs,
        request.config,
        request.execution,
    )
}

fn ensure_requested_development_package_installed_for(
    package_name: Option<&str>,
    db_path: &Path,
) -> Result<()> {
    let Some(package_name) = package_name.filter(|name| !name.trim().is_empty()) else {
        return Ok(());
    };

    if deps::is_dep_satisfied_in_db(package_name, db_path)? {
        return Ok(());
    }

    anyhow::bail!(
        "Missing required development package for source builds ({}): {}. Install it first before building packages from source.",
        builder::development_package_option(),
        package_name
    );
}

pub(crate) fn ensure_requested_development_package_installed(db_path: &Path) -> Result<()> {
    let package_name = builder::requested_development_package();
    ensure_requested_development_package_installed_for(package_name.as_deref(), db_path)
}

pub(crate) fn build_lib32_companion_package(
    pkg_spec: &package::PackageSpec,
    src_dir: &Path,
    config: &config::Config,
    cross_config: Option<&cross::CrossConfig>,
    export_compiler_flags: bool,
    force: bool,
) -> Result<Option<(package::PackageSpec, PathBuf)>> {
    if !pkg_spec.builds_lib32_output() && !force {
        return Ok(None);
    }
    if pkg_spec.is_metapackage() {
        crate::log_warn!(
            "Ignoring build.flags.build-32 for metapackage {}",
            pkg_spec.package.name
        );
        return Ok(None);
    }

    crate::log_info!("Running separate lib32 build pass...");
    let host_build_dir = builder::ensure_host_build(
        pkg_spec,
        src_dir,
        cross_config,
        export_compiler_flags,
        builder::TargetBuildKind::Lib32,
    )?;
    let mut lib32_input = pkg_spec.clone();
    lib32_input.build.flags.build_32 = true;
    let mut lib32_build_spec = make_lib32_build_spec(&lib32_input);
    if let Some(host_dir) = host_build_dir.as_ref() {
        lib32_build_spec.build.flags.host_build_dir = Some(host_dir.to_string_lossy().into_owned());
    }
    let lib32_pkg_spec = make_lib32_package_spec(pkg_spec);
    let lib32_destdir = config
        .build_dir
        .join("destdir")
        .join(&lib32_pkg_spec.package.name);
    if lib32_destdir.exists() {
        fs::remove_dir_all(&lib32_destdir).with_context(|| {
            format!("Failed to clean lib32 destdir: {}", lib32_destdir.display())
        })?;
    }
    fs::create_dir_all(&lib32_destdir).with_context(|| {
        format!(
            "Failed to create lib32 destdir: {}",
            lib32_destdir.display()
        )
    })?;

    builder::build(
        &lib32_build_spec,
        src_dir,
        &lib32_destdir,
        cross_config,
        export_compiler_flags,
        host_build_dir.as_deref(),
    )?;

    let lib32_src = lib32_destdir.join("usr/lib32");
    if !lib32_src.exists() {
        anyhow::bail!(
            "lib32 build completed but did not install usr/lib32 into {}",
            lib32_destdir.display()
        );
    }
    install::scripts::stage_scripts_from_spec_dir(&lib32_pkg_spec, &lib32_destdir)?;
    staging::process(&lib32_destdir, &lib32_pkg_spec)?;
    staging::symlink_package_license(
        &lib32_destdir,
        &lib32_pkg_spec.package.name,
        &pkg_spec.package.name,
    )?;

    Ok(Some((lib32_pkg_spec, lib32_destdir)))
}

#[cfg(test)]
mod tests {
    use super::ensure_requested_development_package_installed_for;
    use crate::config;
    use crate::db;
    use crate::package::{
        Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
    };
    use anyhow::Result;
    use std::fs;
    use std::path::PathBuf;

    fn test_spec(name: &str) -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: name.into(),
                real_name: None,
                version: "1.0.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "https://example.test".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.test/src.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "src".into(),
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
            spec_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn requested_development_package_requirement_fails_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("installed.sqlite");
        let err =
            ensure_requested_development_package_installed_for(Some("development-base"), &db_path)
                .expect_err("missing development package should fail");
        assert!(err.to_string().contains("development-base"));
    }

    #[test]
    fn requested_development_package_requirement_passes_when_installed() -> Result<()> {
        let rootfs = tempfile::tempdir()?;
        let config = config::Config::for_rootfs(rootfs.path());
        fs::create_dir_all(&config.db_dir)?;
        let db_path = config.installed_db_path(rootfs.path());

        let spec = test_spec("development-base");
        let dest = rootfs.path().join("dest").join("development-base");
        fs::create_dir_all(dest.join("usr/bin"))?;
        fs::write(dest.join("usr/bin/dev-base"), "bin")?;
        db::register_package(&db_path, &spec, &dest)?;

        ensure_requested_development_package_installed_for(Some("development-base"), &db_path)?;
        Ok(())
    }
}
