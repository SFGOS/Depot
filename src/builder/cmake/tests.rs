use super::*;
use crate::package::{Build, BuildFlags, BuildType, PackageInfo, PackageSpec};
use crate::test_support::TestEnv;
use tempfile::tempdir;

#[test]
fn test_expand_env_vars_replaces_vars() {
    // Set a test env var
    let mut env = TestEnv::new();
    env.set_var("DEPOT_TEST_FOO", "bar");
    let input = "$DEPOT_TEST_FOO and ${DEPOT_TEST_FOO}";
    let out = expand_env_vars(input);
    assert!(out.contains("bar"));
    assert_eq!(out, "bar and bar");
}

#[test]
fn test_expand_with_envs_prefers_provided_envs() {
    let envs = vec![
        ("CXX".to_string(), "my-cxx".to_string()),
        ("CC".to_string(), "my-cc".to_string()),
    ];
    let s = "-DCMAKE_C_COMPILER=$CC -DCMAKE_CXX_COMPILER=${CXX} -DROOT=$HOME";
    let out = expand_with_envs(s, &envs);
    assert!(out.contains("my-cc"));
    assert!(out.contains("my-cxx"));
    // $HOME should be expanded from process env (may be present)
}

#[test]
fn test_expand_with_envs_expands_host_build_dir() {
    let envs = vec![(
        crate::builder::DEPOT_BUILD_HOST_DIR_ENV.to_string(),
        "/tmp/build-host".to_string(),
    )];
    let out = expand_with_envs("-DTOOLS_DIR=$DEPOT_BUILD_HOST_DIR/bin", &envs);
    assert_eq!(out, "-DTOOLS_DIR=/tmp/build-host/bin");
}

#[test]
fn test_num_cpus_at_least_one() {
    let n = num_cpus();
    assert!(n >= 1);
}

#[test]
fn test_phase_targets_merges_singular_and_plural() {
    assert_eq!(
        phase_targets("bootstrap", &["stage1".into(), "stage2".into()]),
        vec![
            "bootstrap".to_string(),
            "stage1".to_string(),
            "stage2".to_string()
        ]
    );
    assert!(phase_targets("", &[]).is_empty());
}

#[test]
fn test_cmake_uses_default_ctest_without_explicit_targets() {
    assert!(cmake_uses_default_ctest(&BuildFlags::default()));

    let explicit_single = BuildFlags {
        make_test_target: "test".into(),
        ..BuildFlags::default()
    };
    assert!(!cmake_uses_default_ctest(&explicit_single));

    let explicit_many = BuildFlags {
        make_test_targets: vec!["check".into()],
        ..BuildFlags::default()
    };
    assert!(!cmake_uses_default_ctest(&explicit_many));
}

#[test]
fn test_cmake_generator_for_make_exec_detects_ninja_and_make() {
    assert_eq!(cmake_generator_for_make_exec("ninja"), Some("Ninja"));
    assert_eq!(
        cmake_generator_for_make_exec("/usr/bin/gmake"),
        Some("Unix Makefiles")
    );
    assert_eq!(cmake_generator_for_make_exec("samurai"), None);
}

#[test]
fn test_cmake_configure_flag_detectors() {
    assert!(cmake_configure_flags_specify_generator(&[
        "-G".to_string(),
        "Ninja".to_string()
    ]));
    assert!(cmake_configure_flags_specify_generator(&[
        "--generator=Unix Makefiles".to_string()
    ]));
    assert!(!cmake_configure_flags_specify_generator(&[
        "-DCMAKE_BUILD_TYPE=Release".to_string()
    ]));

    assert!(cmake_configure_flags_set_make_program(&[
        "-DCMAKE_MAKE_PROGRAM=/usr/bin/ninja".to_string()
    ]));
    assert!(!cmake_configure_flags_set_make_program(&[
        "-DCMAKE_C_COMPILER=clang".to_string()
    ]));
}

#[test]
fn test_cmake_cache_entry_value_supports_plain_and_typed_entries() {
    let flags = vec![
        "-DCMAKE_INSTALL_PREFIX=/usr".to_string(),
        "-DCMAKE_INSTALL_LIBDIR:PATH=/usr/lib64".to_string(),
    ];

    assert_eq!(
        cmake_cache_entry_value(&flags, "CMAKE_INSTALL_PREFIX"),
        Some("/usr")
    );
    assert_eq!(
        cmake_cache_entry_value(&flags, "CMAKE_INSTALL_LIBDIR"),
        Some("/usr/lib64")
    );
    assert_eq!(
        cmake_cache_entry_value(&flags, "CMAKE_INSTALL_BINDIR"),
        None
    );
}

#[test]
fn test_effective_cmake_install_prefix_prefers_explicit_configure_flag() {
    let flags = BuildFlags {
        prefix: "/usr".into(),
        configure: vec!["-DCMAKE_INSTALL_PREFIX=/opt/soundtouch".into()],
        ..BuildFlags::default()
    };

    assert_eq!(effective_cmake_install_prefix(&flags), "/opt/soundtouch");
}

