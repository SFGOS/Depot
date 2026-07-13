use super::*;

#[test]
fn parse_single_source_table() {
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
url = "https://example.com/foo-$version.tar.gz"
sha256 = "skip"
extract_dir = "foo-$version"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.package.name, "foo");
    assert_eq!(spec.sources().len(), 1);
    assert_eq!(
        spec.expand_vars(&spec.sources()[0].url),
        "https://example.com/foo-1.0.tar.gz"
    );
    assert!(spec.sources()[0].patches.is_empty());
    assert!(spec.sources()[0].post_extract.is_empty());
    assert!(spec.sources()[0].cherry_pick.is_empty());
    assert_eq!(spec.spec_dir, tmp.path());
}

#[test]
fn parse_source_array() {
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

[[source]]
url = "https://example.com/foo.tar.gz"
sha256 = "skip"
extract_dir = "foo"

[[source]]
url = "https://example.com/bar.tar.gz"
sha256 = "skip"
extract_dir = "bar"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.sources().len(), 2);
    assert_eq!(spec.sources()[0].extract_dir, "foo");
    assert_eq!(spec.sources()[1].extract_dir, "bar");
}

#[test]
fn parse_source_without_sha256_defaults_to_skip() {
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
extract_dir = "foo"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.sources()[0].sha256, "skip");
}

#[test]
fn parse_git_source_with_cherry_pick() {
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
url = "https://example.com/foo.git#main"
sha256 = "skip"
extract_dir = "foo"
cherry_pick = ["deadbeef", "cafebabe"]

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(
        spec.sources()[0].cherry_pick,
        vec!["deadbeef".to_string(), "cafebabe".to_string()]
    );
}

#[test]
fn parse_rejects_empty_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    // `source = []` is not accepted (must have at least one entry)
    std::fs::write(
        &path,
        r#"
[package]
name = "foo"
version = "1.0"
description = "d"
homepage = "h"
license = "MIT"

source = []

[build]
type = "custom"
"#,
    )
    .unwrap();

    assert!(PackageSpec::from_file(&path).is_err());
}

#[test]
fn parse_allows_metapackage_without_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pkg.toml");

    std::fs::write(
        &path,
        r#"
[package]
name = "foo-meta"
version = "1.0"
description = "metapackage"
homepage = "https://example.com"
license = "MIT"

[build]
type = "meta"

[dependencies]
runtime = ["foo", "bar"]
groups = ["base"]
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert!(spec.source.is_empty());
    assert!(spec.manual_sources.is_empty());
    assert!(spec.is_metapackage());
    assert_eq!(spec.dependencies.runtime, vec!["foo", "bar"]);
    assert_eq!(spec.dependencies.groups, vec!["base"]);
}

#[test]
fn parse_manual_source_with_url() {
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

[[manual_sources]]
url = "https://example.com/manual.patch"
sha256 = "skip"
dest = "patches/manual.patch"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.manual_sources.len(), 1);
    assert_eq!(
        spec.manual_sources[0].url.as_deref(),
        Some("https://example.com/manual.patch")
    );
    assert_eq!(spec.manual_sources[0].file, None);
}

#[test]
fn parse_manual_source_rejects_missing_file_and_url() {
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

[[manual_sources]]
sha256 = "skip"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let err = PackageSpec::from_file(&path).expect_err("spec should be rejected");
    assert!(
        err.to_string()
            .contains("must specify one of 'file', 'files', 'url', or 'urls'")
    );
}

#[test]
fn parse_manual_source_rejects_file_and_url_together() {
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

[[manual_sources]]
file = "manual.patch"
url = "https://example.com/manual.patch"

[build]
type = "custom"
"#,
    )
    .unwrap();

    let err = PackageSpec::from_file(&path).expect_err("spec should be rejected");
    assert!(
        err.to_string()
            .contains("cannot mix local ('file'/'files') and remote ('url'/'urls') entries")
    );
}

#[test]
fn parse_manual_source_with_files_array() {
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

[[manual_sources]]
files = ["other", "system-auth"]

[build]
type = "custom"
"#,
    )
    .unwrap();

    let spec = PackageSpec::from_file(&path).unwrap();
    assert_eq!(spec.manual_sources.len(), 1);
    assert_eq!(spec.manual_sources[0].files, vec!["other", "system-auth"]);
    assert!(spec.manual_sources[0].urls.is_empty());
}
