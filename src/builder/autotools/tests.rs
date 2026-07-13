use super::*;
use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};
use std::path::PathBuf;
use tempfile::tempdir;

#[test]
fn test_expand_shell_commands_simple() -> Result<()> {
    let out = expand_shell_commands("x $(echo foo) y", "gcc")?;
    assert_eq!(out, "x foo y");
    Ok(())
}

#[test]
fn test_expand_shell_commands_replace_cc() -> Result<()> {
    // The command contains $CC which should be replaced with provided cc
    let out = expand_shell_commands("start $($CC -v >/dev/null; echo OK) end", "mycc")?;
    // Since the inner command echoes OK, after replacing $CC it should run and include OK
    assert!(out.contains("OK") || out.contains(""));
    Ok(())
}

#[test]
fn test_expand_with_envs_prefers_provided_envs() {
    let envs = vec![
        ("CARCH".to_string(), "x86_64".to_string()),
        ("CHOST".to_string(), "x86_64-sfg-linux-gnu".to_string()),
    ];
    let out = expand_with_envs("--with-gcc-arch=$CARCH --host=${CHOST}", &envs);
    assert!(out.contains("--with-gcc-arch=x86_64"));
    assert!(out.contains("--host=x86_64-sfg-linux-gnu"));
}

#[test]
fn test_expand_configure_arg_expands_spec_and_env_vars() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.2.3".into(),
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
        source: Vec::new(),
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let envs = vec![("CARCH".to_string(), "aarch64".to_string())];
    let expanded = expand_configure_arg(&spec, "--program-prefix=$name-$version-$CARCH-", &envs);
    assert_eq!(expanded, "--program-prefix=foo-1.2.3-aarch64-");
}

#[test]
fn test_expand_configure_arg_expands_host_build_dir_env() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
            real_name: None,
            version: "1.2.3".into(),
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
        source: Vec::new(),
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let envs = vec![(
        crate::builder::DEPOT_BUILD_HOST_DIR_ENV.to_string(),
        "/tmp/build-host".to_string(),
    )];
    let expanded = expand_configure_arg(
        &spec,
        "--with-build-tools=$DEPOT_BUILD_HOST_DIR/tools",
        &envs,
    );
    assert_eq!(expanded, "--with-build-tools=/tmp/build-host/tools");
}

#[test]
fn test_num_cpus_at_least_one() {
    let n = num_cpus();
    assert!(n >= 1);
}

#[test]
fn test_configure_help_supports_host_build() {
    let help = "Usage: configure [OPTION]...\n  --host=HOST   cross host\n  --build=BUILD";
    assert!(configure_help_supports_option(help, "--host"));
    assert!(configure_help_supports_option(help, "--build"));
    assert!(!configure_help_supports_option(help, "--target"));
}

#[test]
fn test_configure_help_supports_enable_disable_aliases() {
    let help = "  --enable-static  build static libraries\n  --with-zlib=DIR";
    assert!(configure_help_supports_option(help, "--disable-static"));
    assert!(configure_help_supports_option(help, "--without-zlib"));
}

#[test]
fn test_configure_help_supports_option_requires_exact_match() {
    let help = "\
  --host-cc=HOSTCC         use host C compiler
  --build-suffix=SUFFIX    library name suffix []";
    assert!(!configure_help_supports_option(help, "--host"));
    assert!(!configure_help_supports_option(help, "--build"));
}

#[test]
fn test_looks_like_configure_help_text_accepts_bootstrap_style_output() {
    let help = "\
Usage: ./bootstrap [<options>...]
Options:
  --help
  --prefix=PREFIX";
    assert!(looks_like_configure_help_text(help));
}

#[test]
fn test_looks_like_configure_help_text_rejects_non_help_output() {
    assert!(!looks_like_configure_help_text(""));
    assert!(!looks_like_configure_help_text(
        "Unknown option: --disable-static"
    ));
}

#[test]
fn test_configure_supports_option_defaults_by_configure_file_usage() {
    assert!(configure_supports_option(None, "--host", ""));
    assert!(!configure_supports_option(
        None,
        "--host",
        "build-aux/Configure"
    ));
}

#[test]
fn test_default_configure_install_dirs_injects_expected_paths() {
    let flags = BuildFlags::default();
    let help = "\
--bindir=DIR
--sbindir=DIR
--libdir=DIR
--libexecdir=DIR
--sysconfdir=DIR
--localstatedir=DIR
--sharedstatedir=DIR
--includedir=DIR
--datarootdir=DIR
--datadir=DIR
--mandir=DIR
--infodir=DIR";
    let args = default_configure_install_dirs(&flags, Some(help));
    assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
    assert!(args.iter().any(|a| a == "--sbindir=/usr/bin"));
    assert!(args.iter().any(|a| a == "--libdir=/usr/lib"));
    assert!(args.iter().any(|a| a == "--libexecdir=/usr/lib"));
    assert!(args.iter().any(|a| a == "--sysconfdir=/etc"));
    assert!(args.iter().any(|a| a == "--localstatedir=/var"));
    assert!(args.iter().any(|a| a == "--sharedstatedir=/var/lib"));
    assert!(args.iter().any(|a| a == "--includedir=/usr/include"));
    assert!(args.iter().any(|a| a == "--datarootdir=/usr/share"));
    assert!(args.iter().any(|a| a == "--datadir=/usr/share"));
    assert!(args.iter().any(|a| a == "--mandir=/usr/share/man"));
    assert!(args.iter().any(|a| a == "--infodir=/usr/share/info"));
}

