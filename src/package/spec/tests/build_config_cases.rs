use super::*;

#[test]
fn parse_python_config_settings_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "python"

[build.flags]
config-setting = ["editable_mode=compat", "setup-args=--plat-name=x86_64"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.config_settings,
        vec![
            "editable_mode=compat".to_string(),
            "setup-args=--plat-name=x86_64".to_string()
        ]
    );
}

#[test]
fn test_apply_config() {
    let mut spec = mk_spec("foo", "1.0");
    let mut config = crate::config::Config::for_rootfs(Path::new("/tmp/nonexistent"));

    // Mock some overrides and appends
    config.build_overrides = toml::from_str(
        r#"
[flags]
cc = "my-cc"
cxx = "my-cxx"
ar = "my-ar"
ranlib = "my-ranlib"
strip = "my-strip"
ld = "ld.lld"
nm = "my-nm"
objcopy = "my-objcopy"
objdump = "my-objdump"
readelf = "my-readelf"
CPP = "clang-cpp"
tool_dir = "/opt/toolchain/bin"
cflags = ["-O2"]
replace_cflags = ["-O2=>-O3"]
cxxflags = ["-O2", "-pipe"]
replace_cxxflags = ["-pipe=>-fPIC"]
passthrough_env = ["RUSTFLAGS"]
env_vars = ["SETUPTOOLS_SCM_PRETEND_VERSION=$version"]
bindir = "/opt/bin"
sbindir = "/opt/sbin"
libdir = "/opt/lib64"
sysconfdir = "/opt/etc"
datarootdir = "/opt/share-root"
makeflags = "-j8"
make_vars = ["V=1"]
make_dirs = ["lib"]
make_test_dirs = ["tests"]
make_install_dirs = ["lib"]
ltoflags = ["-flto=auto"]
RUSTLTOFLAGS = ["-Clinker-plugin-lto"]
replace_ltoflags = ["auto=>thin"]
rustflags = ["-C", "debuginfo=2"]
replace_rustflags = ["debuginfo=2=>opt-level=2"]
use_lto = true
no_flags = true
no_strip = true
no_delete_static = true
no_compress_man = true
skip_tests = true
keep = ["etc/locale.gen"]
configure_file = "configure.gnu"
config-setting = ["editable_mode=compat"]
post_configure = ["echo configured"]
"#,
    )
    .unwrap();
    config.appends.insert(
        "build.flags.cflags".to_string(),
        vec![toml::Value::String("-g".to_string())],
    );
    config.appends.insert(
        "build.flags.replace_cflags".to_string(),
        vec![toml::Value::String(
            "-D_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".to_string(),
        )],
    );
    config.appends.insert(
        "build.flags.cxxflags".to_string(),
        vec![toml::Value::String("-stdlib=libc++".to_string())],
    );
    config.appends.insert(
        "build.flags.replace_cxxflags".to_string(),
        vec![toml::Value::String(
            "-stdlib=libc++=>-stdlib=libstdc++".to_string(),
        )],
    );
    config.appends.insert(
        "build.flags.rustflags".to_string(),
        vec![toml::Value::Array(vec![
            toml::Value::String("-C".to_string()),
            toml::Value::String("opt-level=3".to_string()),
        ])],
    );
    config.appends.insert(
        "build.flags.replace_rustflags".to_string(),
        vec![toml::Value::String("opt-level=3=>opt-level=z".to_string())],
    );
    config.appends.insert(
        "build.flags.keep".to_string(),
        vec![toml::Value::Array(vec![toml::Value::String(
            "etc/locale.gen".to_string(),
        )])],
    );
    config.appends.insert(
        "build.flags.ltoflags".to_string(),
        vec![toml::Value::Array(vec![toml::Value::String(
            "-fno-fat-lto-objects".to_string(),
        )])],
    );
    config.appends.insert(
        "build.flags.replace_ltoflags".to_string(),
        vec![toml::Value::String(
            "-fno-fat-lto-objects=>-flto-jobs=8".to_string(),
        )],
    );
    config.appends.insert(
        "build.flags.use_lto".to_string(),
        vec![toml::Value::Boolean(false)],
    );
    config.appends.insert(
        "build.flags.no_strip".to_string(),
        vec![toml::Value::Boolean(false)],
    );
    config.appends.insert(
        "build.flags.no_compress_man".to_string(),
        vec![toml::Value::Boolean(false)],
    );
    config.appends.insert(
        "build.flags.no_delete_static".to_string(),
        vec![toml::Value::Boolean(false)],
    );
    config.appends.insert(
        "build.flags.passthrough_env".to_string(),
        vec![toml::Value::String("CARGO_HOME".to_string())],
    );
    config.appends.insert(
        "build.flags.env_vars".to_string(),
        vec![toml::Value::String(
            "SOURCE_DATE_EPOCH=1700000000".to_string(),
        )],
    );
    config.appends.insert(
        "build.flags.make_test_vars".to_string(),
        vec![toml::Value::String("TESTS=smoke".to_string())],
    );
    config.appends.insert(
        "build.flags.makeflags".to_string(),
        vec![toml::Value::String("--output-sync=target".to_string())],
    );
    config.appends.insert(
        "build.flags.make_dirs".to_string(),
        vec![toml::Value::String("libelf".to_string())],
    );
    config.appends.insert(
        "build.flags.make_test_dirs".to_string(),
        vec![toml::Value::String("fuzz".to_string())],
    );
    config.appends.insert(
        "build.flags.make_install_dirs".to_string(),
        vec![toml::Value::String("tools".to_string())],
    );
    config.appends.insert(
        "build.flags.make_install_vars".to_string(),
        vec![toml::Value::String("DESTDIR=/tmp/pkg".to_string())],
    );
    config.appends.insert(
        "build.flags.configure_file".to_string(),
        vec![toml::Value::String("build-aux/configure".to_string())],
    );
    config.appends.insert(
        "build.flags.libexecdir".to_string(),
        vec![toml::Value::String("/opt/libexec".to_string())],
    );
    config.appends.insert(
        "build.flags.datadir".to_string(),
        vec![toml::Value::String("/opt/share-data".to_string())],
    );
    config.appends.insert(
        "build.flags.config-setting".to_string(),
        vec![toml::Value::String(
            "setup-args=--plat-name=x86_64".to_string(),
        )],
    );
    config.appends.insert(
        "build.flags.post_configure".to_string(),
        vec![toml::Value::String("touch configured.stamp".to_string())],
    );
    config.appends.insert(
        "build.flags.configure_x86_64".to_string(),
        vec![toml::Value::String("--enable-x86-tuning".to_string())],
    );

    spec.apply_config(&config);

    assert_eq!(spec.build.flags.cc, "my-cc");
    assert_eq!(spec.build.flags.cxx, "my-cxx");
    assert_eq!(spec.build.flags.ar, "my-ar");
    assert_eq!(spec.build.flags.ranlib, "my-ranlib");
    assert_eq!(spec.build.flags.strip, "my-strip");
    assert_eq!(spec.build.flags.ld, "ld.lld");
    assert_eq!(spec.build.flags.nm, "my-nm");
    assert_eq!(spec.build.flags.objcopy, "my-objcopy");
    assert_eq!(spec.build.flags.objdump, "my-objdump");
    assert_eq!(spec.build.flags.readelf, "my-readelf");
    assert_eq!(spec.build.flags.cpp, "clang-cpp");
    assert_eq!(spec.build.flags.tool_dir, "/opt/toolchain/bin");
    assert!(spec.build.flags.cflags.contains(&"-O2".to_string()));
    assert!(spec.build.flags.cflags.contains(&"-g".to_string()));
    assert!(
        spec.build
            .flags
            .replace_cflags
            .contains(&"-O2=>-O3".to_string())
    );
    assert!(
        spec.build
            .flags
            .replace_cflags
            .contains(&"-D_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".to_string())
    );
    assert!(spec.build.flags.cxxflags.contains(&"-O2".to_string()));
    assert!(spec.build.flags.cxxflags.contains(&"-pipe".to_string()));
    assert!(
        spec.build
            .flags
            .cxxflags
            .contains(&"-stdlib=libc++".to_string())
    );
    assert!(
        spec.build
            .flags
            .replace_cxxflags
            .contains(&"-pipe=>-fPIC".to_string())
    );
    assert!(
        spec.build
            .flags
            .replace_cxxflags
            .contains(&"-stdlib=libc++=>-stdlib=libstdc++".to_string())
    );
    assert!(spec.build.flags.rustflags.contains(&"-C".to_string()));
    assert!(
        spec.build
            .flags
            .rustflags
            .contains(&"debuginfo=2".to_string())
    );
    assert!(
        spec.build
            .flags
            .rustflags
            .contains(&"opt-level=3".to_string())
    );
    assert!(
        spec.build
            .flags
            .replace_rustflags
            .contains(&"debuginfo=2=>opt-level=2".to_string())
    );
    assert!(
        spec.build
            .flags
            .replace_rustflags
            .contains(&"opt-level=3=>opt-level=z".to_string())
    );
    assert!(
        spec.build
            .flags
            .ltoflags
            .contains(&"-flto=auto".to_string())
    );
    assert!(
        spec.build
            .flags
            .rustltoflags
            .contains(&"-Clinker-plugin-lto".to_string())
    );
    assert!(
        spec.build
            .flags
            .ltoflags
            .contains(&"-fno-fat-lto-objects".to_string())
    );
    assert!(
        spec.build
            .flags
            .replace_ltoflags
            .contains(&"auto=>thin".to_string())
    );
    assert!(
        spec.build
            .flags
            .replace_ltoflags
            .contains(&"-fno-fat-lto-objects=>-flto-jobs=8".to_string())
    );
    assert!(!spec.build.flags.use_lto);
    assert!(spec.build.flags.no_flags);
    assert!(!spec.build.flags.no_strip);
    assert!(!spec.build.flags.no_delete_static);
    assert!(!spec.build.flags.no_compress_man);
    assert!(
        spec.build
            .flags
            .keep
            .contains(&"etc/locale.gen".to_string())
    );
    assert!(
        spec.build
            .flags
            .passthrough_env
            .contains(&"RUSTFLAGS".to_string())
    );
    assert!(
        spec.build
            .flags
            .passthrough_env
            .contains(&"CARGO_HOME".to_string())
    );
    assert!(
        spec.build
            .flags
            .env_vars
            .contains(&"SETUPTOOLS_SCM_PRETEND_VERSION=$version".to_string())
    );
    assert!(
        spec.build
            .flags
            .env_vars
            .contains(&"SOURCE_DATE_EPOCH=1700000000".to_string())
    );
    assert_eq!(spec.build.flags.bindir, "/opt/bin");
    assert_eq!(spec.build.flags.sbindir, "/opt/sbin");
    assert_eq!(spec.build.flags.libdir, "/opt/lib64");
    assert_eq!(spec.build.flags.libexecdir, "/opt/libexec");
    assert_eq!(spec.build.flags.sysconfdir, "/opt/etc");
    assert_eq!(spec.build.flags.datarootdir, "/opt/share-root");
    assert_eq!(spec.build.flags.datadir, "/opt/share-data");
    assert_eq!(
        spec.build.flags.configure_arch.get("x86_64"),
        Some(&vec!["--enable-x86-tuning".to_string()])
    );
    assert_eq!(spec.build.flags.makeflags, "-j8 --output-sync=target");
    assert!(spec.build.flags.make_vars.contains(&"V=1".to_string()));
    assert!(spec.build.flags.make_dirs.contains(&"lib".to_string()));
    assert!(spec.build.flags.make_dirs.contains(&"libelf".to_string()));
    assert!(spec.build.flags.skip_tests);
    assert!(
        spec.build
            .flags
            .make_test_vars
            .contains(&"TESTS=smoke".to_string())
    );
    assert!(
        spec.build
            .flags
            .make_test_dirs
            .contains(&"tests".to_string())
    );
    assert!(
        spec.build
            .flags
            .make_test_dirs
            .contains(&"fuzz".to_string())
    );
    assert!(
        spec.build
            .flags
            .make_install_vars
            .contains(&"DESTDIR=/tmp/pkg".to_string())
    );
    assert!(
        spec.build
            .flags
            .make_install_dirs
            .contains(&"lib".to_string())
    );
    assert!(
        spec.build
            .flags
            .make_install_dirs
            .contains(&"tools".to_string())
    );
    assert_eq!(spec.build.flags.configure_file, "build-aux/configure");
    assert!(
        spec.build
            .flags
            .config_settings
            .contains(&"editable_mode=compat".to_string())
    );
    assert!(
        spec.build
            .flags
            .config_settings
            .contains(&"setup-args=--plat-name=x86_64".to_string())
    );
    assert!(
        spec.build
            .flags
            .post_configure
            .contains(&"echo configured".to_string())
    );
    assert!(
        spec.build
            .flags
            .post_configure
            .contains(&"touch configured.stamp".to_string())
    );
}

