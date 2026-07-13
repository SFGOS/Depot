use super::*;
use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};
use crate::test_support::TestEnv;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;

fn mk_spec(cflags: Vec<&str>, ldflags: Vec<&str>) -> PackageSpec {
    let flags = BuildFlags {
        cflags: cflags.into_iter().map(String::from).collect(),
        ldflags: ldflags.into_iter().map(String::from).collect(),
        ..BuildFlags::default()
    };
    PackageSpec {
        package: PackageInfo {
            name: "env-test".into(),
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
        source: vec![crate::package::Source {
            url: "https://example.test/src.tar.gz".into(),
            sha256: "abc".into(),
            extract_dir: "src".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Custom,
            flags,
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    }
}

fn env_value<'a>(env: &'a EnvVars, key: &str) -> Option<&'a str> {
    env.iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.as_str())
}

#[test]
fn test_prepare_command() {
    let mut cmd = Command::new("ls");
    // Set an env var that should be cleared
    cmd.env("FORBIDDEN", "value");
    // Set PATH manually in the current process to ensure it's picked up if it exists
    let mut env = TestEnv::new();
    env.set_var("PATH", "/usr/bin");
    env.set_var("HOME", "/home/test");
    env.set_var("SHELL", "/bin/zsh");
    env.set_var("DEPOT_ROOTFS", "/my/rootfs");
    env.set_var("TERM", "xterm-256color");
    env.set_var("CLICOLOR_FORCE", "1");

    prepare_command(&mut cmd, &vec![("MYVAR".to_string(), "myval".to_string())]);

    let envs: HashMap<_, _> = cmd.get_envs().collect();
    assert!(envs.contains_key(OsStr::new("PATH")));
    assert!(envs.contains_key(OsStr::new("HOME")));
    assert!(!envs.contains_key(OsStr::new("FORBIDDEN")));
    assert_eq!(
        envs.get(OsStr::new("SHELL")),
        Some(&Some(std::ffi::OsString::from("/bin/sh").as_os_str()))
    );
    assert_eq!(
        envs.get(OsStr::new("MYVAR")),
        Some(&Some(std::ffi::OsString::from("myval").as_os_str()))
    );
    // DEPOT_ROOTFS should be preserved from the parent environment
    assert_eq!(
        envs.get(OsStr::new("DEPOT_ROOTFS")),
        Some(&Some(std::ffi::OsString::from("/my/rootfs").as_os_str()))
    );
    assert_eq!(
        envs.get(OsStr::new("TERM")),
        Some(&Some(
            std::ffi::OsString::from("xterm-256color").as_os_str()
        ))
    );
    assert_eq!(
        envs.get(OsStr::new("CLICOLOR_FORCE")),
        Some(&Some(std::ffi::OsString::from("1").as_os_str()))
    );
}

#[test]
fn test_prepare_command_preserves_destdir() {
    let mut cmd = std::process::Command::new("ls");
    let mut env = TestEnv::new();
    env.set_var("DESTDIR", "/tmp/dest");
    prepare_command(&mut cmd, &Vec::new());
    let envs: HashMap<_, _> = cmd.get_envs().collect();
    assert_eq!(
        envs.get(OsStr::new("DESTDIR")),
        Some(&Some(std::ffi::OsString::from("/tmp/dest").as_os_str()))
    );
}

#[test]
fn test_prepare_command_preserves_rust_toolchain_homes() {
    let mut cmd = std::process::Command::new("ls");
    let mut env = TestEnv::new();
    env.set_var("CARGO_HOME", "/var/cache/cargo-home");
    env.set_var("RUSTUP_HOME", "/var/cache/rustup-home");
    prepare_command(&mut cmd, &Vec::new());
    let envs: HashMap<_, _> = cmd.get_envs().collect();
    assert_eq!(
        envs.get(OsStr::new("CARGO_HOME")),
        Some(&Some(
            std::ffi::OsString::from("/var/cache/cargo-home").as_os_str()
        ))
    );
    assert_eq!(
        envs.get(OsStr::new("RUSTUP_HOME")),
        Some(&Some(
            std::ffi::OsString::from("/var/cache/rustup-home").as_os_str()
        ))
    );
}

