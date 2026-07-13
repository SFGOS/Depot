use super::*;
use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, Source};
use crate::test_support::TestEnv;
use tempfile::tempdir;

#[test]
fn test_num_cpus_at_least_one() {
    let n = num_cpus();
    assert!(n >= 1);
}

#[test]
fn test_meson_setup_args_include_configure_flags() {
    let flags = BuildFlags {
        prefix: "/usr".to_string(),
        configure: vec!["-Dmanpages=false".to_string()],
        ..BuildFlags::default()
    };

    let args = meson_setup_args(&flags, None, &[]);
    assert!(args.iter().any(|a| a == "-Dmanpages=false"));
    assert!(args.iter().any(|a| a == "--prefix=/usr"));
    assert!(args.iter().any(|a| a == "--buildtype=release"));
}

#[test]
fn test_meson_setup_args_expand_host_build_dir() {
    let flags = BuildFlags {
        configure: vec!["-Dtools_dir=$DEPOT_BUILD_HOST_DIR/bin".into()],
        ..BuildFlags::default()
    };

    let args = meson_setup_args(
        &flags,
        None,
        &[(
            crate::builder::DEPOT_BUILD_HOST_DIR_ENV.to_string(),
            "/tmp/build-host".to_string(),
        )],
    );
    assert!(args.iter().any(|a| a == "-Dtools_dir=/tmp/build-host/bin"));
}

#[test]
fn test_configure_pkg_config_env_uses_lib32_dirs_and_pkgconf() -> Result<()> {
    let tmp = tempdir()?;
    let pkgconf = tmp.path().join("pkgconf");
    std::fs::write(&pkgconf, "#!/bin/sh\nexit 0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&pkgconf)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&pkgconf, perms)?;
    }

    let mut env = TestEnv::new();
    env.set_var("PATH", tmp.path());
    env.set_var("PKG_CONFIG", "");

    let mut env_vars = Vec::new();
    let flags = BuildFlags {
        lib32_variant: true,
        rootfs: "/".into(),
        ..BuildFlags::default()
    };
    configure_pkg_config_env(&mut env_vars, &flags, None);

    assert!(
        env_vars
            .iter()
            .any(|(k, v)| k == "PKG_CONFIG" && v.ends_with("/pkgconf"))
    );
    assert!(env_vars.iter().any(|(k, v)| {
        k == "PKG_CONFIG_LIBDIR" && v == "/usr/lib32/pkgconfig:/usr/share/pkgconfig"
    }));
    Ok(())
}

#[test]
fn test_generate_lib32_meson_cross_file_writes_pkg_config_binary_when_available() -> Result<()> {
    let tmp = tempdir()?;
    let tools = tempdir()?;
    let pkgconf = tools.path().join("pkgconf");
    std::fs::write(&pkgconf, "#!/bin/sh\nexit 0\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&pkgconf)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&pkgconf, perms)?;
    }

    let mut env = TestEnv::new();
    env.set_var("PATH", tools.path());
    env.set_var("PKG_CONFIG", "");

    let flags = BuildFlags {
        chost: "x86_64-sfg-linux-gnu".into(),
        ..BuildFlags::default()
    };

    let path = generate_lib32_meson_cross_file(&flags, tmp.path())?;
    let content = std::fs::read_to_string(path)?;
    assert!(content.contains("pkg-config = '/"));
    assert!(content.contains("pkgconf"));
    Ok(())
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
fn test_meson_setup_args_use_lib32_defaults() {
    let flags = BuildFlags {
        lib32_variant: true,
        ..BuildFlags::default()
    };

    let args = meson_setup_args(&flags, None, &[]);
    assert!(args.iter().any(|a| a == "--libdir=/usr/lib32"));
    assert!(args.iter().any(|a| a == "--libexecdir=/usr/lib32"));
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
        strip: "llvm-strip".to_string(),
        ld: "ld.lld".to_string(),
        nm: "llvm-nm".to_string(),
        objcopy: "llvm-objcopy".to_string(),
        objdump: "llvm-objdump".to_string(),
        readelf: "llvm-readelf".to_string(),
        ..BuildFlags::default()
    };

    let path = generate_lib32_meson_cross_file(&flags, tmp.path())?;
    let content = fs::read_to_string(path)?;
    assert!(content.contains("Generated by depot for target: i686-sfg-linux-gnu"));
    assert!(content.contains("c = ['clang', '-m32', '--target=i686-sfg-linux-gnu']"));
    assert!(content.contains("cpp = ['clang++', '-m32', '--target=i686-sfg-linux-gnu']"));
    assert!(content.contains("strip = 'llvm-strip'"));
    assert!(content.contains("ld = 'ld.lld'"));
    assert!(content.contains("nm = 'llvm-nm'"));
    assert!(content.contains("objcopy = 'llvm-objcopy'"));
    assert!(content.contains("objdump = 'llvm-objdump'"));
    assert!(content.contains("readelf = 'llvm-readelf'"));
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
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "d".into(),
            homepage: "h".into(),
            abi_breaking: false,
            built_against: Vec::new(),
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
