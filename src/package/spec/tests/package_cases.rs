use super::*;

#[test]
fn parse_package_built_against_metadata() {
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
built-against = ["icu78"]

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.package.built_against, vec!["icu78".to_string()]);
}

#[test]
fn parse_package_dependencies_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "llvm"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/llvm.tar.gz"
sha256 = "skip"
extract_dir = "llvm"

[build]
type = "custom"

[dependencies]
runtime = ["base"]
groups = ["toolchain"]

[package_dependencies.clang]
runtime = ["llvm-libs", "llvm-libgcc"]
groups = ["compiler"]

[package_dependencies.llvm-libs]
runtime = ["llvm-libgcc", "zstd"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.dependencies_for_output("llvm").runtime,
        vec!["base".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("llvm").groups,
        vec!["toolchain".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("clang").runtime,
        vec!["llvm-libs".to_string(), "llvm-libgcc".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("clang").groups,
        vec!["compiler".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("llvm-libs").runtime,
        vec!["llvm-libgcc".to_string(), "zstd".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("llvm-libs").groups,
        Vec::<String>::new()
    );
}

#[test]
fn parse_lib32_dependencies_override() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "llvm"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/llvm.tar.gz"
sha256 = "skip"
extract_dir = "llvm"

[build]
type = "custom"

[dependencies]
runtime = ["base"]
groups = ["toolchain"]

[dependencies.lib32]
build = ["gcc-multilib"]
runtime = ["lib32-zlib"]
test = ["bats"]
groups = ["lib32-toolchain"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.dependencies_for_output("llvm").runtime,
        vec!["base".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("llvm").groups,
        vec!["toolchain".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("lib32-llvm").build,
        vec!["gcc-multilib".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("lib32-llvm").runtime,
        vec!["lib32-zlib".to_string(), "llvm".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("lib32-llvm").test,
        vec!["bats".to_string()]
    );
    assert_eq!(
        spec.dependencies_for_output("lib32-llvm").groups,
        vec!["lib32-toolchain".to_string()]
    );
}

#[test]
fn parse_package_alternatives_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "llvm"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/llvm.tar.gz"
sha256 = "skip"
extract_dir = "llvm"

[build]
type = "custom"

[alternatives]
provides = ["toolchain"]
conflicts = ["gcc"]

[package_alternatives.clang]
provides = ["cc", "c++", "gcc"]
conflicts = ["clang-legacy"]

[package_alternatives.llvm]
provides = ["binutils"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.alternatives_for_output("llvm").provides,
        vec!["binutils".to_string()]
    );
    assert_eq!(
        spec.alternatives_for_output("llvm").conflicts,
        Vec::<String>::new()
    );
    assert_eq!(
        spec.alternatives_for_output("clang").provides,
        vec!["cc".to_string(), "c++".to_string(), "gcc".to_string()]
    );
    assert_eq!(
        spec.alternatives_for_output("clang").conflicts,
        vec!["clang-legacy".to_string()]
    );
    assert_eq!(
        spec.alternatives_for_output("other").provides,
        vec!["toolchain".to_string()]
    );
    assert_eq!(
        spec.alternatives_for_output("other").conflicts,
        vec!["gcc".to_string()]
    );
}

#[test]
fn parse_lib32_alternatives_override() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "llvm"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/llvm.tar.gz"
sha256 = "skip"
extract_dir = "llvm"

[build]
type = "custom"

[alternatives]
provides = ["toolchain"]
replaces = ["clang"]

[alternatives.lib32]
provides = ["lib32-toolchain"]
conflicts = ["lib32-gcc"]
replaces = ["lib32-clang"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.alternatives_for_output("llvm").replaces,
        vec!["clang".to_string()]
    );
    assert_eq!(
        spec.alternatives_for_output("lib32-llvm").provides,
        vec!["lib32-toolchain".to_string()]
    );
    assert_eq!(
        spec.alternatives_for_output("lib32-llvm").conflicts,
        vec!["lib32-gcc".to_string()]
    );
    assert_eq!(
        spec.alternatives_for_output("lib32-llvm").replaces,
        vec!["lib32-clang".to_string()]
    );
}

#[test]
fn lib32_output_does_not_fallback_to_primary_alternatives() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "llvm"
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
build_32 = true

[alternatives]
provides = ["toolchain"]
conflicts = ["gcc"]
replaces = ["clang"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    let lib32 = spec.alternatives_for_output("lib32-llvm");

    assert!(lib32.provides.is_empty());
    assert!(lib32.conflicts.is_empty());
    assert!(lib32.replaces.is_empty());
}

#[test]
fn parse_python_build_type() {
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
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(matches!(spec.build.build_type, BuildType::Python));
}

#[test]
fn parse_perl_build_type() {
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
type = "perl"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(matches!(spec.build.build_type, BuildType::Perl));
}

#[test]
fn parse_multiple_licenses() {
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
license = ["MIT", "Apache-2.0"]

[source]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.package.license,
        vec!["MIT".to_string(), "Apache-2.0".to_string()]
    );
}

#[test]
fn parse_keep_from_spec() {
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
keep = ["etc/locale.gen", "etc/resolv.conf"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.build.flags.keep,
        vec!["etc/locale.gen".to_string(), "etc/resolv.conf".to_string()]
    );
}

#[test]
fn parse_split_docs_from_spec() {
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
split_docs = true
doc_dirs = ["/opt/docs", "usr/share/devhelp"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.build.flags.split_docs);
    assert_eq!(
        spec.build.flags.doc_dirs,
        vec!["/opt/docs".to_string(), "usr/share/devhelp".to_string()]
    );
}

#[test]
fn test_package_filename() {
    let mut spec = mk_spec("foo", "1.0");
    spec.package.revision = 2;
    assert_eq!(
        spec.package_filename("x86_64"),
        "foo-1.0-2-x86_64.depot.pkg.tar.zst"
    );
}

#[test]
fn parse_packages_array() {
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

[[packages]]
name = "foo-dev"
version = "1.0"
description = "development files"
homepage = "h"
license = "MIT"

[[source]]
url = "https://example.com/foo-1.0.tar.gz"
sha256 = "skip"
extract_dir = "foo-1.0"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    let outputs = spec.outputs();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].name, "foo");
    assert_eq!(outputs[1].name, "foo-dev");
}

#[test]
fn docs_output_uses_runtime_dependency_on_parent_package() {
    let mut spec = mk_spec("foo", "1.0");
    spec.build.flags.split_docs = true;
    let docs_name = PackageSpec::docs_package_name("foo");

    let deps = spec.dependencies_for_output(&docs_name);
    assert_eq!(deps.runtime, vec!["foo".to_string()]);

    let alternatives = spec.alternatives_for_output(&docs_name);
    assert!(alternatives.provides.is_empty());
    assert!(alternatives.conflicts.is_empty());
}

#[test]
fn docs_package_for_output_derives_name_and_description() {
    let mut spec = mk_spec("foo", "1.0");
    spec.build.flags.split_docs = true;

    let docs = spec.docs_package_for_output(&spec.package);
    assert_eq!(docs.name, "foo-docs");
    assert_eq!(docs.description, "Documentation for foo");
    assert_eq!(docs.version, "1.0");
}

#[test]
fn parse_dkms_build_type_and_modules() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("zfs.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "zfs-dkms"
version = "2.4.3"
description = "d"
homepage = "h"
license = "CDDL"

[source]
url = "https://github.com/openzfs/zfs/releases/download/zfs-$version/zfs-$version.tar.gz"
sha256 = "skip"
extract_dir = "zfs-$version"

[build]
type = "dkms"

[build.flags]
dkms_name = "zfs"
dkms_version = "$version"
dkms_source_dir = "."
dkms_install_dir = "updates/depot"
dkms_make_args = ["V=1"]
dkms_pre_build = [
  "./configure --with-config=kernel --with-linux=$kernel_build_dir --with-linux-obj=$kernel_build_dir"
]

[[build.flags.dkms_modules]]
name = "zfs"
path = "module"
built_location = "module/zfs"

[[build.flags.dkms_modules]]
name = "spl"
dest_name = "spl_compat"
build_dir = "module"
built_location = "module/spl"
install_dir = "/updates/storage"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(matches!(spec.build.build_type, BuildType::Dkms));
    assert_eq!(spec.effective_dkms_name(), "zfs");
    assert_eq!(spec.effective_dkms_version(), "2.4.3");
    assert_eq!(spec.effective_dkms_install_dir(), "updates/depot");
    assert_eq!(spec.build.flags.dkms_pre_build.len(), 1);
    assert_eq!(spec.build.flags.dkms_modules.len(), 2);
    assert_eq!(spec.build.flags.dkms_modules[0].build_dir, "module");
    assert_eq!(
        spec.effective_dkms_module_install_dir(&spec.build.flags.dkms_modules[1]),
        "updates/storage"
    );
}

#[test]
fn parse_dkms_rejects_unsafe_module_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "bad-dkms"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

[source]
url = "https://example.com/bad.tar.gz"
sha256 = "skip"
extract_dir = "bad"

[build]
type = "dkms"

[[build.flags.dkms_modules]]
name = "bad"
path = "../outside"
"#,
    )
    .unwrap();

    let err = PackageSpec::from_file(&path).unwrap_err().to_string();
    assert!(err.contains("unsafe path component"), "{err}");
}