#[test]
fn test_build_tool_package_option_maps_supported_builders() {
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
fn test_static_build_args_skip_disable_static_when_no_delete_static_enabled() {
    let args = static_build_args_for_request(BuildType::Autotools, Some(false), true);
    assert!(args.is_empty());

    let args = static_build_args_for_request(BuildType::CMake, Some(false), true);
    assert!(args.is_empty());
}

#[test]
fn test_static_build_args_keep_other_requested_modes() {
    assert_eq!(
        static_build_args_for_request(BuildType::Autotools, Some(false), false),
        vec![
            "--enable-shared".to_string(),
            "--disable-static".to_string()
        ]
    );
    assert_eq!(
        static_build_args_for_request(BuildType::CMake, Some(false), false),
        vec!["-DBUILD_SHARED_LIBS=ON".to_string()]
    );
    assert_eq!(
        static_build_args_for_request(BuildType::Meson, Some(false), false),
        vec!["-Ddefault_library=shared".to_string()]
    );
    assert_eq!(
        static_build_args_for_request(BuildType::Perl, Some(false), false),
        vec!["LINKTYPE=dynamic".to_string()]
    );
    assert_eq!(
        static_build_args_for_request(BuildType::Meson, Some(true), true),
        vec!["-Ddefault_library=static".to_string()]
    );
}

#[test]
fn test_standard_build_env_exports_native_linker_and_cpp() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.ranlib = "llvm-ranlib".to_string();
    spec.build.flags.strip = "llvm-strip".to_string();
    spec.build.flags.ld = "ld.lld".to_string();
    spec.build.flags.nm = "llvm-nm".to_string();
    spec.build.flags.objcopy = "llvm-objcopy".to_string();
    spec.build.flags.objdump = "llvm-objdump".to_string();
    spec.build.flags.readelf = "llvm-readelf".to_string();
    spec.build.flags.cpp = "clang-cpp".to_string();

    let env = standard_build_env(&spec, None, true, true);
    assert!(env.iter().any(|(k, v)| k == "RANLIB" && v == "llvm-ranlib"));
    assert!(env.iter().any(|(k, v)| k == "STRIP" && v == "llvm-strip"));
    assert!(env.iter().any(|(k, v)| k == "LD" && v == "ld.lld"));
    assert!(env.iter().any(|(k, v)| k == "NM" && v == "llvm-nm"));
    assert!(
        env.iter()
            .any(|(k, v)| k == "OBJCOPY" && v == "llvm-objcopy")
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "OBJDUMP" && v == "llvm-objdump")
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "READELF" && v == "llvm-readelf")
    );
    assert!(env.iter().any(|(k, v)| k == "CPP" && v == "clang-cpp"));
}

#[test]
fn test_standard_build_env_exports_tool_dir_and_expands_tool_commands() {
    let mut spec = mk_spec(
        vec!["--gcc-toolchain=$TOOL_DIR"],
        vec!["-B$TOOL_DIR", "-Wl,--as-needed"],
    );
    spec.build.flags.tool_dir = "/opt/depot-tools/bin".to_string();
    spec.build.flags.cc = "$TOOL_DIR/clang".to_string();
    spec.build.flags.cxx = "$TOOL_DIR/clang++".to_string();
    spec.build.flags.ar = "$TOOL_DIR/llvm-ar".to_string();
    spec.build.flags.ranlib = "$TOOL_DIR/llvm-ranlib".to_string();
    spec.build.flags.ld = "$TOOL_DIR/ld.lld".to_string();
    spec.build.flags.env_vars = vec!["LLVM_CONFIG=$TOOL_DIR/llvm-config".to_string()];

    let env = standard_build_env(&spec, None, true, true);

    assert_eq!(env_value(&env, "TOOL_DIR"), Some("/opt/depot-tools/bin"));
    assert_eq!(env_value(&env, "CC"), Some("/opt/depot-tools/bin/clang"));
    assert_eq!(env_value(&env, "CXX"), Some("/opt/depot-tools/bin/clang++"));
    assert_eq!(env_value(&env, "AR"), Some("/opt/depot-tools/bin/llvm-ar"));
    assert_eq!(
        env_value(&env, "RANLIB"),
        Some("/opt/depot-tools/bin/llvm-ranlib")
    );
    assert_eq!(env_value(&env, "LD"), Some("/opt/depot-tools/bin/ld.lld"));
    assert_eq!(
        env_value(&env, "CFLAGS"),
        Some("--gcc-toolchain=/opt/depot-tools/bin")
    );
    assert_eq!(
        env_value(&env, "LDFLAGS"),
        Some("-B/opt/depot-tools/bin -Wl,--as-needed")
    );
    assert_eq!(
        env_value(&env, "LLVM_CONFIG"),
        Some("/opt/depot-tools/bin/llvm-config")
    );
}

