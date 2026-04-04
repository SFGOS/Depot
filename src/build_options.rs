use crate::package::BuildType;
use anyhow::Result;

const BUILD_DEPOT_STATIC_OPTION: &str = "BUILD_DEPOT_STATIC";
const DEPOT_DEVELOPMENT_PACKAGE_OPTION: &str = "DEPOT_DEVELOPMENT_PACKAGE";

fn parse_boolish_option(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enable" | "enabled" | "static" => Some(true),
        "0" | "false" | "no" | "off" | "disable" | "disabled" | "shared" => Some(false),
        _ => None,
    }
}

fn normalize_string_option(value: Option<&'static str>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

pub(crate) fn requested_static_build() -> Result<Option<bool>> {
    let Some(raw) = option_env!("BUILD_DEPOT_STATIC") else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    parse_boolish_option(raw).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid -D{} value '{}'; expected true/false, on/off, enable/disable, static/shared, or 1/0",
            BUILD_DEPOT_STATIC_OPTION,
            raw
        )
    }).map(Some)
}

pub(crate) fn build_tool_package_option(build_type: BuildType) -> Option<&'static str> {
    match build_type {
        BuildType::Autotools => Some("DEPOT_AUTOTOOLS_PACKAGE"),
        BuildType::CMake => Some("DEPOT_CMAKE_PACKAGE"),
        BuildType::Meson => Some("DEPOT_MESON_PACKAGE"),
        BuildType::Perl => Some("DEPOT_PERL_PACKAGE"),
        BuildType::Custom => Some("DEPOT_CUSTOM_PACKAGE"),
        BuildType::Python => Some("DEPOT_PYTHON_PACKAGE"),
        BuildType::Rust => Some("DEPOT_RUST_PACKAGE"),
        BuildType::Makefile => Some("DEPOT_MAKEFILE_PACKAGE"),
        BuildType::Bin | BuildType::Meta => None,
    }
}

pub(crate) fn requested_build_tool_package(build_type: BuildType) -> Option<String> {
    match build_type {
        BuildType::Autotools => normalize_string_option(option_env!("DEPOT_AUTOTOOLS_PACKAGE")),
        BuildType::CMake => normalize_string_option(option_env!("DEPOT_CMAKE_PACKAGE")),
        BuildType::Meson => normalize_string_option(option_env!("DEPOT_MESON_PACKAGE")),
        BuildType::Perl => normalize_string_option(option_env!("DEPOT_PERL_PACKAGE")),
        BuildType::Custom => normalize_string_option(option_env!("DEPOT_CUSTOM_PACKAGE")),
        BuildType::Python => normalize_string_option(option_env!("DEPOT_PYTHON_PACKAGE")),
        BuildType::Rust => normalize_string_option(option_env!("DEPOT_RUST_PACKAGE")),
        BuildType::Makefile => normalize_string_option(option_env!("DEPOT_MAKEFILE_PACKAGE")),
        BuildType::Bin | BuildType::Meta => None,
    }
}

pub(crate) fn development_package_option() -> &'static str {
    DEPOT_DEVELOPMENT_PACKAGE_OPTION
}

pub(crate) fn requested_development_package() -> Option<String> {
    normalize_string_option(option_env!("DEPOT_DEVELOPMENT_PACKAGE"))
}

#[cfg(test)]
mod tests {
    use super::{
        build_tool_package_option, development_package_option, normalize_string_option,
        parse_boolish_option,
    };
    use crate::package::BuildType;

    #[test]
    fn parse_boolish_option_accepts_expected_values() {
        assert_eq!(parse_boolish_option("true"), Some(true));
        assert_eq!(parse_boolish_option("enable"), Some(true));
        assert_eq!(parse_boolish_option("shared"), Some(false));
        assert_eq!(parse_boolish_option("off"), Some(false));
        assert_eq!(parse_boolish_option("maybe"), None);
    }

    #[test]
    fn normalize_string_option_trims_and_drops_empty_values() {
        assert_eq!(
            normalize_string_option(Some("  meson-bootstrap  ")),
            Some("meson-bootstrap".to_string())
        );
        assert_eq!(normalize_string_option(Some("   ")), None);
        assert_eq!(normalize_string_option(None), None);
    }

    #[test]
    fn build_tool_package_option_maps_supported_builders() {
        assert_eq!(
            build_tool_package_option(BuildType::Meson),
            Some("DEPOT_MESON_PACKAGE")
        );
        assert_eq!(
            build_tool_package_option(BuildType::CMake),
            Some("DEPOT_CMAKE_PACKAGE")
        );
        assert_eq!(build_tool_package_option(BuildType::Bin), None);
    }

    #[test]
    fn development_package_option_matches_expected_name() {
        assert_eq!(development_package_option(), "DEPOT_DEVELOPMENT_PACKAGE");
    }
}
