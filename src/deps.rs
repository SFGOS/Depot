//! Dependency resolution for packages
//!
//! Supports versioned dependencies with operators:
//! - `package` - any version
//! - `package#1.2.3` - exactly version 1.2.3
//! - `package>1.2.3` - greater than 1.2.3
//! - `package<1.2.3` - less than 1.2.3
//! - `package>=1.2.3` - greater than or equal to 1.2.3
//! - `package<=1.2.3` - less than or equal to 1.2.3

use crate::db;
use crate::package::{BuildType, PackageSpec};
use crate::ui;
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;

/// Version comparison operator
#[derive(Debug, Clone, Copy, PartialEq)]
enum VersionOp {
    Exact, // #
    Gt,    // >
    Lt,    // <
    Gte,   // >=
    Lte,   // <=
    Any,   // no operator
}

/// Parsed dependency with optional version constraint
struct ParsedDep<'a> {
    name: &'a str,
    version: Option<&'a str>,
    op: VersionOp,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RequestedOutputs {
    PrimaryOnly,
    PrimaryAndLib32,
    Lib32Only,
}

impl RequestedOutputs {
    pub(crate) fn includes_lib32(self) -> bool {
        matches!(self, Self::PrimaryAndLib32 | Self::Lib32Only)
    }
}

/// Parse a dependency string into name, version, and operator
fn parse_dep(dep: &str) -> ParsedDep<'_> {
    // Try operators in order of specificity (>= before >, etc.)
    if let Some((name, ver)) = dep.split_once(">=") {
        return ParsedDep {
            name,
            version: Some(ver),
            op: VersionOp::Gte,
        };
    }
    if let Some((name, ver)) = dep.split_once("<=") {
        return ParsedDep {
            name,
            version: Some(ver),
            op: VersionOp::Lte,
        };
    }
    if let Some((name, ver)) = dep.split_once('>') {
        return ParsedDep {
            name,
            version: Some(ver),
            op: VersionOp::Gt,
        };
    }
    if let Some((name, ver)) = dep.split_once('<') {
        return ParsedDep {
            name,
            version: Some(ver),
            op: VersionOp::Lt,
        };
    }
    if let Some((name, ver)) = dep.split_once('#') {
        return ParsedDep {
            name,
            version: Some(ver),
            op: VersionOp::Exact,
        };
    }

    ParsedDep {
        name: dep,
        version: None,
        op: VersionOp::Any,
    }
}

/// Return the dependency name portion without any version/operator suffix.
pub fn dep_name(dep: &str) -> &str {
    parse_dep(dep).name
}

/// Compare two version strings using semver if possible, fallback to string compare
fn compare_versions(installed: &str, required: &str, op: VersionOp) -> bool {
    // Try semver comparison first
    if let (Ok(inst), Ok(req)) = (
        semver::Version::parse(installed),
        semver::Version::parse(required),
    ) {
        return match op {
            VersionOp::Exact => inst == req,
            VersionOp::Gt => inst > req,
            VersionOp::Lt => inst < req,
            VersionOp::Gte => inst >= req,
            VersionOp::Lte => inst <= req,
            VersionOp::Any => true,
        };
    }

    // Fallback to string comparison for non-semver versions
    match op {
        VersionOp::Exact => installed == required,
        VersionOp::Gt => installed > required,
        VersionOp::Lt => installed < required,
        VersionOp::Gte => installed >= required,
        VersionOp::Lte => installed <= required,
        VersionOp::Any => true,
    }
}

/// Check if a versioned dependency is satisfied
fn is_dep_satisfied(
    dep: &str,
    installed: &std::collections::HashSet<String>,
    provides: &std::collections::HashSet<String>,
    replaces: &std::collections::HashSet<String>,
    db_path: &Path,
) -> Result<bool> {
    let parsed = parse_dep(dep);

    // Check if package is installed, provided, or replaced by an installed package.
    if !installed.contains(parsed.name)
        && !provides.contains(parsed.name)
        && !replaces.contains(parsed.name)
    {
        return Ok(false);
    }

    // If no version required, we're good
    let Some(required) = parsed.version else {
        return Ok(true);
    };

    // Check version matches
    if let Some(installed_version) = db::get_dependency_version(db_path, parsed.name)? {
        Ok(compare_versions(&installed_version, required, parsed.op))
    } else {
        // Package might be satisfied by an alternative or replacement, accept it.
        Ok(provides.contains(parsed.name) || replaces.contains(parsed.name))
    }
}

fn build_type_runs_automatic_tests(spec: &PackageSpec) -> bool {
    matches!(
        spec.build.build_type,
        BuildType::Autotools | BuildType::CMake | BuildType::Meson | BuildType::Perl
    )
}

