use super::*;
use crate::package::{
    BuildFlags, BuildType, Dependencies, ManualSource, PackageInfo, PackageSpec, Source,
};

#[test]
fn spec_to_minimal_toml_omits_defaults() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "A test".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo-1.0.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo-1.0".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    assert!(toml.contains("name = \"foo\""));
    assert!(toml.contains("version = \"1.0\""));
    assert!(toml.contains("description = \"A test\""));
    // defaults should not be present
    assert!(!toml.contains("cflags"));
    assert!(!toml.contains("rustflags"));
    // sha256="skip" should be omitted
    assert!(!toml.contains("sha256"));
}

#[test]
fn spec_to_minimal_toml_includes_additional_packages() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "A test".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: vec![PackageInfo {
            name: "foo-dev".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "dev files".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        }],
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo-1.0.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo-1.0".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    assert!(toml.contains("[[packages]]"));
    assert!(toml.contains("name = \"foo-dev\""));
}

#[test]
fn spec_to_minimal_toml_includes_multiple_licenses_as_array() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "A test".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into(), "Apache-2.0".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo-1.0.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo-1.0".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    assert!(toml.contains("license = ["));
    assert!(toml.contains("\"MIT\""));
    assert!(toml.contains("\"Apache-2.0\""));
}

#[test]
fn spec_to_minimal_toml_includes_extended_build_flags() {
    let flags = BuildFlags {
        source_subdir: "project/subdir".into(),
        configure: vec!["--disable-static".into(), "--enable-foo".into()],
        configure_file: "build-aux/configure".into(),
        post_configure: vec!["./configure-helper.sh".into()],
        post_compile: vec!["make check".into()],
        post_install: vec!["strip $DESTDIR/usr/bin/foo".into()],
        makefile_commands: vec!["make".into()],
        makefile_install_commands: vec!["make DESTDIR=$DESTDIR install".into()],
        cargs: vec!["--locked".into()],
        config_settings: vec!["editable_mode=compat".into()],
        rustflags: vec!["-Ctarget-cpu=native".into()],
        cxxflags: vec!["-O2".into(), "-fno-rtti".into()],
        fuse_ld: "lld".into(),
        ltoflags: vec!["-flto=auto".into()],
        target: "x86_64-unknown-linux-gnu".into(),
        keep: vec!["etc/locale.gen".into()],
        sbindir: "/usr/sbin".into(),
        libdir: "/usr/lib64".into(),
        libexecdir: "/usr/libexec".into(),
        sysconfdir: "/etc/custom".into(),
        localstatedir: "/var/custom".into(),
        sharedstatedir: "/var/lib/custom".into(),
        includedir: "/usr/include/custom".into(),
        datarootdir: "/usr/share/root".into(),
        datadir: "/usr/share/custom".into(),
        mandir: "/usr/share/custom/man".into(),
        infodir: "/usr/share/custom/info".into(),
        use_lto: false,
        no_flags: true,
        no_strip: true,
        no_delete_static: true,
        no_compress_man: true,
        skip_tests: true,
        makeflags: "-j12 --output-sync=target".into(),
        make_vars: vec!["V=1".into()],
        make_dirs: vec!["lib".into(), "libelf".into()],
        make_test_vars: vec!["TESTS=unit".into()],
        make_test_dirs: vec!["tests".into()],
        make_install_vars: vec!["STRIPPROG=true".into()],
        make_install_dirs: vec!["lib".into(), "apps".into()],
        env_vars: vec![
            "SETUPTOOLS_SCM_PRETEND_VERSION=$version".into(),
            "PYO3_CONFIG_FILE=$specdir/pyo3.toml".into(),
        ],
        ..BuildFlags::default()
    };

    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "A test".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo-1.0.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo-1.0".into(),
            patches: vec!["fix.patch".into()],
            post_extract: vec!["autoreconf -fi".into()],
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags,
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    assert!(toml.contains("source_subdir = \"project/subdir\""));
    assert!(toml.contains("configure = ["));
    assert!(toml.contains("configure_file = \"build-aux/configure\""));
    assert!(toml.contains("post_configure = ["));
    assert!(toml.contains("post_compile = ["));
    assert!(toml.contains("post_install = ["));
    assert!(toml.contains("makefile_commands = ["));
    assert!(toml.contains("makefile_install_commands = ["));
    assert!(toml.contains("cargs = ["));
    assert!(toml.contains("config_setting = ["));
    assert!(toml.contains("rustflags = ["));
    assert!(toml.contains("cxxflags = ["));
    assert!(toml.contains("fuse_ld = \"lld\""));
    assert!(toml.contains("ltoflags = ["));
    assert!(toml.contains("target = \"x86_64-unknown-linux-gnu\""));
    assert!(toml.contains("keep = ["));
    assert!(toml.contains("\"etc/locale.gen\""));
    assert!(toml.contains("sbindir = \"/usr/sbin\""));
    assert!(toml.contains("libdir = \"/usr/lib64\""));
    assert!(toml.contains("libexecdir = \"/usr/libexec\""));
    assert!(toml.contains("sysconfdir = \"/etc/custom\""));
    assert!(toml.contains("localstatedir = \"/var/custom\""));
    assert!(toml.contains("sharedstatedir = \"/var/lib/custom\""));
    assert!(toml.contains("includedir = \"/usr/include/custom\""));
    assert!(toml.contains("datarootdir = \"/usr/share/root\""));
    assert!(toml.contains("datadir = \"/usr/share/custom\""));
    assert!(toml.contains("mandir = \"/usr/share/custom/man\""));
    assert!(toml.contains("infodir = \"/usr/share/custom/info\""));
    assert!(toml.contains("use_lto = false"));
    assert!(toml.contains("no_flags = true"));
    assert!(toml.contains("no_strip = true"));
    assert!(toml.contains("no_delete_static = true"));
    assert!(toml.contains("no_compress_man = true"));
    assert!(toml.contains("skip_tests = true"));
    assert!(toml.contains("makeflags = \"-j12 --output-sync=target\""));
    assert!(toml.contains("make_vars = ["));
    assert!(toml.contains("make_dirs = ["));
    assert!(toml.contains("make_test_vars = ["));
    assert!(toml.contains("make_test_dirs = ["));
    assert!(toml.contains("make_install_vars = ["));
    assert!(toml.contains("make_install_dirs = ["));
    assert!(toml.contains("env_vars = ["));
    assert!(toml.contains("patches = ["));
    assert!(toml.contains("post_extract = ["));
}