#[test]
fn test_standard_build_env_cross_uses_package_tool_overrides() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.cc = "/tools/bin/clang".to_string();
    spec.build.flags.cxx = "/tools/bin/clang++".to_string();
    spec.build.flags.ar = "/tools/bin/llvm-ar".to_string();
    spec.build.flags.ranlib = "/tools/bin/llvm-ranlib".to_string();
    spec.build.flags.strip = "/tools/bin/llvm-strip".to_string();
    spec.build.flags.ld = "/tools/bin/ld.lld".to_string();
    spec.build.flags.nm = "/tools/bin/llvm-nm".to_string();
    spec.build.flags.objcopy = "/tools/bin/llvm-objcopy".to_string();
    spec.build.flags.objdump = "/tools/bin/llvm-objdump".to_string();
    spec.build.flags.readelf = "/tools/bin/llvm-readelf".to_string();
    spec.build.flags.cpp = "/tools/bin/clang-cpp".to_string();
    let cross = CrossConfig {
        prefix: "x86_64-test-linux-gnu".into(),
        cc: "x86_64-test-linux-gnu-gcc".into(),
        cxx: "x86_64-test-linux-gnu-g++".into(),
        ar: "x86_64-test-linux-gnu-ar".into(),
        ranlib: "x86_64-test-linux-gnu-ranlib".into(),
        strip: "x86_64-test-linux-gnu-strip".into(),
        ld: "x86_64-test-linux-gnu-ld".into(),
        nm: "x86_64-test-linux-gnu-nm".into(),
        objcopy: "x86_64-test-linux-gnu-objcopy".into(),
        objdump: "x86_64-test-linux-gnu-objdump".into(),
        readelf: "x86_64-test-linux-gnu-readelf".into(),
    };

    let env = standard_build_env(&spec, Some(&cross), true, true);

    assert_eq!(env_value(&env, "CC"), Some("/tools/bin/clang"));
    assert_eq!(env_value(&env, "CXX"), Some("/tools/bin/clang++"));
    assert_eq!(env_value(&env, "AR"), Some("/tools/bin/llvm-ar"));
    assert_eq!(env_value(&env, "RANLIB"), Some("/tools/bin/llvm-ranlib"));
    assert_eq!(env_value(&env, "STRIP"), Some("/tools/bin/llvm-strip"));
    assert_eq!(env_value(&env, "LD"), Some("/tools/bin/ld.lld"));
    assert_eq!(env_value(&env, "NM"), Some("/tools/bin/llvm-nm"));
    assert_eq!(env_value(&env, "OBJCOPY"), Some("/tools/bin/llvm-objcopy"));
    assert_eq!(env_value(&env, "OBJDUMP"), Some("/tools/bin/llvm-objdump"));
    assert_eq!(env_value(&env, "READELF"), Some("/tools/bin/llvm-readelf"));
    assert_eq!(env_value(&env, "CPP"), Some("/tools/bin/clang-cpp"));
    assert_eq!(
        env_value(&env, "CROSS_PREFIX"),
        Some("x86_64-test-linux-gnu")
    );
}

#[test]
fn test_standard_build_env_exports_effective_carch_for_cross_and_lib32() {
    let spec = mk_spec(Vec::new(), Vec::new());
    let cross = CrossConfig {
        prefix: "aarch64-linux-gnu".into(),
        cc: "aarch64-linux-gnu-gcc".into(),
        cxx: "aarch64-linux-gnu-g++".into(),
        ar: "aarch64-linux-gnu-ar".into(),
        ranlib: "aarch64-linux-gnu-ranlib".into(),
        strip: "aarch64-linux-gnu-strip".into(),
        ld: "aarch64-linux-gnu-ld".into(),
        nm: "aarch64-linux-gnu-nm".into(),
        objcopy: "aarch64-linux-gnu-objcopy".into(),
        objdump: "aarch64-linux-gnu-objdump".into(),
        readelf: "aarch64-linux-gnu-readelf".into(),
    };

    let cross_env = standard_build_env(&spec, Some(&cross), true, true);
    assert!(
        cross_env
            .iter()
            .any(|(k, v)| k == "CARCH" && v == "aarch64"),
        "expected cross builds to export target CARCH"
    );
    assert!(
        cross_env
            .iter()
            .any(|(k, v)| k == "OBJCOPY" && v == "aarch64-linux-gnu-objcopy"),
        "expected cross builds to export OBJCOPY"
    );
    assert!(
        cross_env
            .iter()
            .any(|(k, v)| k == "OBJDUMP" && v == "aarch64-linux-gnu-objdump"),
        "expected cross builds to export OBJDUMP"
    );
    assert!(
        cross_env
            .iter()
            .any(|(k, v)| k == "READELF" && v == "aarch64-linux-gnu-readelf"),
        "expected cross builds to export READELF"
    );

    let mut lib32_spec = spec.clone();
    lib32_spec.build.flags.lib32_variant = true;
    lib32_spec.build.flags.carch = "x86_64".into();
    let lib32_env = standard_build_env(&lib32_spec, None, true, true);
    assert!(
        lib32_env.iter().any(|(k, v)| k == "CARCH" && v == "i686"),
        "expected lib32 builds to export i686 CARCH"
    );
}