#[test]
fn test_apply_config_preserves_package_scalar_tool_and_layout_overrides() {
    let mut spec = mk_spec("foo", "1.0");
    spec.build.flags.ld = "ld.lld".to_string();
    spec.build.flags.libdir = "/package/lib".to_string();
    spec.build.flags.sysconfdir = "/package/etc".to_string();
    let mut config = crate::config::Config::for_rootfs(Path::new("/tmp/nonexistent"));
    config.build_overrides = toml::from_str(
        r#"
ld = "/config/bin/ld"
fuse_ld = "/config/bin/ld.lld"
ranlib = "/config/bin/ranlib"
libdir = "/config/lib"
sysconfdir = "/config/etc"
"#,
    )
    .unwrap();

    spec.apply_config(&config);

    assert_eq!(spec.build.flags.ld, "ld.lld");
    assert_eq!(spec.build.flags.libdir, "/package/lib");
    assert_eq!(spec.build.flags.sysconfdir, "/package/etc");
    assert_eq!(spec.build.flags.ranlib, "/config/bin/ranlib");
    assert_eq!(spec.build.flags.fuse_ld, "/config/bin/ld.lld");
}

#[test]
fn test_apply_config_preserves_package_lto_disable() {
    let mut spec = mk_spec("glibc", "2.43");
    spec.build.flags.use_lto = false;
    let mut config = crate::config::Config::for_rootfs(Path::new("/tmp/nonexistent"));
    config.build_overrides = toml::from_str(
        r#"
[flags]
ltoflags = ["-flto=thin"]
use_lto = true
"#,
    )
    .unwrap();

    spec.apply_config(&config);

    assert!(!spec.build.flags.use_lto);
    assert_eq!(spec.build.flags.ltoflags, vec!["-flto=thin".to_string()]);
}

