//! Binary package "build" system — used when package supplies a prebuilt binary installer

use crate::cross::CrossConfig;
use crate::package::PackageSpec;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// For binary packages we simply copy the extracted files into DESTDIR (preserving
/// directory structure). This is useful for .deb packages where extract step
/// already unpacked the data payload into the source directory.
pub fn build(
    _spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    _cross: Option<&CrossConfig>,
    _export_compiler_flags: bool,
    _host_build_dir: Option<&Path>,
) -> Result<()> {
    crate::log_info!(
        "Binary install: copying files from {} to {} (pkg type={})",
        src_dir.display(),
        destdir.display(),
        _spec.build.flags.binary_type
    );
    fs::create_dir_all(destdir)
        .with_context(|| format!("Failed to create destdir: {}", destdir.display()))?;

    crate::fs_copy::copy_tree_preserving_links(src_dir, destdir)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo};
    use std::fs;
    use std::os::unix::fs as unix_fs;
    use tempfile::tempdir;

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
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "h".into(),
                sha256: "s".into(),
                extract_dir: "e".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Bin,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: std::path::PathBuf::from("."),
        }
    }

    #[test]
    fn test_bin_build_copies_files_and_symlinks() -> Result<()> {
        let tmp_src = tempdir()?;
        let tmp_dest = tempdir()?;
        let src = tmp_src.path();
        let dest = tmp_dest.path();

        // Create a directory and files
        fs::create_dir_all(src.join("usr/bin"))?;
        fs::write(src.join("usr/bin/hello"), b"hi")?;

        // Create a symlink
        let target = src.join("usr/lib/libdummy.so");
        fs::create_dir_all(target.parent().unwrap())?;
        fs::write(&target, b"lib")?;
        unix_fs::symlink(&target, src.join("usr/lib/libdummy.so.link"))?;

        let spec = mk_spec("bin-test", "1.0");
        build(&spec, src, dest, None, true, None)?;

        // Check copied file
        assert!(dest.join("usr/bin/hello").exists());

        // Check symlink target exists at dest
        let link_path = dest.join("usr/lib/libdummy.so.link");
        assert!(link_path.exists());

        Ok(())
    }
}