#[test]
fn test_spec_with_target_configure_appends_matching_arch_args() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.configure = vec!["--base".to_string()];
    spec.build
        .flags
        .configure_arch
        .insert("aarch64".to_string(), vec!["--for-aarch64".to_string()]);
    spec.build
        .flags
        .configure_arch
        .insert("x86_64".to_string(), vec!["--for-x86".to_string()]);
    let cross = CrossConfig {
        prefix: "aarch64-linux-gnu".into(),
        cc: "aarch64-linux-gnu-gcc".into(),
        cxx: "aarch64-linux-gnu-g++".into(),
        ar: "aarch64-linux-gnu-ar".into(),
        ranlib: "aarch64-linux-gnu-ranlib".into(),
        strip: "aarch64-linux-gnu-strip".into(),
        ld: "aarch64-linux-gnu-ld".into(),
        nm: "aarch64-linux-gnu-nm".into(),
        objcopy: "aarch64-linux-gnu-objcopy".into(),
        objdump: "aarch64-linux-gnu-objdump".into(),
        readelf: "aarch64-linux-gnu-readelf".into(),
    };

    let adjusted = spec_with_target_configure(&spec, Some(&cross), TargetBuildKind::Primary)
        .expect("expected arch-specific configure args");

    assert_eq!(
        adjusted.build.flags.configure,
        vec!["--base".to_string(), "--for-aarch64".to_string()]
    );
}

#[test]
fn test_spec_with_target_configure_uses_lib32_arch() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.lib32_variant = true;
    spec.build.flags.carch = "x86_64".to_string();
    spec.build
        .flags
        .configure_arch
        .insert("i686".to_string(), vec!["--for-lib32".to_string()]);

    let adjusted = spec_with_target_configure(&spec, None, TargetBuildKind::Lib32)
        .expect("expected lib32 configure args");

    assert_eq!(
        adjusted.build.flags.configure,
        vec!["--for-lib32".to_string()]
    );
}

#[test]
fn test_standard_build_env_respects_export_compiler_flags_toggle() {
    let mut spec = mk_spec(vec!["-O2"], vec!["-Wl,--as-needed"]);
    spec.build.flags.cxxflags = vec!["-O2".into(), "-fno-exceptions".into()];

    let enabled = standard_build_env(&spec, None, true, true);
    assert!(
        enabled.iter().any(|(k, v)| k == "CFLAGS" && v == "-O2"),
        "expected CFLAGS to be exported when enabled"
    );
    assert!(
        enabled
            .iter()
            .any(|(k, v)| k == "CXXFLAGS" && v == "-O2 -fno-exceptions"),
        "expected CXXFLAGS to be exported when enabled"
    );
    assert!(
        enabled
            .iter()
            .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,--as-needed"),
        "expected LDFLAGS to be exported when enabled"
    );

    let disabled = standard_build_env(&spec, None, true, false);
    assert!(
        !disabled.iter().any(|(k, _)| k == "CFLAGS"),
        "expected CFLAGS to be omitted when disabled"
    );
    assert!(
        !disabled.iter().any(|(k, _)| k == "CXXFLAGS"),
        "expected CXXFLAGS to be omitted when disabled"
    );
    assert!(
        !disabled.iter().any(|(k, _)| k == "LDFLAGS"),
        "expected LDFLAGS to be omitted when disabled"
    );

    let mut disabled_by_spec = spec.clone();
    disabled_by_spec.build.flags.no_flags = true;
    let disabled_env = standard_build_env(&disabled_by_spec, None, true, true);
    assert!(
        !disabled_env.iter().any(|(k, _)| k == "CFLAGS"),
        "expected CFLAGS to be omitted when no_flags is set in spec"
    );
    assert!(
        !disabled_env.iter().any(|(k, _)| k == "CXXFLAGS"),
        "expected CXXFLAGS to be omitted when no_flags is set in spec"
    );
    assert!(
        !disabled_env.iter().any(|(k, _)| k == "LDFLAGS"),
        "expected LDFLAGS to be omitted when no_flags is set in spec"
    );
}