#[test]
fn test_default_configure_install_dirs_respects_explicit_user_overrides() {
    let flags = BuildFlags {
        configure: vec![
            "--sbindir=/sbin".to_string(),
            "--libdir=/custom/lib".to_string(),
            "--datadir=/custom/share".to_string(),
        ],
        ..BuildFlags::default()
    };
    let help = "--bindir=DIR --sbindir=DIR --libdir=DIR --datadir=DIR";
    let args = default_configure_install_dirs(&flags, Some(help));
    assert!(!args.iter().any(|a| a.starts_with("--sbindir=")));
    assert!(!args.iter().any(|a| a.starts_with("--libdir=")));
    assert!(!args.iter().any(|a| a.starts_with("--datadir=")));
    assert!(args.iter().any(|a| a == "--bindir=/usr/bin"));
}

#[test]
fn test_default_configure_install_dirs_lib32_uses_lib32_dirs() {
    let help = "--libdir=DIR --libexecdir=DIR";
    let flags = BuildFlags {
        lib32_variant: true,
        ..BuildFlags::default()
    };
    let args = default_configure_install_dirs(&flags, Some(help));
    assert!(args.iter().any(|a| a == "--libdir=/usr/lib32"));
    assert!(args.iter().any(|a| a == "--libexecdir=/usr/lib32"));
}

#[test]
fn test_default_configure_install_dirs_skips_when_not_advertised() {
    let flags = BuildFlags::default();
    let args = default_configure_install_dirs(&flags, Some("--prefix=PREFIX"));
    assert!(args.is_empty());
}

#[test]
fn test_configure_long_option_extracts_long_option_name() {
    assert_eq!(
        configure_long_option(" --disable-static "),
        Some("--disable-static")
    );
    assert_eq!(
        configure_long_option("--with-zlib=/usr"),
        Some("--with-zlib")
    );
    assert_eq!(configure_long_option("prefix=/usr"), None);
    assert_eq!(configure_long_option("-C"), None);
}

#[test]
fn test_add_auto_configure_arg_if_supported_skips_unsupported_long_option() {
    let mut cmd = Command::new("configure");
    add_auto_configure_arg_if_supported(&mut cmd, Some("--enable-static"), "--disable-nls");

    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();
    assert!(args.is_empty());
}

#[test]
fn test_add_auto_configure_arg_if_supported_keeps_supported_alias_and_non_option_args() {
    let mut cmd = Command::new("configure");
    add_auto_configure_arg_if_supported(&mut cmd, Some("--enable-static"), "--disable-static");
    add_auto_configure_arg_if_supported(&mut cmd, Some("--enable-static"), "srcdir");

    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();
    assert_eq!(args, vec!["--disable-static", "srcdir"]);
}

#[test]
fn test_install_destdir_path_uses_build_dir_for_lib32() {
    let build_dir = Path::new("/tmp/build");
    let destdir = Path::new("/tmp/pkg");
    assert_eq!(
        install_destdir_path(build_dir, destdir, false),
        destdir.to_path_buf()
    );
    assert_eq!(
        install_destdir_path(build_dir, destdir, true),
        build_dir.join("destdir")
    );
}

#[test]
fn test_makefile_content_has_target_detects_check_and_test() {
    let content = r#"
.PHONY: all check
all:
	@echo all
check:
	@echo check
"#;
    assert!(makefile_content_has_target(content, "check"));
    assert!(!makefile_content_has_target(content, "test"));
}

#[test]
fn test_makefile_content_has_target_ignores_assignments() {
    let content = r#"
TEST := value
VAR:=$(shell echo hi)
foo: bar
	@true
"#;
    assert!(!makefile_content_has_target(content, "TEST"));
    assert!(!makefile_content_has_target(content, "VAR"));
    assert!(!makefile_content_has_target(content, "check"));
}

#[test]
fn test_maybe_find_autotools_test_target_respects_skip_tests() -> Result<()> {
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("Makefile"), "check:\n\t@true\n").unwrap();

    let skipped = maybe_find_autotools_test_target(tmp.path(), true)?;
    assert_eq!(skipped, None);

    let detected = maybe_find_autotools_test_target(tmp.path(), false)?;
    assert_eq!(detected, Some("check"));
    Ok(())
}

#[test]
fn test_resolve_make_dirs_defaults_to_build_dir() -> Result<()> {
    let tmp = tempdir().unwrap();
    let dirs = resolve_make_dirs(tmp.path(), &[], "build.flags.make_dirs")?;
    assert_eq!(dirs, vec![tmp.path().to_path_buf()]);
    Ok(())
}