fn automatic_tests_disabled_for_outputs(spec: &PackageSpec, outputs: RequestedOutputs) -> bool {
    spec.should_skip_automatic_tests() || outputs.includes_lib32()
}

/// Check whether a dependency expression is satisfied by the installed package DB.
pub fn is_dep_satisfied_in_db(dep: &str, db_path: &Path) -> Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }

    let installed = db::get_installed_dependency_names(db_path)?;
    let provides = db::get_all_provides(db_path)?;
    let replaces = db::get_all_replaces(db_path)?;
    is_dep_satisfied(dep, &installed, &provides, &replaces, db_path)
}

fn push_unique(v: &mut Vec<String>, item: String) {
    if !v.contains(&item) {
        v.push(item);
    }
}

fn requested_dependency_sets(
    spec: &PackageSpec,
    outputs: RequestedOutputs,
) -> Vec<crate::package::Dependencies> {
    match outputs {
        RequestedOutputs::PrimaryOnly => vec![spec.dependencies.primary_dependencies()],
        RequestedOutputs::PrimaryAndLib32 => {
            vec![
                spec.dependencies.primary_dependencies(),
                spec.lib32_dependencies(),
            ]
        }
        RequestedOutputs::Lib32Only => vec![spec.lib32_dependencies()],
    }
}

fn requested_local_provides(spec: &PackageSpec, outputs: RequestedOutputs) -> HashSet<String> {
    match outputs {
        RequestedOutputs::PrimaryOnly => spec.local_dependency_provides_for_selection(true, false),
        RequestedOutputs::PrimaryAndLib32 => {
            spec.local_dependency_provides_for_selection(true, true)
        }
        RequestedOutputs::Lib32Only => spec.local_dependency_provides_for_selection(false, true),
    }
}

fn collect_build_deps(spec: &PackageSpec, outputs: RequestedOutputs) -> Vec<String> {
    let mut deps = Vec::new();
    for dep_set in requested_dependency_sets(spec, outputs) {
        for dep in dep_set.build {
            push_unique(&mut deps, dep);
        }
    }
    deps
}

fn collect_runtime_deps(spec: &PackageSpec, outputs: RequestedOutputs) -> Vec<String> {
    let mut deps = Vec::new();
    for dep_set in requested_dependency_sets(spec, outputs) {
        for dep in dep_set.runtime {
            push_unique(&mut deps, dep);
        }
    }
    deps
}

pub(crate) fn declared_test_deps(spec: &PackageSpec, outputs: RequestedOutputs) -> Vec<String> {
    let mut deps = Vec::new();
    for dep_set in requested_dependency_sets(spec, outputs) {
        for dep in dep_set.test {
            push_unique(&mut deps, dep);
        }
    }
    deps
}

/// Check if all build dependencies are satisfied for the selected outputs.
pub(crate) fn check_build_deps_for_outputs(
    spec: &PackageSpec,
    db_path: &Path,
    outputs: RequestedOutputs,
) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    let build_deps = collect_build_deps(spec, outputs);

    if !db_path.exists() {
        return Ok(build_deps);
    }

    let installed = db::get_installed_dependency_names(db_path)?;
    let provides = db::get_all_provides(db_path)?;
    let replaces = db::get_all_replaces(db_path)?;

    for dep in &build_deps {
        if !is_dep_satisfied(dep, &installed, &provides, &replaces, db_path)? {
            missing.push(dep.clone());
        }
    }

    Ok(missing)
}

/// Check if all runtime dependencies are satisfied for the selected outputs.
pub(crate) fn check_runtime_deps_for_outputs(
    spec: &PackageSpec,
    db_path: &Path,
    outputs: RequestedOutputs,
) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    let runtime_deps = collect_runtime_deps(spec, outputs);
    let local_provides = requested_local_provides(spec, outputs);

    if !db_path.exists() {
        for dep in &runtime_deps {
            if !local_provides.contains(dep_name(dep)) {
                missing.push(dep.clone());
            }
        }
        return Ok(missing);
    }

    let installed = db::get_installed_dependency_names(db_path)?;
    let provides = db::get_all_provides(db_path)?;
    let replaces = db::get_all_replaces(db_path)?;

    for dep in &runtime_deps {
        if local_provides.contains(dep_name(dep)) {
            continue;
        }
        if !is_dep_satisfied(dep, &installed, &provides, &replaces, db_path)? {
            missing.push(dep.clone());
        }
    }

    Ok(missing)
}