#[test]
fn test_standard_build_env_injects_ltoflags_into_compiler_and_linker_flags() {
    let mut spec = mk_spec(vec!["-O2"], vec!["-Wl,--as-needed"]);
    spec.build.flags.cxxflags = vec!["-O2".into()];
    spec.build.flags.ltoflags = vec!["-flto=auto".into(), "-fuse-linker-plugin".into()];
    spec.build.flags.use_lto = true;

    let env = standard_build_env(&spec, None, true, true);
    assert!(
        env.iter()
            .any(|(k, v)| { k == "CFLAGS" && v == "-O2 -flto=auto -fuse-linker-plugin" }),
        "expected LTOFLAGS to be appended to CFLAGS"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "CXXFLAGS" && v == "-O2 -flto=auto -fuse-linker-plugin"),
        "expected LTOFLAGS to be appended to CXXFLAGS"
    );
    assert!(
        env.iter().any(|(k, v)| {
            k == "LDFLAGS" && v == "-Wl,--as-needed -flto=auto -fuse-linker-plugin"
        }),
        "expected LTOFLAGS to be appended to LDFLAGS"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=auto -fuse-linker-plugin"),
        "expected LTOFLAGS variable to be exported"
    );
}

#[test]
fn test_standard_build_env_injects_fuse_ld_into_ldflags() {
    let mut spec = mk_spec(Vec::new(), vec!["-Wl,--as-needed"]);
    spec.build.flags.fuse_ld = "/usr/bin/ld.lld".into();

    let env = standard_build_env(&spec, None, true, true);

    assert_eq!(
        env_value(&env, "LDFLAGS"),
        Some("-fuse-ld=/usr/bin/ld.lld -Wl,--as-needed")
    );
}

#[test]
fn test_standard_build_env_normalizes_fuse_ld_tool_names() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.fuse_ld = "ld.lld".into();

    let env = standard_build_env(&spec, None, true, true);

    assert_eq!(env_value(&env, "LDFLAGS"), Some("-fuse-ld=lld"));
}

#[test]
fn test_standard_build_env_applies_replace_flag_rules() {
    let mut spec = mk_spec(vec!["-D_FORTIFY_SOURCE=3", "-O2"], vec!["-Wl,-O3"]);
    spec.build.flags.cxxflags = vec!["-O2".into(), "-stdlib=libc++".into()];
    spec.build.flags.replace_cflags = vec!["_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".into()];
    spec.build.flags.replace_cxxflags = vec!["-stdlib=libc++=>-stdlib=libstdc++".into()];
    spec.build.flags.replace_ldflags = vec!["-O3=>-O2".into()];
    spec.build.flags.ltoflags = vec!["-flto=auto".into()];
    spec.build.flags.replace_ltoflags = vec!["auto=>thin".into()];
    spec.build.flags.use_lto = true;

    let env = standard_build_env(&spec, None, true, true);
    assert!(
        env.iter()
            .any(|(k, v)| k == "CFLAGS" && v == "-D_FORTIFY_SOURCE=2 -O2 -flto=thin"),
        "expected replace_cflags and replace_ltoflags to be applied"
    );
    assert!(
        env.iter()
            .any(|(k, v)| { k == "CXXFLAGS" && v == "-O2 -stdlib=libstdc++ -flto=thin" }),
        "expected replace_cxxflags to be applied"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,-O2 -flto=thin"),
        "expected replace_ldflags to be applied"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=thin"),
        "expected replace_ltoflags to affect exported LTOFLAGS"
    );
}

#[test]
fn test_standard_build_env_drops_bare_dash_flags() {
    let mut spec = mk_spec(vec!["-O2", "-", ""], vec!["-Wl,--as-needed", "  "]);
    spec.build.flags.cxxflags = vec!["-O2".into(), "-".into(), "-fno-exceptions".into()];
    spec.build.flags.ltoflags = vec!["-".into(), "-flto=thin".into()];
    spec.build.flags.use_lto = true;

    let env = standard_build_env(&spec, None, true, true);
    assert!(
        env.iter()
            .any(|(k, v)| k == "CFLAGS" && v == "-O2 -flto=thin"),
        "expected bare dash entries to be removed from CFLAGS"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "CXXFLAGS" && v == "-O2 -fno-exceptions -flto=thin"),
        "expected bare dash entries to be removed from CXXFLAGS"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,--as-needed -flto=thin"),
        "expected blank and bare dash entries to be removed from LDFLAGS"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=thin"),
        "expected bare dash entries to be removed from LTOFLAGS"
    );
}