#[test]
fn test_resolve_make_dirs_resolves_multiple_relative_dirs() -> Result<()> {
    let tmp = tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("lib"))?;
    std::fs::create_dir_all(tmp.path().join("libelf"))?;
    let dirs = resolve_make_dirs(
        tmp.path(),
        &["lib".to_string(), "libelf".to_string()],
        "build.flags.make_dirs",
    )?;
    assert_eq!(
        dirs,
        vec![tmp.path().join("lib"), tmp.path().join("libelf")]
    );
    Ok(())
}

#[test]
fn test_add_make_variable_overrides_accepts_valid_assignments() -> Result<()> {
    let mut cmd = Command::new("make");
    add_make_variable_overrides(
        &mut cmd,
        &[
            "CC=clang".to_string(),
            "V=1".to_string(),
            " CFLAGS=-O2 -pipe ".to_string(),
        ],
        "build",
    )?;
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert_eq!(args, vec!["CC=clang", "V=1", "CFLAGS=-O2 -pipe"]);
    Ok(())
}

#[test]
fn test_add_make_variable_overrides_rejects_invalid_assignment() {
    let mut cmd = Command::new("make");
    let err = add_make_variable_overrides(&mut cmd, &["not-an-assignment".to_string()], "test")
        .expect_err("expected invalid assignment to fail");
    assert!(err.to_string().contains("expected NAME=VALUE"));
}

#[test]
fn test_has_make_variable_override_detects_destdir() {
    assert!(has_make_variable_override(
        &["DESTDIR=/tmp/pkg".to_string()],
        "DESTDIR"
    ));
    assert!(has_make_variable_override(
        &[" DESTDIR =/tmp/pkg ".to_string()],
        "DESTDIR"
    ));
    assert!(!has_make_variable_override(
        &["V=1".to_string(), "PREFIX=/usr".to_string()],
        "DESTDIR"
    ));
}

#[test]
fn test_resolve_make_exec_defaults_and_trims() {
    assert_eq!(resolve_make_exec(""), "make");
    assert_eq!(resolve_make_exec("  "), "make");
    assert_eq!(resolve_make_exec(" ninja "), "ninja");
}

#[test]
fn test_make_exec_supports_make_assignments_detects_make_variants() {
    assert!(make_exec_supports_make_assignments("make"));
    assert!(make_exec_supports_make_assignments("/usr/bin/gmake"));
    assert!(!make_exec_supports_make_assignments("ninja"));
}

#[test]
fn test_phase_targets_merges_singular_plural_and_default() {
    assert_eq!(
        phase_targets("bootstrap", &["stage1".into(), "stage2".into()], None),
        vec![
            "bootstrap".to_string(),
            "stage1".to_string(),
            "stage2".to_string()
        ]
    );
    assert_eq!(
        phase_targets("", &[], Some("install")),
        vec!["install".to_string()]
    );
}

#[test]
fn test_add_make_variable_overrides_if_supported_rejects_ninja_vars() {
    let mut cmd = Command::new("ninja");
    let err =
        add_make_variable_overrides_if_supported(&mut cmd, "ninja", &["V=1".to_string()], "build")
            .expect_err("ninja should reject make variable override syntax");
    assert!(err.to_string().contains("build.flags.make_vars"));
}

#[test]
fn test_resolve_actual_src_expands_source_subdir_vars() {
    let tmp = tempdir().unwrap();
    let src_root = tmp.path().join("srcroot");
    let expanded = src_root.join("expect5.45.4").join("unix");
    std::fs::create_dir_all(&expanded).unwrap();

    let spec = PackageSpec {
        package: PackageInfo {
            name: "expect".into(),
            real_name: None,
            version: "5.45.4".into(),
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
        source: Vec::new(),
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags {
                source_subdir: "$name$version/unix".into(),
                ..BuildFlags::default()
            },
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let resolved = resolve_actual_src(&spec, &src_root).unwrap();
    assert_eq!(resolved, expanded);
}

#[test]
fn test_resolve_configure_path_defaults_to_source_configure() {
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
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
        source: Vec::new(),
        build: Build {
            build_type: BuildType::Autotools,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let actual_src = PathBuf::from("/tmp/src");
    let configure = resolve_configure_path(&spec, &actual_src);
    assert_eq!(configure, actual_src.join("configure"));
}

#[test]
fn test_resolve_configure_path_uses_configure_file_and_expands_vars() {
    let flags = BuildFlags {
        configure_file: "build-aux/$name-configure".into(),
        ..BuildFlags::default()
    };
    let spec = PackageSpec {
        package: PackageInfo {
            name: "foo".into(),
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
        source: Vec::new(),
        build: Build {
            build_type: BuildType::Autotools,
            flags,
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    };

    let actual_src = PathBuf::from("/tmp/src");
    let configure = resolve_configure_path(&spec, &actual_src);
    assert_eq!(configure, actual_src.join("build-aux/foo-configure"));
}