#[test]
fn spec_to_minimal_toml_includes_extract_dir_for_variable_default() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "A test".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo-1.0.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "$name-$version".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    assert!(toml.contains("extract_dir = \"$name-$version\""));
}

#[test]
fn spec_to_minimal_toml_includes_test_and_optional_dependencies() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "A test".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo-1.0.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo-1.0".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies {
            build: vec![],
            runtime: vec![],
            test: vec!["python".into(), "bats".into()],
            optional: vec!["gtk-doc".into()],
            groups: vec!["base".into(), "devtools".into()],
            lib32: None,
        },
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    let val: toml::Value = toml::from_str(&toml).unwrap();
    let test_deps = val
        .get("dependencies")
        .and_then(|d| d.get("test"))
        .and_then(|t| t.as_array())
        .expect("expected dependencies.test array");
    assert_eq!(test_deps.len(), 2);
    assert_eq!(test_deps[0].as_str(), Some("python"));
    assert_eq!(test_deps[1].as_str(), Some("bats"));
    let optional_deps = val
        .get("dependencies")
        .and_then(|d| d.get("optional"))
        .and_then(|t| t.as_array())
        .expect("expected dependencies.optional array");
    assert_eq!(optional_deps.len(), 1);
    assert_eq!(optional_deps[0].as_str(), Some("gtk-doc"));
    let groups = val
        .get("dependencies")
        .and_then(|d| d.get("groups"))
        .and_then(|t| t.as_array())
        .expect("expected dependencies.groups array");
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].as_str(), Some("base"));
    assert_eq!(groups[1].as_str(), Some("devtools"));
}