#[test]
fn test_standard_build_env_skips_lto_injection_when_disabled() {
    let mut spec = mk_spec(vec!["-O2"], vec!["-Wl,--as-needed"]);
    spec.build.flags.cxxflags = vec!["-O2".into()];
    spec.build.flags.ltoflags = vec!["-flto=auto".into()];
    spec.build.flags.rustltoflags = vec!["-Clinker-plugin-lto".into()];
    spec.build.flags.use_lto = false;

    let env = standard_build_env(&spec, None, true, true);
    assert!(
        env.iter().any(|(k, v)| k == "CFLAGS" && v == "-O2"),
        "expected CFLAGS to remain unchanged when use_lto is false"
    );
    assert!(
        env.iter().any(|(k, v)| k == "CXXFLAGS" && v == "-O2"),
        "expected CXXFLAGS to remain unchanged when use_lto is false"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,--as-needed"),
        "expected LDFLAGS to remain unchanged when use_lto is false"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=auto"),
        "expected LTOFLAGS variable to be exported even when use_lto is false"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "RUSTLTOFLAGS" && v == "-Clinker-plugin-lto"),
        "expected RUSTLTOFLAGS variable to be exported even when use_lto is false"
    );
    assert_eq!(effective_rustflags(&spec.build.flags), Vec::<String>::new());
}

#[test]
fn test_standard_build_env_exports_makeflags() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.makeflags = "-j12 --output-sync=target".to_string();

    let env = standard_build_env(&spec, None, true, true);
    assert!(
        env.iter()
            .any(|(k, v)| k == "MAKEFLAGS" && v == "-j12 --output-sync=target"),
        "expected MAKEFLAGS to be exported from build flags"
    );
}

#[test]
fn test_standard_build_env_exports_install_dir_vars() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.prefix = "/opt/vertex".into();
    spec.build.flags.bindir = "/opt/vertex/bin".into();
    spec.build.flags.sbindir = "/opt/vertex/sbin".into();
    spec.build.flags.libdir = "/opt/vertex/lib".into();
    spec.build.flags.libexecdir = "/opt/vertex/libexec".into();
    spec.build.flags.sysconfdir = "/etc/vertex".into();
    spec.build.flags.localstatedir = "/var".into();
    spec.build.flags.sharedstatedir = "/var/lib".into();
    spec.build.flags.includedir = "/opt/vertex/include".into();
    spec.build.flags.datarootdir = "/opt/vertex/share".into();
    spec.build.flags.datadir = "/opt/vertex/share/data".into();
    spec.build.flags.mandir = "/opt/vertex/share/man".into();
    spec.build.flags.infodir = "/opt/vertex/share/info".into();

    let env = standard_build_env(&spec, None, false, true);

    assert_eq!(env_value(&env, "PREFIX"), Some("/opt/vertex"));
    assert_eq!(env_value(&env, "BINDIR"), Some("/opt/vertex/bin"));
    assert_eq!(env_value(&env, "SBINDIR"), Some("/opt/vertex/sbin"));
    assert_eq!(env_value(&env, "LIBDIR"), Some("/opt/vertex/lib"));
    assert_eq!(env_value(&env, "LIBEXECDIR"), Some("/opt/vertex/libexec"));
    assert_eq!(env_value(&env, "SYSCONFDIR"), Some("/etc/vertex"));
    assert_eq!(env_value(&env, "LOCALSTATEDIR"), Some("/var"));
    assert_eq!(env_value(&env, "SHAREDSTATEDIR"), Some("/var/lib"));
    assert_eq!(env_value(&env, "INCLUDEDIR"), Some("/opt/vertex/include"));
    assert_eq!(env_value(&env, "DATAROOTDIR"), Some("/opt/vertex/share"));
    assert_eq!(env_value(&env, "DATADIR"), Some("/opt/vertex/share/data"));
    assert_eq!(env_value(&env, "MANDIR"), Some("/opt/vertex/share/man"));
    assert_eq!(env_value(&env, "INFODIR"), Some("/opt/vertex/share/info"));
}

#[test]
fn test_standard_build_env_install_dir_vars_use_effective_defaults() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.lib32_variant = true;

    let env = standard_build_env(&spec, None, false, true);

    assert_eq!(env_value(&env, "LIBDIR"), Some("/usr/lib32"));
    assert_eq!(env_value(&env, "LIBEXECDIR"), Some("/usr/lib32"));
    assert_eq!(env_value(&env, "DATAROOTDIR"), Some("/usr/share"));
    assert_eq!(env_value(&env, "DATADIR"), Some("/usr/share"));
}