/// Check if all test dependencies are satisfied for the selected outputs.
pub(crate) fn check_test_deps_for_outputs(
    spec: &PackageSpec,
    db_path: &Path,
    outputs: RequestedOutputs,
) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    let test_deps = declared_test_deps(spec, outputs);

    if !db_path.exists() {
        return Ok(test_deps);
    }

    let installed = db::get_installed_dependency_names(db_path)?;
    let provides = db::get_all_provides(db_path)?;
    let replaces = db::get_all_replaces(db_path)?;

    for dep in &test_deps {
        if !is_dep_satisfied(dep, &installed, &provides, &replaces, db_path)? {
            missing.push(dep.clone());
        }
    }

    Ok(missing)
}

/// Print dependency status
pub fn print_dep_status(spec: &PackageSpec, db_path: &Path) -> Result<()> {
    print_dep_status_for_outputs(spec, db_path, RequestedOutputs::PrimaryOnly)
}

fn print_named_dep_status(label: &str, deps: &[String], missing: &[String], warn_on_missing: bool) {
    if deps.is_empty() {
        return;
    }

    ui::info(format!("{label}: {}", deps.join(", ")));
    if warn_on_missing && !missing.is_empty() {
        ui::warn(format!("{label} missing: {}", missing.join(", ")));
    }
}

/// Print dependency status for the selected outputs.
pub(crate) fn print_dep_status_for_outputs(
    spec: &PackageSpec,
    db_path: &Path,
    outputs: RequestedOutputs,
) -> Result<()> {
    let primary = spec.dependencies.primary_dependencies();
    let lib32 = spec.lib32_dependencies();

    if matches!(
        outputs,
        RequestedOutputs::PrimaryOnly | RequestedOutputs::PrimaryAndLib32
    ) {
        let missing_build =
            check_build_deps_for_outputs(spec, db_path, RequestedOutputs::PrimaryOnly)?;
        let missing_runtime =
            check_runtime_deps_for_outputs(spec, db_path, RequestedOutputs::PrimaryOnly)?;
        let missing_test =
            check_test_deps_for_outputs(spec, db_path, RequestedOutputs::PrimaryOnly)?;

        print_named_dep_status("Build dependencies", &primary.build, &missing_build, true);
        print_named_dep_status(
            "Runtime dependencies",
            &primary.runtime,
            &missing_runtime,
            true,
        );
        print_named_dep_status(
            "Test dependencies",
            &primary.test,
            &missing_test,
            !automatic_tests_disabled_for_outputs(spec, outputs)
                && build_type_runs_automatic_tests(spec),
        );
        if !primary.optional.is_empty() {
            ui::info(format!(
                "Optional dependencies: {}",
                primary.optional.join(", ")
            ));
        }
    }

    if matches!(
        outputs,
        RequestedOutputs::PrimaryAndLib32 | RequestedOutputs::Lib32Only
    ) {
        let missing_build =
            check_build_deps_for_outputs(spec, db_path, RequestedOutputs::Lib32Only)?;
        let missing_runtime =
            check_runtime_deps_for_outputs(spec, db_path, RequestedOutputs::Lib32Only)?;
        let missing_test = check_test_deps_for_outputs(spec, db_path, RequestedOutputs::Lib32Only)?;

        if outputs == RequestedOutputs::Lib32Only || lib32 != primary {
            print_named_dep_status(
                "Lib32 build dependencies",
                &lib32.build,
                &missing_build,
                true,
            );
            print_named_dep_status(
                "Lib32 runtime dependencies",
                &lib32.runtime,
                &missing_runtime,
                true,
            );
            print_named_dep_status(
                "Lib32 test dependencies",
                &lib32.test,
                &missing_test,
                !automatic_tests_disabled_for_outputs(spec, outputs)
                    && build_type_runs_automatic_tests(spec),
            );
            if !lib32.optional.is_empty() {
                ui::info(format!(
                    "Lib32 optional dependencies: {}",
                    lib32.optional.join(", ")
                ));
            }
        }
    }

    Ok(())
}

/// Verify all build dependencies are installed for the selected outputs.
pub(crate) fn require_build_deps_for_outputs(
    spec: &PackageSpec,
    db_path: &Path,
    outputs: RequestedOutputs,
) -> Result<()> {
    let missing = check_build_deps_for_outputs(spec, db_path, outputs)?;

    if !missing.is_empty() {
        anyhow::bail!(
            "Missing build dependencies: {}\nInstall them first with: depot install <package>",
            missing.join(", ")
        );
    }

    Ok(())
}