#[test]
fn parse_no_flags_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
no_flags = true
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.build.flags.no_flags);
}

#[test]
fn parse_tool_commands_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
cc = "/tools/bin/cc"
cxx = "/tools/bin/c++"
ar = "/tools/bin/ar"
ranlib = "/tools/bin/ranlib"
strip = "/tools/bin/strip"
ld = "/tools/bin/ld"
fuse_ld = "/usr/bin/ld.lld"
nm = "/tools/bin/nm"
objcopy = "/tools/bin/objcopy"
objdump = "/tools/bin/objdump"
readelf = "/tools/bin/readelf"
cpp = "/tools/bin/cpp"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.build.flags.cc, "/tools/bin/cc");
    assert_eq!(spec.build.flags.cxx, "/tools/bin/c++");
    assert_eq!(spec.build.flags.ar, "/tools/bin/ar");
    assert_eq!(spec.build.flags.ranlib, "/tools/bin/ranlib");
    assert_eq!(spec.build.flags.strip, "/tools/bin/strip");
    assert_eq!(spec.build.flags.ld, "/tools/bin/ld");
    assert_eq!(spec.build.flags.fuse_ld, "/usr/bin/ld.lld");
    assert_eq!(spec.build.flags.nm, "/tools/bin/nm");
    assert_eq!(spec.build.flags.objcopy, "/tools/bin/objcopy");
    assert_eq!(spec.build.flags.objdump, "/tools/bin/objdump");
    assert_eq!(spec.build.flags.readelf, "/tools/bin/readelf");
    assert_eq!(spec.build.flags.cpp, "/tools/bin/cpp");
}