#[test]
fn test_standard_build_env_exports_passthrough_env() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.passthrough_env = vec!["RUSTFLAGS".into()];

    let mut env = TestEnv::new();
    env.set_var("RUSTFLAGS", "-C target-cpu=native");

    let env = standard_build_env(&spec, None, false, true);
    assert!(
        env.iter()
            .any(|(k, v)| k == "RUSTFLAGS" && v == "-C target-cpu=native"),
        "expected RUSTFLAGS to be copied from parent environment"
    );
}

#[test]
fn test_standard_build_env_exports_declared_env_vars() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.package.version = "2.4.1".to_string();
    spec.spec_dir = PathBuf::from("/tmp/specs/demo");
    spec.build.flags.env_vars = vec![
        "SETUPTOOLS_SCM_PRETEND_VERSION=$version".into(),
        "PYO3_CONFIG_FILE=$specdir/pyo3.toml".into(),
        "PKG_CONFIG_PATH=$LIBDIR/pkgconfig".into(),
    ];

    let env = standard_build_env(&spec, None, false, true);
    assert!(
        env.iter()
            .any(|(k, v)| k == "SETUPTOOLS_SCM_PRETEND_VERSION" && v == "2.4.1"),
        "expected env_vars values to expand package variables"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "PYO3_CONFIG_FILE" && v == "/tmp/specs/demo/pyo3.toml"),
        "expected env_vars values to expand specdir variables"
    );
    assert!(
        env.iter()
            .any(|(k, v)| k == "PKG_CONFIG_PATH" && v == "/usr/lib/pkgconfig"),
        "expected env_vars values to expand install directory variables"
    );
}

#[test]
fn test_standard_build_env_declared_env_vars_override_defaults_and_passthrough() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.cc = "spec-cc".to_string();
    spec.build.flags.passthrough_env = vec!["CC".into()];
    spec.build.flags.env_vars = vec!["CC=custom-cc".into()];

    let mut env = TestEnv::new();
    env.set_var("CC", "host-cc");

    let env = standard_build_env(&spec, None, true, true);
    assert!(
        env.iter().any(|(k, v)| k == "CC" && v == "custom-cc"),
        "expected explicit env_vars assignments to override default and passthrough values"
    );
}

#[test]
fn test_effective_rustflags_applies_replace_rules() {
    let flags = BuildFlags {
        rustflags: vec!["-C".into(), "debuginfo=2".into()],
        replace_rustflags: vec!["debuginfo=2=>opt-level=2".into()],
        ..BuildFlags::default()
    };

    assert_eq!(effective_rustflags(&flags), vec!["-C", "opt-level=2"]);
}

#[test]
fn test_effective_rustflags_appends_rustltoflags_when_enabled() {
    let flags = BuildFlags {
        rustflags: vec!["-C".into(), "opt-level=3".into()],
        rustltoflags: vec!["-Clinker-plugin-lto".into(), "-Cembed-bitcode=yes".into()],
        use_lto: true,
        ..BuildFlags::default()
    };

    assert_eq!(
        effective_rustflags(&flags),
        vec![
            "-C",
            "opt-level=3",
            "-Clinker-plugin-lto",
            "-Cembed-bitcode=yes"
        ]
    );
}

#[test]
fn test_standard_build_env_passthrough_does_not_override_default_vars() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.cc = "spec-cc".to_string();
    spec.build.flags.passthrough_env = vec!["CC".into()];

    let mut env = TestEnv::new();
    env.set_var("CC", "host-cc");

    let env = standard_build_env(&spec, None, true, true);
    assert!(
        env.iter().any(|(k, v)| k == "CC" && v == "spec-cc"),
        "expected default CC to take precedence over passthrough CC"
    );
}

#[test]
fn test_install_dirs_use_defaults_and_lib32_fallbacks() {
    let default_dirs = install_dirs(&BuildFlags::default());
    assert_eq!(default_dirs.bindir, "/usr/bin");
    assert_eq!(default_dirs.sbindir, "/usr/bin");
    assert_eq!(default_dirs.libdir, "/usr/lib");
    assert_eq!(default_dirs.libexecdir, "/usr/lib");
    assert_eq!(default_dirs.datarootdir, "/usr/share");
    assert_eq!(default_dirs.datadir, "/usr/share");

    let lib32_dirs = install_dirs(&BuildFlags {
        lib32_variant: true,
        ..BuildFlags::default()
    });
    assert_eq!(lib32_dirs.libdir, "/usr/lib32");
    assert_eq!(lib32_dirs.libexecdir, "/usr/lib32");
}

