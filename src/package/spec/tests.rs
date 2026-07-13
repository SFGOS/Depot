use super::*;
use std::path::{Path, PathBuf};

fn mk_spec(name: &str, version: &str) -> PackageSpec {
    PackageSpec {
        package: PackageInfo {
            name: name.into(),
            real_name: None,
            version: version.into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "h".into(),
            sha256: "s".into(),
            extract_dir: "e".into(),
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

mod build_config_cases;
mod package_cases;
mod source_cases;
mod validation_cases;