#[test]
fn parse_ltoflags_and_use_lto_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
ltoflags = ["-flto=auto", "-fuse-linker-plugin"]
use_lto = false
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.ltoflags,
        vec!["-flto=auto".to_string(), "-fuse-linker-plugin".to_string()]
    );
    assert!(!spec.build.flags.use_lto);
}

#[test]
fn parse_ltoflags_and_use_lto_aliases_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
LTOFLAGS = "-flto=auto"
"use-lto" = false
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.build.flags.ltoflags, vec!["-flto=auto".to_string()]);
    assert!(!spec.build.flags.use_lto);
}

#[test]
fn parse_no_strip_no_delete_static_and_no_compress_man_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
no_strip = true
"no-delete-static" = true
no-compress-man = true
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.build.flags.no_strip);
    assert!(spec.build.flags.no_delete_static);
    assert!(spec.build.flags.no_compress_man);
}

#[test]
fn parse_no_flags_alias_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
"no-flags" = true
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.build.flags.no_flags);
}

#[test]
fn parse_skip_tests_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
skip_tests = true
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.build.flags.skip_tests);
}

#[test]
fn parse_skip_tests_alias_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
"skip-tests" = true
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.build.flags.skip_tests);
}

#[test]
fn parse_configure_file_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
configure_file = "build-aux/configure"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.build.flags.configure_file, "build-aux/configure");
}