#[test]
fn test_cmake_dir_value_for_prefix_converts_prefix_owned_absolute_paths() {
    assert_eq!(cmake_dir_value_for_prefix("/usr", "/usr/lib".into()), "lib");
    assert_eq!(
        cmake_dir_value_for_prefix("/usr", "/usr/share/man".into()),
        "share/man"
    );
}

#[test]
fn test_cmake_dir_value_for_prefix_keeps_non_prefix_absolute_paths() {
    assert_eq!(cmake_dir_value_for_prefix("/usr", "/etc".into()), "/etc");
    assert_eq!(
        cmake_dir_value_for_prefix("/opt/pkg", "/usr/bin".into()),
        "/usr/bin"
    );
}

#[test]
fn test_cmake_install_dir_args_include_prefix_relative_defaults() {
    let args = cmake_install_dir_args(&BuildFlags::default(), "/usr");
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_BINDIR=bin"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_SBINDIR=bin"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_LIBDIR=lib"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_LIBEXECDIR=lib"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_SYSCONFDIR=/etc"));
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_INSTALL_LOCALSTATEDIR=/var")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_INSTALL_SHAREDSTATEDIR=/var/lib")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_INSTALL_INCLUDEDIR=include")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_INSTALL_DATAROOTDIR=share")
    );
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_DATADIR=share"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_MANDIR=share/man"));
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_INSTALL_INFODIR=share/info")
    );
}

#[test]
fn test_cmake_install_dir_args_use_lib32_defaults() {
    let flags = BuildFlags {
        lib32_variant: true,
        ..BuildFlags::default()
    };

    let args = cmake_install_dir_args(&flags, "/usr");
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_LIBDIR=lib32"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_LIBEXECDIR=lib32"));
}

#[test]
fn test_cmake_lib32_target_args_include_compiler_target_defaults() {
    let flags = BuildFlags {
        lib32_variant: true,
        chost: "x86_64-sfg-linux-gnu".into(),
        ..BuildFlags::default()
    };

    let args = cmake_lib32_target_args(&flags, None);
    assert!(args.iter().any(|a| a == "-DCMAKE_SYSTEM_PROCESSOR=i686"));
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_C_COMPILER_TARGET=i686-sfg-linux-gnu")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_CXX_COMPILER_TARGET=i686-sfg-linux-gnu")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_ASM_COMPILER_TARGET=i686-sfg-linux-gnu")
    );
}

#[test]
fn test_cmake_lib32_target_args_respect_explicit_overrides() {
    let flags = BuildFlags {
        lib32_variant: true,
        chost: "x86_64-sfg-linux-gnu".into(),
        configure: vec![
            "-DCMAKE_C_COMPILER_TARGET=i686-custom-linux-gnu".into(),
            "-DCMAKE_SYSTEM_PROCESSOR=i686".into(),
        ],
        ..BuildFlags::default()
    };

    let args = cmake_lib32_target_args(&flags, None);
    assert!(
        !args
            .iter()
            .any(|a| a.starts_with("-DCMAKE_C_COMPILER_TARGET="))
    );
    assert!(
        !args
            .iter()
            .any(|a| a.starts_with("-DCMAKE_SYSTEM_PROCESSOR="))
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_CXX_COMPILER_TARGET=i686-sfg-linux-gnu")
    );
}

#[test]
fn test_cmake_depot_sysroot_args_skip_live_rootfs() {
    let args = cmake_depot_sysroot_args(&BuildFlags::default(), "/");
    assert!(args.is_empty());
}

#[test]
fn test_cmake_depot_sysroot_args_include_non_live_rootfs_defaults() {
    let args = cmake_depot_sysroot_args(&BuildFlags::default(), "/tmp/depot-root");
    assert!(args.iter().any(|a| a == "-DCMAKE_SYSROOT=/tmp/depot-root"));
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_FIND_ROOT_PATH_MODE_PROGRAM=NEVER")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_FIND_ROOT_PATH_MODE_LIBRARY=ONLY")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_FIND_ROOT_PATH_MODE_INCLUDE=ONLY")
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_FIND_ROOT_PATH_MODE_PACKAGE=ONLY")
    );
}

#[test]
fn test_cmake_depot_sysroot_args_respect_explicit_configure_overrides() {
    let flags = BuildFlags {
        configure: vec![
            "-DCMAKE_SYSROOT=/opt/custom-root".into(),
            "-DCMAKE_FIND_ROOT_PATH_MODE_LIBRARY:STRING=BOTH".into(),
        ],
        ..BuildFlags::default()
    };

    let args = cmake_depot_sysroot_args(&flags, "/tmp/depot-root");
    assert!(!args.iter().any(|a| a.starts_with("-DCMAKE_SYSROOT=")));
    assert!(
        !args
            .iter()
            .any(|a| a.starts_with("-DCMAKE_FIND_ROOT_PATH_MODE_LIBRARY="))
    );
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_FIND_ROOT_PATH_MODE_PROGRAM=NEVER")
    );
}