#[test]
fn spec_to_minimal_toml_includes_alternatives_conflicts_and_provides() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "A test".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives {
            provides: vec!["editor".into(), "sh".into()],
            conflicts: vec!["nano".into(), "busybox-sh".into()],
            replaces: vec!["vi".into()],
            lib32: Some(crate::package::AlternativeGroup {
                provides: Vec::new(),
                conflicts: Vec::new(),
                replaces: vec!["lib32-vi".into()],
            }),
        },
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo-1.0.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo-1.0".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    let val: toml::Value = toml::from_str(&toml).unwrap();
    let alternatives = val
        .get("alternatives")
        .and_then(|v| v.as_table())
        .expect("expected alternatives table");
    let provides = alternatives
        .get("provides")
        .and_then(|v| v.as_array())
        .expect("expected alternatives.provides array");
    let conflicts = alternatives
        .get("conflicts")
        .and_then(|v| v.as_array())
        .expect("expected alternatives.conflicts array");
    let replaces = alternatives
        .get("replaces")
        .and_then(|v| v.as_array())
        .expect("expected alternatives.replaces array");
    let lib32 = alternatives
        .get("lib32")
        .and_then(|v| v.as_table())
        .expect("expected alternatives.lib32 table");
    let lib32_replaces = lib32
        .get("replaces")
        .and_then(|v| v.as_array())
        .expect("expected alternatives.lib32.replaces array");

    assert_eq!(provides.len(), 2);
    assert_eq!(provides[0].as_str(), Some("editor"));
    assert_eq!(provides[1].as_str(), Some("sh"));
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0].as_str(), Some("nano"));
    assert_eq!(conflicts[1].as_str(), Some("busybox-sh"));
    assert_eq!(replaces.len(), 1);
    assert_eq!(replaces[0].as_str(), Some("vi"));
    assert_eq!(lib32_replaces.len(), 1);
    assert_eq!(lib32_replaces[0].as_str(), Some("lib32-vi"));
}

#[test]
fn spec_to_minimal_toml_supports_metapackage_without_sources() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo-meta".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "Meta package".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: Build {
            build_type: BuildType::Meta,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies {
            build: Vec::new(),
            runtime: vec!["foo".into(), "bar".into()],
            test: Vec::new(),
            optional: Vec::new(),
            groups: vec!["base".into()],
            lib32: None,
        },
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    assert!(toml.contains("type = \"meta\""));
    assert!(!toml.contains("[[source]]"));

    let val: toml::Value = toml::from_str(&toml).unwrap();
    assert!(val.get("source").is_none());
}

#[test]
fn spec_to_minimal_toml_includes_manual_sources() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "vertex-keyring".into(),
            real_name: None,
            version: "1.0.0".into(),
            revision: 1,
            description: "keyring".into(),
            homepage: "https://www.vertexlinux.net".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources: vec![
            ManualSource {
                file: Some("vertex.pub".into()),
                files: Vec::new(),
                url: None,
                urls: Vec::new(),
                sha256: None,
                dest: Some("usr/share/depot/keys/public/vertex.pub".into()),
            },
            ManualSource {
                file: None,
                files: Vec::new(),
                url: Some("file:///tmp/vertex.minisig".into()),
                urls: Vec::new(),
                sha256: Some("skip".into()),
                dest: Some("usr/share/depot/keys/sign/vertex.minisig".into()),
            },
        ],
        source: Vec::new(),
        build: Build {
            build_type: BuildType::Custom,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let toml = spec_to_minimal_toml(&spec).unwrap();
    assert!(toml.contains("[[manual_sources]]"));
    assert!(toml.contains("file = \"vertex.pub\""));
    assert!(toml.contains("url = \"file:///tmp/vertex.minisig\""));
    assert!(toml.contains("dest = \"usr/share/depot/keys/public/vertex.pub\""));
    assert!(!toml.contains("sha256 = \"skip\""));

    let val: toml::Value = toml::from_str(&toml).unwrap();
    let arr = val
        .get("manual_sources")
        .and_then(|v| v.as_array())
        .expect("expected manual_sources array");
    assert_eq!(arr.len(), 2);
}

#[test]
fn compute_sha256_for_local_path_and_file_url() {
    use sha2::Digest as TestDigest;
    use sha2::Sha256 as TestSha256;
    use tempfile::NamedTempFile;

    let mut tmp = NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tmp, b"abc").unwrap();
    let expected = {
        let mut h = TestSha256::new();
        h.update(b"abc");
        crate::hex::encode_lower(h.finalize())
    };

    // plain path
    let p = tmp.path().to_str().unwrap().to_string();
    assert_eq!(compute_sha256_for_url(&p).unwrap(), expected);

    // file:// URL
    let file_url = format!("file://{}", tmp.path().to_str().unwrap());
    assert_eq!(compute_sha256_for_url(&file_url).unwrap(), expected);
}

#[test]
fn expand_known_package_vars_replaces_name_and_version_patterns() {
    let input = "https://example.com/$name/${name}-$version-${version}.tar.xz";
    let out = expand_known_package_vars(input, "python", "3.13.1");
    assert_eq!(
        out,
        "https://example.com/python/python-3.13.1-3.13.1.tar.xz"
    );
}