#[test]
fn parse_install_dirs_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "cmake"

[build.flags]
bindir = "/custom/bin"
sbindir = "/custom/sbin"
libdir = "/custom/lib64"
libexecdir = "/custom/libexec"
sysconfdir = "/custom/etc"
localstatedir = "/custom/var"
sharedstatedir = "/custom/var/lib"
includedir = "/custom/include"
datarootdir = "/custom/share-root"
datadir = "/custom/share"
mandir = "/custom/share/man"
infodir = "/custom/share/info"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.build.flags.bindir, "/custom/bin");
    assert_eq!(spec.build.flags.sbindir, "/custom/sbin");
    assert_eq!(spec.build.flags.libdir, "/custom/lib64");
    assert_eq!(spec.build.flags.libexecdir, "/custom/libexec");
    assert_eq!(spec.build.flags.sysconfdir, "/custom/etc");
    assert_eq!(spec.build.flags.localstatedir, "/custom/var");
    assert_eq!(spec.build.flags.sharedstatedir, "/custom/var/lib");
    assert_eq!(spec.build.flags.includedir, "/custom/include");
    assert_eq!(spec.build.flags.datarootdir, "/custom/share-root");
    assert_eq!(spec.build.flags.datadir, "/custom/share");
    assert_eq!(spec.build.flags.mandir, "/custom/share/man");
    assert_eq!(spec.build.flags.infodir, "/custom/share/info");
}

#[test]
fn parse_lib32_build_flags_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
"build-32" = "true"
"lib32-only" = "yes"
"CFLAGS-lib32" = ["-mstackrealign"]
"CXXFLAGS-lib32" = ["-fno-rtti"]
"configure-lib32" = ["--disable-static"]
"post_configure-lib32" = ["echo configured lib32"]
"post_compile-lib32" = ["echo compiled lib32"]
"post_install-lib32" = ["echo lib32"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.build.flags.build_32);
    assert!(spec.build.flags.lib32_only);
    assert!(spec.builds_lib32_output());
    assert!(spec.builds_only_lib32_output());
    assert_eq!(spec.build.flags.cflags_lib32, vec!["-mstackrealign"]);
    assert_eq!(spec.build.flags.cxxflags_lib32, vec!["-fno-rtti"]);
    assert_eq!(spec.build.flags.configure_lib32, vec!["--disable-static"]);
    assert_eq!(
        spec.build.flags.post_configure_lib32,
        vec!["echo configured lib32"]
    );
    assert_eq!(
        spec.build.flags.post_compile_lib32,
        vec!["echo compiled lib32"]
    );
    assert_eq!(spec.build.flags.post_install_lib32, vec!["echo lib32"]);
}

#[test]
fn multilib_builds_skip_automatic_tests() {
    let mut spec = mk_spec("foo", "1.0");
    assert!(!spec.should_skip_automatic_tests());

    spec.build.flags.build_32 = true;
    assert!(spec.should_skip_automatic_tests());

    spec.build.flags.build_32 = false;
    spec.build.flags.skip_tests = true;
    assert!(spec.should_skip_automatic_tests());
}

#[test]
fn parse_post_configure_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "cmake"

[build.flags]
post_configure = ["cmake -L . > cmake-options.txt"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.post_configure,
        vec!["cmake -L . > cmake-options.txt".to_string()]
    );
}

