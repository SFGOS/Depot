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
    db_path: &Path,
) -> Result<bool> {
    let parsed = parse_dep(dep);

    // Check if package is installed or provided
    if !installed.contains(parsed.name) && !provides.contains(parsed.name) {
        return Ok(false);
    }

    // If no version required, we're good
    let Some(required) = parsed.version else {
        return Ok(true);
    };

    // Check version matches
    if let Some(installed_version) = db::get_package_version(db_path, parsed.name)? {
        Ok(compare_versions(&installed_version, required, parsed.op))
    } else {
        // Package might be provided by an alternative, accept it
        Ok(provides.contains(parsed.name))
    }
}

fn build_type_runs_automatic_tests(spec: &PackageSpec) -> bool {
    matches!(
        spec.build.build_type,
        BuildType::Autotools | BuildType::CMake
    )
}

/// Check whether a dependency expression is satisfied by the installed package DB.
pub fn is_dep_satisfied_in_db(dep: &str, db_path: &Path) -> Result<bool> {
    if !db_path.exists() {
        return Ok(false);
    }

    let installed = db::get_installed_packages(db_path)?;
    let provides = db::get_all_provides(db_path)?;
    is_dep_satisfied(dep, &installed, &provides, db_path)
}

/// Check if all build dependencies are satisfied
pub fn check_build_deps(spec: &PackageSpec, db_path: &Path) -> Result<Vec<String>> {
    let mut missing = Vec::new();

    if !db_path.exists() {
        return Ok(spec.dependencies.build.clone());
    }

    let installed = db::get_installed_packages(db_path)?;
    let provides = db::get_all_provides(db_path)?;

    for dep in &spec.dependencies.build {
        if !is_dep_satisfied(dep, &installed, &provides, db_path)? {
            missing.push(dep.clone());
        }
    }

    Ok(missing)
}

/// Check if all runtime dependencies are satisfied
pub fn check_runtime_deps(spec: &PackageSpec, db_path: &Path) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    let local_provides = spec.local_dependency_provides();

    if !db_path.exists() {
        for dep in &spec.dependencies.runtime {
            if !local_provides.contains(dep_name(dep)) {
                missing.push(dep.clone());
            }
        }
        return Ok(missing);
    }

    let installed = db::get_installed_packages(db_path)?;
    let provides = db::get_all_provides(db_path)?;

    for dep in &spec.dependencies.runtime {
        if local_provides.contains(dep_name(dep)) {
            continue;
        }
        if !is_dep_satisfied(dep, &installed, &provides, db_path)? {
            missing.push(dep.clone());
        }
    }

    Ok(missing)
}

/// Check if all test dependencies are satisfied
pub fn check_test_deps(spec: &PackageSpec, db_path: &Path) -> Result<Vec<String>> {
    let mut missing = Vec::new();

    if !db_path.exists() {
        return Ok(spec.dependencies.test.clone());
    }

    let installed = db::get_installed_packages(db_path)?;
    let provides = db::get_all_provides(db_path)?;

    for dep in &spec.dependencies.test {
        if !is_dep_satisfied(dep, &installed, &provides, db_path)? {
            missing.push(dep.clone());
        }
    }

    Ok(missing)
}

/// Print dependency status
pub fn print_dep_status(spec: &PackageSpec, db_path: &Path) -> Result<()> {
    let missing_build = check_build_deps(spec, db_path)?;
    let missing_runtime = check_runtime_deps(spec, db_path)?;
    let missing_test = check_test_deps(spec, db_path)?;

    if !spec.dependencies.build.is_empty() {
        ui::info(format!(
            "Build dependencies: {}",
            spec.dependencies.build.join(", ")
        ));
        if !missing_build.is_empty() {
            ui::warn(format!("Build deps missing: {}", missing_build.join(", ")));
        }
    }

    if !spec.dependencies.runtime.is_empty() {
        ui::info(format!(
            "Runtime dependencies: {}",
            spec.dependencies.runtime.join(", ")
        ));
        if !missing_runtime.is_empty() {
            ui::warn(format!(
                "Runtime deps missing: {}",
                missing_runtime.join(", ")
            ));
        }
    }

    if !spec.dependencies.test.is_empty() {
        ui::info(format!(
            "Test dependencies: {}",
            spec.dependencies.test.join(", ")
        ));
        if !spec.build.flags.skip_tests
            && build_type_runs_automatic_tests(spec)
            && !missing_test.is_empty()
        {
            ui::warn(format!("Test deps missing: {}", missing_test.join(", ")));
        }
    }

    if !spec.dependencies.optional.is_empty() {
        ui::info(format!(
            "Optional dependencies: {}",
            spec.dependencies.optional.join(", ")
        ));
    }

    Ok(())
}

/// Verify all build dependencies are installed, error if not
pub fn require_build_deps(spec: &PackageSpec, db_path: &Path) -> Result<()> {
    let missing = check_build_deps(spec, db_path)?;

    if !missing.is_empty() {
        anyhow::bail!(
            "Missing build dependencies: {}\nInstall them first with: depot install <package>",
            missing.join(", ")
        );
    }

    Ok(())
}

/// Verify all runtime dependencies are installed, error if not.
pub fn require_runtime_deps(spec: &PackageSpec, db_path: &Path) -> Result<()> {
    let missing = check_runtime_deps(spec, db_path)?;

    if !missing.is_empty() {
        anyhow::bail!(
            "Missing runtime dependencies: {}\nInstall them first with: depot install <package>",
            missing.join(", ")
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
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
            },
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        };

        let missing = check_test_deps(&spec, Path::new("/definitely/not/a/real/db")).unwrap();
        assert_eq!(missing, vec!["bats".to_string(), "python".to_string()]);
    }

    #[test]
    fn test_require_runtime_deps_errors_when_db_missing() {
        let spec = PackageSpec {
            package: crate::package::PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
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
            },
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        };

        let err = require_runtime_deps(&spec, Path::new("/definitely/not/a/real/db"))
            .expect_err("runtime deps should be required");
        assert!(err.to_string().contains("Missing runtime dependencies"));
    }

    #[test]
    fn test_check_runtime_deps_ignores_local_outputs_and_provides() {
        let spec = PackageSpec {
            package: crate::package::PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: vec![crate::package::PackageInfo {
                name: "foo-libs".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
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
            },
            package_alternatives: std::collections::BTreeMap::from([(
                "foo-libs".to_string(),
                crate::package::Alternatives {
                    provides: vec!["libfoo".into()],
                    replaces: Vec::new(),
                },
            )]),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        };

        let missing = check_runtime_deps(&spec, Path::new("/definitely/not/a/real/db")).unwrap();
        assert_eq!(missing, vec!["python".to_string()]);
    }
}
