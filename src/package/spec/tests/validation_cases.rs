use super::*;

#[test]
fn reject_unknown_nested_keys_from_spec() {
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
runtime = ["glibc"]
skip_tests = true
"#,
    )
    .unwrap();

    let err = PackageSpec::from_file(&path).expect_err("expected unknown nested key to fail");
    assert!(
        err.to_string()
            .contains("unknown key: dependencies.skip_tests")
    );
}