#[test]
fn test_build_helper_context_restores_runtime_build_flags() {
    let mut spec = mk_spec(Vec::new(), Vec::new());
    spec.build.flags.lib32_variant = true;
    spec.build.flags.host_build_dir = Some("/tmp/build-host".into());

    let restored = BuildHelperContext::from_spec(&spec).build_flags();
    assert!(restored.lib32_variant);
    assert_eq!(restored.host_build_dir.as_deref(), Some("/tmp/build-host"));
}

#[test]
fn test_install_dirs_respect_explicit_overrides_and_derived_defaults() {
    let dirs = install_dirs(&BuildFlags {
        bindir: "/opt/bin".into(),
        libdir: "/opt/lib64".into(),
        datarootdir: "/opt/share-root".into(),
        ..BuildFlags::default()
    });

    assert_eq!(dirs.bindir, "/opt/bin");
    assert_eq!(dirs.libdir, "/opt/lib64");
    assert_eq!(dirs.libexecdir, "/opt/lib64");
    assert_eq!(dirs.datarootdir, "/opt/share-root");
    assert_eq!(dirs.datadir, "/opt/share-root");
}

#[test]
fn test_install_destdir_path_uses_build_dir_for_lib32() {
    let build_dir = Path::new("/tmp/build");
    let destdir = Path::new("/tmp/pkg");
    assert_eq!(install_destdir_path(build_dir, destdir, false), destdir);
    assert_eq!(
        install_destdir_path(build_dir, destdir, true),
        build_dir.join("destdir")
    );
}

#[test]
fn test_stage_lib32_install_tree_uses_usr_lib32_when_present() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let staging = temp.path().join("staging");
    let dest = temp.path().join("dest");
    fs::create_dir_all(staging.join("usr/lib32"))?;
    fs::create_dir_all(staging.join("usr/bin"))?;
    fs::write(staging.join("usr/lib32/libfoo.so.1"), "lib32")?;
    fs::write(staging.join("usr/bin/foo"), "bin")?;

    stage_lib32_install_tree(&staging, &dest)?;

    assert_eq!(
        fs::read_to_string(dest.join("usr/lib32/libfoo.so.1"))?,
        "lib32"
    );
    assert!(!dest.join("usr/bin/foo").exists());
    Ok(())
}

#[test]
#[cfg(unix)]
fn test_stage_lib32_install_tree_relocates_usr_lib_when_needed() -> Result<()> {
    use std::os::unix::fs as unix_fs;

    let temp = tempfile::tempdir()?;
    let staging = temp.path().join("staging");
    let dest = temp.path().join("dest");
    fs::create_dir_all(staging.join("usr/lib"))?;
    fs::create_dir_all(staging.join("usr/share/man/man1"))?;
    fs::write(staging.join("usr/lib/libfoo.so.1"), "relocated")?;
    fs::write(staging.join("usr/share/man/man1/foo.1"), "manpage")?;
    unix_fs::symlink("libfoo.so.1", staging.join("usr/lib/libfoo.so"))?;

    stage_lib32_install_tree(&staging, &dest)?;

    assert_eq!(
        fs::read_to_string(dest.join("usr/lib32/libfoo.so.1"))?,
        "relocated"
    );
    assert_eq!(
        fs::read_link(dest.join("usr/lib32/libfoo.so"))?,
        PathBuf::from("libfoo.so.1")
    );
    assert!(!dest.join("usr/share/man/man1/foo.1").exists());
    assert!(!dest.join("usr/lib").exists());
    Ok(())
}

#[test]
fn test_stage_lib32_install_tree_preserves_hardlinks() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let temp = tempfile::tempdir()?;
    let staging = temp.path().join("staging");
    let dest = temp.path().join("dest");
    fs::create_dir_all(staging.join("usr/lib32"))?;
    fs::write(staging.join("usr/lib32/libfoo.so.1"), "lib32")?;
    fs::hard_link(
        staging.join("usr/lib32/libfoo.so.1"),
        staging.join("usr/lib32/libfoo-current.so"),
    )?;

    stage_lib32_install_tree(&staging, &dest)?;

    let first = dest.join("usr/lib32/libfoo.so.1").metadata()?;
    let second = dest.join("usr/lib32/libfoo-current.so").metadata()?;
    assert_eq!(first.ino(), second.ino());
    assert_eq!(first.nlink(), 2);
    assert_eq!(second.nlink(), 2);
    Ok(())
}