#[test]
fn test_cmake_install_dir_args_respect_explicit_user_overrides() {
    let flags = BuildFlags {
        configure: vec![
            "-DCMAKE_INSTALL_SBINDIR=/sbin".to_string(),
            "-DCMAKE_INSTALL_LIBDIR:PATH=/custom/lib".to_string(),
            "-DCMAKE_INSTALL_DATADIR=/custom/share".to_string(),
        ],
        ..BuildFlags::default()
    };

    let args = cmake_install_dir_args(&flags, "/usr");
    assert!(
        !args
            .iter()
            .any(|a| a.starts_with("-DCMAKE_INSTALL_SBINDIR="))
    );
    assert!(
        !args
            .iter()
            .any(|a| a.starts_with("-DCMAKE_INSTALL_LIBDIR="))
    );
    assert!(
        !args
            .iter()
            .any(|a| a.starts_with("-DCMAKE_INSTALL_DATADIR="))
    );
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_BINDIR=bin"));
}

#[test]
fn test_cmake_install_dir_args_convert_custom_prefix_owned_dirs() {
    let flags = BuildFlags {
        prefix: "/opt/soundtouch".into(),
        bindir: "/opt/soundtouch/bin".into(),
        libdir: "/opt/soundtouch/lib64".into(),
        includedir: "/opt/soundtouch/include/soundtouch".into(),
        datadir: "/opt/soundtouch/share".into(),
        mandir: "/opt/soundtouch/share/man".into(),
        ..BuildFlags::default()
    };

    let args = cmake_install_dir_args(&flags, effective_cmake_install_prefix(&flags));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_BINDIR=bin"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_LIBDIR=lib64"));
    assert!(
        args.iter()
            .any(|a| a == "-DCMAKE_INSTALL_INCLUDEDIR=include/soundtouch")
    );
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_DATADIR=share"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_MANDIR=share/man"));
}

#[test]
fn test_cmake_install_dir_args_keep_absolute_dirs_outside_prefix() {
    let flags = BuildFlags {
        prefix: "/opt/soundtouch".into(),
        bindir: "/usr/bin".into(),
        sysconfdir: "/etc".into(),
        ..BuildFlags::default()
    };

    let args = cmake_install_dir_args(&flags, effective_cmake_install_prefix(&flags));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_BINDIR=/usr/bin"));
    assert!(args.iter().any(|a| a == "-DCMAKE_INSTALL_SYSCONFDIR=/etc"));
}

#[test]
fn resolve_actual_src_prefers_srcdir_then_specdir_and_handles_absolute() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("srcroot");
    let spec_dir = tmp.path().join("specdir");
    let external = tmp.path().join("external");
    let expanded = src_root.join("x-1.0").join("sub");
    std::fs::create_dir_all(src_root.join("sub")).unwrap();
    std::fs::create_dir_all(&expanded).unwrap();
    // create directories for candidates
    std::fs::create_dir_all(spec_dir.join("../llvm")).unwrap();
    std::fs::create_dir_all(&external).unwrap();

    let spec = PackageSpec {
        package: PackageInfo {
            name: "x".into(),
            real_name: None,
            version: "1.0".into(),
            revision: 1,
            description: "".into(),
            homepage: "".into(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: vec!["MIT".into()],
        },
        packages: Vec::new(),
        alternatives: Default::default(),
        manual_sources: Vec::new(),
        source: Vec::new(),
        build: Build {
            build_type: BuildType::CMake,
            flags: BuildFlags {
                source_subdir: "sub".into(),
                ..BuildFlags::default()
            },
        },
        dependencies: Default::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: spec_dir.clone(),
    };

    // case: relative path under src_dir
    let p = resolve_actual_src(&spec, &src_root).unwrap();
    assert!(p.ends_with("sub"));

    // case: ../llvm should resolve relative to spec_dir
    let mut spec2 = spec.clone();
    spec2.build.flags.source_subdir = "../llvm".into();
    let p2 = resolve_actual_src(&spec2, &src_root).unwrap();
    assert!(p2.ends_with("llvm"));

    // case: absolute path
    let mut spec3 = spec.clone();
    spec3.build.flags.source_subdir = external.to_string_lossy().into_owned();
    let p3 = resolve_actual_src(&spec3, &src_root).unwrap();
    assert_eq!(p3, external);

    // case: variable expansion in source_subdir
    let mut spec4 = spec.clone();
    spec4.build.flags.source_subdir = "$name-$version/sub".into();
    let p4 = resolve_actual_src(&spec4, &src_root).unwrap();
    assert_eq!(p4, expanded);
}
