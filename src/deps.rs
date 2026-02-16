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
use crate::package::PackageSpec;
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

    if !db_path.exists() {
        return Ok(spec.dependencies.runtime.clone());
    }

    let installed = db::get_installed_packages(db_path)?;
    let provides = db::get_all_provides(db_path)?;

    for dep in &spec.dependencies.runtime {
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

    if !spec.dependencies.build.is_empty() {
        println!("Build dependencies: {}", spec.dependencies.build.join(", "));
        if !missing_build.is_empty() {
            println!("  Missing: {}", missing_build.join(", "));
        }
    }

    if !spec.dependencies.runtime.is_empty() {
        println!(
            "Runtime dependencies: {}",
            spec.dependencies.runtime.join(", ")
        );
        if !missing_runtime.is_empty() {
            println!("  Missing: {}", missing_runtime.join(", "));
        }
    }

    Ok(())
}

/// Verify all build dependencies are installed, error if not
pub fn require_build_deps(spec: &PackageSpec, db_path: &Path) -> Result<()> {
    let missing = check_build_deps(spec, db_path)?;

    if !missing.is_empty() {
        anyhow::bail!(
            "Missing build dependencies: {}\nInstall them first with: nyapm install <package>",
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
}