#[test]
fn parse_build_flags_appends_from_spec_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
replace_cflags = ["-O2=>-O3"]
replace_rustflags = ["debuginfo=2=>opt-level=2"]
cxxflags = ["-O2"]
cxxflags += [ "-Wno-gnu-statement-expression-from-macro-expansion" ]
ldflags += "-Wl,--as-needed"
replace_cflags += [ "_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2" ]
replace_rustflags += "opt-level=3=>opt-level=z"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.cxxflags,
        vec![
            "-O2".to_string(),
            "-Wno-gnu-statement-expression-from-macro-expansion".to_string()
        ]
    );
    assert_eq!(
        spec.build.flags.ldflags,
        vec!["-Wl,--as-needed".to_string()]
    );
    assert_eq!(
        spec.build.flags.replace_cflags,
        vec![
            "-O2=>-O3".to_string(),
            "_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".to_string()
        ]
    );
    assert_eq!(
        spec.build.flags.replace_rustflags,
        vec![
            "debuginfo=2=>opt-level=2".to_string(),
            "opt-level=3=>opt-level=z".to_string()
        ]
    );
}

#[test]
fn parse_configure_arch_appends_from_spec_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
configure = ["--base"]
configure_x86_64 += ["--enable-sse2"]
configure_aarch64 += "--enable-neon"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.build.flags.configure, vec!["--base".to_string()]);
    assert_eq!(
        spec.build.flags.configure_arch.get("x86_64"),
        Some(&vec!["--enable-sse2".to_string()])
    );
    assert_eq!(
        spec.build.flags.configure_arch.get("aarch64"),
        Some(&vec!["--enable-neon".to_string()])
    );
}

#[test]
fn parse_build_flags_appends_accepts_quoted_and_uppercase_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
"cflags" += ["-fPIC"]
CXXFLAGS += ["-stdlib=libc++"]
"LDFLAGS" += "-Wl,--as-needed"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.build.flags.cflags, vec!["-fPIC".to_string()]);
    assert_eq!(
        spec.build.flags.cxxflags,
        vec!["-stdlib=libc++".to_string()]
    );
    assert_eq!(
        spec.build.flags.ldflags,
        vec!["-Wl,--as-needed".to_string()]
    );
}

#[test]
fn apply_config_reads_build_flag_appends_from_rootfs_build_toml() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("etc/depot.d/build.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        r#"
[flags]
cflags += ["-g"]
CXXFLAGS += ["-stdlib=libc++"]
LDFLAGS += "-Wl,--as-needed"
"#,
    )
    .unwrap();

    let config = crate::config::Config::for_rootfs(tmp.path());
    assert_eq!(
        config.appends.get("build.flags.cflags").unwrap()[0]
            .as_array()
            .unwrap()[0]
            .as_str(),
        Some("-g")
    );
    assert_eq!(
        config.appends.get("build.flags.cxxflags").unwrap()[0]
            .as_array()
            .unwrap()[0]
            .as_str(),
        Some("-stdlib=libc++")
    );
    assert_eq!(
        config.appends.get("build.flags.ldflags").unwrap()[0].as_str(),
        Some("-Wl,--as-needed")
    );
    let mut spec = mk_spec("foo", "1.0");
    spec.apply_config(&config);

    assert!(spec.build.flags.cflags.contains(&"-g".to_string()));
    assert!(
        spec.build
            .flags
            .cxxflags
            .contains(&"-stdlib=libc++".to_string())
    );
    assert!(
        spec.build
            .flags
            .ldflags
            .contains(&"-Wl,--as-needed".to_string())
    );
}

#[test]
fn parse_passthrough_env_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"

[build.flags]
passthrough_env = ["RUSTFLAGS", "CARGO_HOME"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.passthrough_env,
        vec!["RUSTFLAGS".to_string(), "CARGO_HOME".to_string()]
    );
}

#[test]
fn parse_env_vars_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "python"

[build.flags]
env_vars = ["SETUPTOOLS_SCM_PRETEND_VERSION=$version", "PYO3_CONFIG_FILE=$specdir/pyo3.toml"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.env_vars,
        vec![
            "SETUPTOOLS_SCM_PRETEND_VERSION=$version".to_string(),
            "PYO3_CONFIG_FILE=$specdir/pyo3.toml".to_string()
        ]
    );
}