/// Verify all runtime dependencies are installed for the selected outputs.
pub(crate) fn require_runtime_deps_for_outputs(
    spec: &PackageSpec,
    db_path: &Path,
    outputs: RequestedOutputs,
) -> Result<()> {
    let missing = check_runtime_deps_for_outputs(spec, db_path, outputs)?;

    if !missing.is_empty() {
        anyhow::bail!(
            "Missing runtime dependencies: {}\nInstall them first with: depot install <package>",
            missing.join(", ")
        );
    }

    Ok(())
}

/// Verify all test dependencies are installed for the selected outputs.
pub(crate) fn require_test_deps_for_outputs(
    spec: &PackageSpec,
    db_path: &Path,
    outputs: RequestedOutputs,
) -> Result<()> {
    let missing = check_test_deps_for_outputs(spec, db_path, outputs)?;

    if !missing.is_empty() {
        anyhow::bail!(
            "Missing test dependencies: {}\nInstall them first with: depot install --test-deps <package>",
            missing.join(", ")
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec_with_build(
        build_type: BuildType,
        configure_test_target: Option<&str>,
        configure_test_targets: &[&str],
    ) -> PackageSpec {
        let mut flags = crate::package::BuildFlags::default();
        if let Some(target) = configure_test_target {
            flags.make_test_target = target.to_string();
        }
        flags.make_test_targets = configure_test_targets
            .iter()
            .map(|target| (*target).to_string())
            .collect();
        PackageSpec {
            package: crate::package::PackageInfo {
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
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "https://example.test/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: crate::package::Build { build_type, flags },
            dependencies: crate::package::Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        }
    }

    #[test]
    fn test_parse_dep() {
        let cases = vec![
            ("package", "package", None, VersionOp::Any),
            ("pkg#1.0.0", "pkg", Some("1.0.0"), VersionOp::Exact),
            ("pkg>1.0", "pkg", Some("1.0"), VersionOp::Gt),
            ("pkg<2.0", "pkg", Some("2.0"), VersionOp::Lt),
            ("pkg>=1.5", "pkg", Some("1.5"), VersionOp::Gte),
            ("pkg<=2.5", "pkg", Some("2.5"), VersionOp::Lte),
        ];

        for (input, name, ver, op) in cases {
            let parsed = parse_dep(input);
            assert_eq!(parsed.name, name, "Failed name for {}", input);
            assert_eq!(parsed.version, ver, "Failed version for {}", input);
            assert_eq!(parsed.op, op, "Failed op for {}", input);
        }
    }

    #[test]
    fn test_compare_versions_semver() {
        assert!(compare_versions("1.0.0", "1.0.0", VersionOp::Exact));
        assert!(!compare_versions("1.0.1", "1.0.0", VersionOp::Exact));

        assert!(compare_versions("1.1.0", "1.0.0", VersionOp::Gt));
        assert!(!compare_versions("1.0.0", "1.0.0", VersionOp::Gt));

        assert!(compare_versions("0.9.0", "1.0.0", VersionOp::Lt));
        assert!(!compare_versions("1.0.0", "1.0.0", VersionOp::Lt));

        assert!(compare_versions("1.0.0", "1.0.0", VersionOp::Gte));
        assert!(compare_versions("1.1.0", "1.0.0", VersionOp::Gte));

        assert!(compare_versions("1.0.0", "1.0.0", VersionOp::Lte));
        assert!(compare_versions("0.9.0", "1.0.0", VersionOp::Lte));
    }

    #[test]
    fn test_compare_versions_fallback() {
        // String comparison fallback
        assert!(compare_versions("b", "a", VersionOp::Gt));
        assert!(compare_versions("a", "b", VersionOp::Lt));
        assert!(compare_versions("foo", "foo", VersionOp::Exact));
    }

    #[test]
    fn test_check_test_deps_returns_test_deps_when_db_missing() {
        let spec = PackageSpec {
            package: crate::package::PackageInfo {
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
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "https://example.test/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: crate::package::Build {
                build_type: crate::package::BuildType::Custom,
                flags: crate::package::BuildFlags::default(),
            },
            dependencies: crate::package::Dependencies {
                build: Vec::new(),
                runtime: Vec::new(),
                test: vec!["bats".into(), "python".into()],
                optional: Vec::new(),
                groups: Vec::new(),
                lib32: None,
            },
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        };

        let missing = check_test_deps_for_outputs(
            &spec,
            Path::new("/definitely/not/a/real/db"),
            RequestedOutputs::PrimaryOnly,
        )
        .unwrap();
        assert_eq!(missing, vec!["bats".to_string(), "python".to_string()]);
    }

    #[test]
    fn test_require_runtime_deps_errors_when_db_missing() {
        let spec = PackageSpec {
            package: crate::package::PackageInfo {
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
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "https://example.test/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: crate::package::Build {
                build_type: crate::package::BuildType::Custom,
                flags: crate::package::BuildFlags::default(),
            },
            dependencies: crate::package::Dependencies {
                build: Vec::new(),
                runtime: vec!["python".into()],
                test: Vec::new(),
                optional: Vec::new(),
                groups: Vec::new(),
                lib32: None,
            },
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        };

        let err = require_runtime_deps_for_outputs(
            &spec,
            Path::new("/definitely/not/a/real/db"),
            RequestedOutputs::PrimaryOnly,
        )
        .expect_err("runtime deps should be required");
        assert!(err.to_string().contains("Missing runtime dependencies"));
    }

    #[test]
    fn test_check_runtime_deps_ignores_local_outputs_and_provides() {
        let spec = PackageSpec {
            package: crate::package::PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: vec![crate::package::PackageInfo {
                name: "foo-libs".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            }],
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "https://example.test/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: crate::package::Build {
                build_type: crate::package::BuildType::Custom,
                flags: crate::package::BuildFlags::default(),
            },
            dependencies: crate::package::Dependencies {
                build: Vec::new(),
                runtime: vec!["foo-libs".into(), "libfoo".into(), "python".into()],
                test: Vec::new(),
                optional: Vec::new(),
                groups: Vec::new(),
                lib32: None,
            },
            package_alternatives: std::collections::BTreeMap::from([(
                "foo-libs".to_string(),
                crate::package::Alternatives {
                    provides: vec!["libfoo".into()],
                    conflicts: Vec::new(),
                    replaces: Vec::new(),
                    lib32: None,
                },
            )]),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        };

        let missing = check_runtime_deps_for_outputs(
            &spec,
            Path::new("/definitely/not/a/real/db"),
            RequestedOutputs::PrimaryOnly,
        )
        .unwrap();
        assert_eq!(missing, vec!["python".to_string()]);
    }

    #[test]
    fn test_check_runtime_deps_for_lib32_only_does_not_treat_primary_output_as_local() {
        let mut spec = test_spec_with_build(BuildType::Custom, None, &[]);
        spec.dependencies.lib32 = Some(crate::package::DependencyGroup {
            build: Vec::new(),
            runtime: vec!["foo".into()],
            test: Vec::new(),
            optional: Vec::new(),
            groups: Vec::new(),
        });

        let missing = check_runtime_deps_for_outputs(
            &spec,
            Path::new("/definitely/not/a/real/db"),
            RequestedOutputs::Lib32Only,
        )
        .unwrap();
        assert_eq!(missing, vec!["foo".to_string()]);
    }

    #[test]
    fn test_installed_replacements_satisfy_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
        std::fs::write(destdir.join("usr/bin/vx"), "vx").unwrap();

        let mut spec = test_spec_with_build(BuildType::Custom, None, &[]);
        spec.package.name = "vx".into();
        spec.alternatives.replaces = vec!["patch".into(), "grep".into()];

        crate::db::register_package(&db_path, &spec, &destdir).unwrap();

        assert!(is_dep_satisfied_in_db("patch", &db_path).unwrap());
        assert!(is_dep_satisfied_in_db("grep", &db_path).unwrap());
    }

    #[test]
    fn test_installed_real_name_satisfies_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("packages.db");
        let destdir = tmp.path().join("dest");
        std::fs::create_dir_all(destdir.join("usr/lib")).unwrap();
        std::fs::write(destdir.join("usr/lib/libssl.so"), "ssl").unwrap();

        let mut spec = test_spec_with_build(BuildType::Custom, None, &[]);
        spec.package.name = "libressl43".into();
        spec.package.real_name = Some("libressl".into());
        spec.package.version = "4.3.2".into();

        crate::db::register_package(&db_path, &spec, &destdir).unwrap();

        assert!(is_dep_satisfied_in_db("libressl", &db_path).unwrap());
        assert!(is_dep_satisfied_in_db("libressl>=4.3.0", &db_path).unwrap());
    }

    #[test]
    fn test_build_type_runs_automatic_tests_matches_builder_behavior() {
        assert!(build_type_runs_automatic_tests(&test_spec_with_build(
            BuildType::Autotools,
            None,
            &[]
        )));
        assert!(build_type_runs_automatic_tests(&test_spec_with_build(
            BuildType::Perl,
            None,
            &[]
        )));
        assert!(build_type_runs_automatic_tests(&test_spec_with_build(
            BuildType::Meson,
            None,
            &[]
        )));
        assert!(build_type_runs_automatic_tests(&test_spec_with_build(
            BuildType::CMake,
            None,
            &[]
        )));
    }
}