#[test]
fn parse_test_dependencies_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

	[dependencies]
	build = ["make"]
	test = ["python", "bats"]
	optional = ["gtk-doc"]
	"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.dependencies.test,
        vec!["python".to_string(), "bats".to_string()]
    );
    assert_eq!(spec.dependencies.optional, vec!["gtk-doc".to_string()]);
}

#[test]
fn parse_make_var_overrides_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
make_vars = ["V=1", "CC=clang"]
make_exec = "ninja"
make_target = "bootstrap"
make_targets = ["stage1", "stage2"]
make_dirs = ["lib", "libelf"]
make_test_vars = ["TESTS=unit"]
make_test_target = "test"
make_test_targets = ["test-unit", "test-integration"]
make_test_dirs = ["tests"]
make_install_vars = ["STRIPPROG=true"]
make_install_target = "install/strip"
make_install_targets = ["install-runtime", "install-devel"]
make_install_dirs = ["lib", "apps"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.make_vars,
        vec!["V=1".to_string(), "CC=clang".to_string()]
    );
    assert_eq!(spec.build.flags.make_exec, "ninja");
    assert_eq!(spec.build.flags.make_target, "bootstrap");
    assert_eq!(
        spec.build.flags.make_targets,
        vec!["stage1".to_string(), "stage2".to_string()]
    );
    assert_eq!(
        spec.build.flags.make_dirs,
        vec!["lib".to_string(), "libelf".to_string()]
    );
    assert_eq!(
        spec.build.flags.make_test_vars,
        vec!["TESTS=unit".to_string()]
    );
    assert_eq!(spec.build.flags.make_test_target, "test".to_string());
    assert_eq!(
        spec.build.flags.make_test_targets,
        vec!["test-unit".to_string(), "test-integration".to_string()]
    );
    assert_eq!(spec.build.flags.make_test_dirs, vec!["tests".to_string()]);
    assert_eq!(
        spec.build.flags.make_install_vars,
        vec!["STRIPPROG=true".to_string()]
    );
    assert_eq!(
        spec.build.flags.make_install_target,
        "install/strip".to_string()
    );
    assert_eq!(
        spec.build.flags.make_install_targets,
        vec!["install-runtime".to_string(), "install-devel".to_string()]
    );
    assert_eq!(
        spec.build.flags.make_install_dirs,
        vec!["lib".to_string(), "apps".to_string()]
    );
}

#[test]
fn parse_makeflags_from_spec() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "autotools"

[build.flags]
MAKEFLAGS = ["-j12", "--output-sync=target"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.build.flags.makeflags, "-j12 --output-sync=target");
}

#[test]
fn test_chost_cbuild_overrides() {
    let mut spec = mk_spec("foo", "1.0");
    let config = crate::config::Config {
        cache_dir: "/tmp".into(),
        build_dir: "/tmp".into(),
        db_dir: "/tmp".into(),
        build_overrides: toml::from_str(
            r#"
chost = "x86_64-sfg-linux-gnu"
cbuild = "x86_64-pc-linux-gnu"
"#,
        )
        .unwrap(),
        package_overrides: toml::Value::Table(toml::map::Map::new()),
        appends: std::collections::HashMap::new(),
        repo_settings: crate::config::RepoSettings::default(),
        source_repos: std::collections::BTreeMap::new(),
        binary_repos: std::collections::BTreeMap::new(),
        mirrors: std::collections::HashMap::new(),
        repo_clone_dir: PathBuf::from("/tmp"),
        package_cache_dir: PathBuf::from("/tmp"),
        install_test_deps: false,
    };

    spec.apply_config(&config);
    assert_eq!(spec.build.flags.chost, "x86_64-sfg-linux-gnu");
    assert_eq!(spec.build.flags.cbuild, "x86_64-pc-linux-gnu");
}

#[test]
fn test_default_and_override_carch() {
    let mut spec = mk_spec("foo", "1.0");
    // Default should be host arch
    assert_eq!(spec.build.flags.carch, std::env::consts::ARCH.to_string());

    // Override via config
    let mut config = crate::config::Config::for_rootfs(Path::new("/tmp/nonexistent"));
    config.build_overrides = toml::from_str(
        r#"[flags]
carch = "armv7"
"#,
    )
    .unwrap();
    spec.apply_config(&config);
    assert_eq!(spec.build.flags.carch, "armv7");
}
