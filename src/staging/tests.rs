use super::*;
use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};
use std::io::Read;

fn mk_spec_for_stage_processing() -> PackageSpec {
    let flags = BuildFlags {
        no_strip: true,
        no_compress_man: true,
        ..BuildFlags::default()
    };
    PackageSpec {
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
            build_type: BuildType::Custom,
            flags,
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    }
}

#[test]
fn process_removes_static_archives_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(destdir.join("usr/lib")).unwrap();
    std::fs::write(destdir.join("usr/lib/libfoo.a"), "static").unwrap();
    std::fs::write(destdir.join("usr/lib/libfoo.la"), "libtool").unwrap();
    std::fs::write(destdir.join("usr/lib/libfoo.so"), "shared").unwrap();

    let spec = mk_spec_for_stage_processing();
    process(&destdir, &spec).unwrap();

    assert!(!destdir.join("usr/lib/libfoo.a").exists());
    assert!(!destdir.join("usr/lib/libfoo.la").exists());
    assert!(destdir.join("usr/lib/libfoo.so").exists());
}

#[test]
fn process_preserves_static_archives_when_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(destdir.join("usr/lib")).unwrap();
    std::fs::write(destdir.join("usr/lib/libfoo.a"), "static").unwrap();
    std::fs::write(destdir.join("usr/lib/libfoo.la"), "libtool").unwrap();

    let mut spec = mk_spec_for_stage_processing();
    spec.build.flags.no_delete_static = true;
    process(&destdir, &spec).unwrap();

    assert!(destdir.join("usr/lib/libfoo.a").exists());
    assert!(!destdir.join("usr/lib/libfoo.la").exists());
}

#[test]
fn process_splits_docs_into_docs_output() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(destdir.join("usr/share/doc/foo")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/share/gtk-doc/html/foo")).unwrap();
    std::fs::create_dir_all(destdir.join("opt/foo-docs")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join("usr/share/doc/foo/README"), "doc").unwrap();
    std::fs::write(destdir.join("usr/share/gtk-doc/html/foo/index.html"), "gtk").unwrap();
    std::fs::write(destdir.join("opt/foo-docs/guide.txt"), "guide").unwrap();
    std::fs::write(destdir.join("usr/bin/foo"), "bin").unwrap();

    let mut spec = mk_spec_for_stage_processing();
    spec.build.flags.split_docs = true;
    spec.build.flags.doc_dirs = vec!["/opt/foo-docs".to_string()];

    process(&destdir, &spec).unwrap();

    let docs_destdir = output_staging_dir(&destdir, "foo-docs");
    assert!(docs_destdir.join("usr/share/doc/foo/README").exists());
    assert!(
        docs_destdir
            .join("usr/share/gtk-doc/html/foo/index.html")
            .exists()
    );
    assert!(docs_destdir.join("opt/foo-docs/guide.txt").exists());
    assert!(destdir.join("usr/bin/foo").exists());
    assert!(!destdir.join("usr/share/doc/foo/README").exists());
    assert!(
        !destdir
            .join("usr/share/gtk-doc/html/foo/index.html")
            .exists()
    );
    assert!(!destdir.join("opt/foo-docs/guide.txt").exists());
}

#[test]
fn process_splits_docs_for_additional_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    let dev_destdir = output_staging_dir(&destdir, "foo-dev");
    std::fs::create_dir_all(dev_destdir.join("usr/share/doc/foo-dev")).unwrap();
    std::fs::create_dir_all(dev_destdir.join("usr/include")).unwrap();
    std::fs::write(dev_destdir.join("usr/share/doc/foo-dev/README"), "doc").unwrap();
    std::fs::write(dev_destdir.join("usr/include/foo.h"), "header").unwrap();

    let mut spec = mk_spec_for_stage_processing();
    spec.packages.push(PackageInfo {
        name: "foo-dev".into(),
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        description: "dev".into(),
        homepage: "h".into(),
        abi_breaking: false,
        built_against: Vec::new(),
        license: vec!["MIT".into()],
    });
    spec.build.flags.split_docs = true;

    process(&destdir, &spec).unwrap();

    let docs_destdir = output_staging_dir(&destdir, "foo-dev-docs");
    assert!(docs_destdir.join("usr/share/doc/foo-dev/README").exists());
    assert!(dev_destdir.join("usr/include/foo.h").exists());
    assert!(!dev_destdir.join("usr/share/doc/foo-dev/README").exists());
}

#[test]
fn add_licenses_copies_common_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("src");
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();

    std::fs::write(src_dir.join("LICENSE"), "license text").unwrap();
    std::fs::write(src_dir.join("COPYING.md"), "copying text").unwrap();
    std::fs::write(src_dir.join("README"), "not a license").unwrap();

    let copied = add_licenses(&src_dir, &destdir, "foo").unwrap();
    assert_eq!(copied, 2);

    let lic_dir = destdir.join("usr/share/licenses/foo");
    assert!(lic_dir.join("LICENSE").exists());
    assert!(lic_dir.join("COPYING.md").exists());
    assert!(!lic_dir.join("README").exists());
}

#[test]
fn compress_manpages_zstd_detects_split_output_payload_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    let page = output_staging_dir(&dest, "clang").join("usr/share/man/man1/clang.1");
    std::fs::create_dir_all(page.parent().unwrap()).unwrap();
    std::fs::write(&page, b"clang manpage\n").unwrap();

    let count = compress_manpages_zstd(&dest).unwrap();
    assert_eq!(count, 1);
    assert!(!page.exists());

    let compressed = page.with_extension("1.zst");
    assert!(compressed.exists());
    let encoded = std::fs::read(&compressed).unwrap();
    let decoded = zstd::stream::decode_all(std::io::Cursor::new(encoded)).unwrap();
    assert_eq!(String::from_utf8(decoded).unwrap(), "clang manpage\n");
}

#[test]
fn stage_split_package_licenses_symlinks_matching_outputs_and_copies_distinct_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("src");
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();
    std::fs::write(src_dir.join("LICENSE"), "license text").unwrap();
    add_licenses(&src_dir, &destdir, "foo").unwrap();

    let mut spec = mk_spec_for_stage_processing();
    spec.packages.push(PackageInfo {
        name: "foo-dev".into(),
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        description: "dev".into(),
        homepage: "h".into(),
        abi_breaking: false,
        built_against: Vec::new(),
        license: vec!["MIT".into()],
    });
    spec.packages.push(PackageInfo {
        name: "foo-extras".into(),
        real_name: None,
        version: "1.0".into(),
        revision: 1,
        description: "extras".into(),
        homepage: "h".into(),
        abi_breaking: false,
        built_against: Vec::new(),
        license: vec!["Apache-2.0".into()],
    });

    let dev_dest = output_staging_dir(&destdir, "foo-dev").join("usr/bin");
    let extras_dest = output_staging_dir(&destdir, "foo-extras").join("usr/bin");
    std::fs::create_dir_all(&dev_dest).unwrap();
    std::fs::create_dir_all(&extras_dest).unwrap();
    std::fs::write(dev_dest.join("foo-dev"), "bin").unwrap();
    std::fs::write(extras_dest.join("foo-extras"), "bin").unwrap();

    stage_split_package_licenses(&src_dir, &destdir, &spec).unwrap();

    let dev_license = output_staging_dir(&destdir, "foo-dev").join("usr/share/licenses/foo-dev");
    let dev_meta = std::fs::symlink_metadata(&dev_license).unwrap();
    assert!(dev_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(&dev_license).unwrap(),
        PathBuf::from("foo")
    );

    let extras_license =
        output_staging_dir(&destdir, "foo-extras").join("usr/share/licenses/foo-extras");
    let extras_meta = std::fs::symlink_metadata(&extras_license).unwrap();
    assert!(extras_meta.is_dir());
    let mut text = String::new();
    std::fs::File::open(extras_license.join("LICENSE"))
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(text, "license text");
}

#[test]
fn install_atomic_update_and_rollback_restores_state() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();

    // Existing installed files
    std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    std::fs::write(rootfs.join("usr/bin/foo"), "old").unwrap();
    std::fs::write(rootfs.join("usr/bin/old_only"), "to_remove").unwrap();

    // New staged files
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join("usr/bin/foo"), "new").unwrap();
    std::fs::write(destdir.join("usr/bin/new_only"), "added").unwrap();

    let remove_paths = vec!["usr/bin/old_only".to_string()];
    let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

    // After install: updated + new present, obsolete removed
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/foo")).unwrap(),
        "new"
    );
    assert!(rootfs.join("usr/bin/new_only").exists());
    assert!(!rootfs.join("usr/bin/old_only").exists());

    // Roll back should restore old state
    tx.rollback().unwrap();
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/foo")).unwrap(),
        "old"
    );
    assert!(!rootfs.join("usr/bin/new_only").exists());
    assert!(rootfs.join("usr/bin/old_only").exists());
}

#[test]
fn install_atomic_keep_existing_installs_depotnew() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("etc")).unwrap();
    std::fs::create_dir_all(destdir.join("etc")).unwrap();

    std::fs::write(rootfs.join("etc/locale.gen"), "existing").unwrap();
    std::fs::write(destdir.join("etc/locale.gen"), "from-package").unwrap();

    let keep = vec!["etc/locale.gen".to_string()];
    let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &keep).unwrap();

    assert_eq!(
        std::fs::read_to_string(rootfs.join("etc/locale.gen")).unwrap(),
        "existing"
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("etc/locale.gen.depotnew")).unwrap(),
        "from-package"
    );

    tx.rollback().unwrap();
    assert_eq!(
        std::fs::read_to_string(rootfs.join("etc/locale.gen")).unwrap(),
        "existing"
    );
    assert!(!rootfs.join("etc/locale.gen.depotnew").exists());
}

#[test]
fn install_atomic_preserves_staged_hardlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();

    let coreutils = destdir.join("usr/bin/coreutils");
    let ls = destdir.join("usr/bin/ls");
    std::fs::write(&coreutils, "multicall").unwrap();
    std::fs::hard_link(&coreutils, &ls).unwrap();

    let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    let coreutils_meta = rootfs.join("usr/bin/coreutils").metadata().unwrap();
    let ls_meta = rootfs.join("usr/bin/ls").metadata().unwrap();
    assert_eq!(coreutils_meta.ino(), ls_meta.ino());
    assert_eq!(coreutils_meta.nlink(), 2);
    assert_eq!(ls_meta.nlink(), 2);
}

#[test]
fn install_atomic_keep_wildcard_matches_directory_children() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("etc/pam.d")).unwrap();
    std::fs::create_dir_all(destdir.join("etc/pam.d")).unwrap();
    std::fs::create_dir_all(destdir.join("etc/pam.d/subdir")).unwrap();

    std::fs::write(rootfs.join("etc/pam.d/system-auth"), "existing-auth").unwrap();
    std::fs::write(destdir.join("etc/pam.d/system-auth"), "pkg-auth").unwrap();
    std::fs::write(destdir.join("etc/pam.d/other"), "pkg-other").unwrap();
    std::fs::write(destdir.join("etc/pam.d/subdir/nested"), "pkg-nested").unwrap();

    let keep = vec!["etc/pam.d/*".to_string()];
    let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &keep).unwrap();

    // Existing matched file is preserved and package version becomes .depotnew
    assert_eq!(
        std::fs::read_to_string(rootfs.join("etc/pam.d/system-auth")).unwrap(),
        "existing-auth"
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("etc/pam.d/system-auth.depotnew")).unwrap(),
        "pkg-auth"
    );

    // New matched file installs normally because no existing file is present
    assert_eq!(
        std::fs::read_to_string(rootfs.join("etc/pam.d/other")).unwrap(),
        "pkg-other"
    );

    // Single-segment * does not cross '/'
    assert_eq!(
        std::fs::read_to_string(rootfs.join("etc/pam.d/subdir/nested")).unwrap(),
        "pkg-nested"
    );
    assert!(!rootfs.join("etc/pam.d/subdir/nested.depotnew").exists());

    tx.rollback().unwrap();
}

#[test]
#[cfg(unix)]
fn install_atomic_replaces_existing_symlink_without_touching_target_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();
    std::fs::write(rootfs.join("usr/bin/existing"), "keep-me").unwrap();
    std::os::unix::fs::symlink("usr/bin", rootfs.join("bin")).unwrap();
    std::os::unix::fs::symlink("usr/bin", destdir.join("bin")).unwrap();

    let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    assert_eq!(
        std::fs::read_link(rootfs.join("bin")).unwrap(),
        PathBuf::from("usr/bin")
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/existing")).unwrap(),
        "keep-me"
    );

    tx.rollback().unwrap();
}

#[test]
#[cfg(unix)]
fn install_atomic_rejects_replacing_directory_with_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
    std::fs::create_dir_all(destdir.join("usr")).unwrap();
    std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

    let err = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap_err();
    assert!(
        err.to_string()
            .contains("Refusing to replace existing directory with packaged file/symlink")
    );
}

#[test]
#[cfg(unix)]
fn install_atomic_relocates_existing_directory_into_packaged_symlink_target() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = rootfs.join("var/cache/depot/build/tx");
    std::fs::create_dir_all(rootfs.join("lib/depot")).unwrap();
    std::fs::write(rootfs.join("lib/depot/lock"), "state").unwrap();
    std::fs::create_dir_all(rootfs.join("usr/lib/depot")).unwrap();
    std::fs::write(rootfs.join("usr/lib/depot/lock"), "state").unwrap();
    std::fs::create_dir_all(destdir.join("usr/lib/misc")).unwrap();
    std::os::unix::fs::symlink("usr/lib", destdir.join("lib")).unwrap();

    let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    let lib_meta = rootfs.join("lib").symlink_metadata().unwrap();
    assert!(lib_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(rootfs.join("lib")).unwrap(),
        PathBuf::from("usr/lib")
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/lib/depot/lock")).unwrap(),
        "state"
    );
    assert!(rootfs.join("usr/lib/misc").is_dir());

    tx.rollback().unwrap();
    let restored = rootfs.join("lib").symlink_metadata().unwrap();
    assert!(restored.file_type().is_dir());
    assert_eq!(
        std::fs::read_to_string(rootfs.join("lib/depot/lock")).unwrap(),
        "state"
    );
}

#[test]
#[cfg(unix)]
fn install_atomic_replaces_obsolete_directory_with_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::create_dir_all(destdir.join("usr")).unwrap();
    std::fs::write(rootfs.join("usr/sbin/legacy"), "old").unwrap();
    std::fs::write(destdir.join("usr/bin/legacy"), "new").unwrap();
    std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

    let remove_paths = vec!["usr/sbin/legacy".to_string(), "usr/sbin".to_string()];
    let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

    let sbin_meta = rootfs.join("usr/sbin").symlink_metadata().unwrap();
    assert!(sbin_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(rootfs.join("usr/sbin")).unwrap(),
        PathBuf::from("bin")
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/legacy")).unwrap(),
        "new"
    );

    tx.rollback().unwrap();
    let restored = rootfs.join("usr/sbin").symlink_metadata().unwrap();
    assert!(restored.file_type().is_dir());
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/sbin/legacy")).unwrap(),
        "old"
    );
}

#[test]
#[cfg(unix)]
fn install_atomic_preserves_non_obsolete_directory_contents_when_replacing_with_symlink() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::create_dir_all(destdir.join("usr")).unwrap();
    std::fs::write(rootfs.join("usr/sbin/keep"), "keep-me").unwrap();
    std::fs::write(rootfs.join("usr/sbin/legacy"), "old").unwrap();
    std::fs::write(destdir.join("usr/bin/legacy"), "new").unwrap();
    std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

    let remove_paths = vec!["usr/sbin/legacy".to_string(), "usr/sbin".to_string()];
    let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

    let sbin_meta = rootfs.join("usr/sbin").symlink_metadata().unwrap();
    assert!(sbin_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/keep")).unwrap(),
        "keep-me"
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/sbin/keep")).unwrap(),
        "keep-me"
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/legacy")).unwrap(),
        "new"
    );

    tx.rollback().unwrap();
    let restored = rootfs.join("usr/sbin").symlink_metadata().unwrap();
    assert!(restored.file_type().is_dir());
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/sbin/keep")).unwrap(),
        "keep-me"
    );
    assert!(!rootfs.join("usr/bin/keep").exists());
}

#[test]
#[cfg(unix)]
fn install_atomic_rejects_symlink_swap_when_relocated_contents_conflict_with_target() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("usr/sbin")).unwrap();
    std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
    std::fs::create_dir_all(destdir.join("usr")).unwrap();
    std::fs::write(rootfs.join("usr/sbin/keep"), "keep-me").unwrap();
    std::fs::write(rootfs.join("usr/bin/keep"), "target-conflict").unwrap();
    std::os::unix::fs::symlink("bin", destdir.join("usr/sbin")).unwrap();

    let remove_paths = vec!["usr/sbin".to_string()];
    let err = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap_err();
    assert!(
        err.to_string()
            .contains("Failed to replay relocated path into")
    );

    let restored = rootfs.join("usr/sbin").symlink_metadata().unwrap();
    assert!(restored.file_type().is_dir());
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/sbin/keep")).unwrap(),
        "keep-me"
    );
    assert_eq!(
        std::fs::read_to_string(rootfs.join("usr/bin/keep")).unwrap(),
        "target-conflict"
    );
}

#[test]
fn keep_glob_matches_question_mark_and_not_path_separator() {
    assert!(glob_match_path(
        "etc/pam.d/system-????",
        "etc/pam.d/system-auth"
    ));
    assert!(!glob_match_path("etc/pam.d/*", "etc/pam.d/subdir/file"));
    assert!(glob_match_path("etc/pam.d/*", "etc/pam.d/file"));
    assert!(glob_match_path("etc/pam.d/**", "etc/pam.d/subdir/file"));
    assert!(glob_match_path("etc/**/file", "etc/pam.d/subdir/file"));
    assert!(glob_match_path("etc/pam.d/**", "etc/pam.d"));
}

#[test]
fn is_manpage_rel_path_detects_uncompressed_manpages() {
    assert!(is_manpage_rel_path("usr/share/man/man1/ls.1"));
    assert!(is_manpage_rel_path("/usr/share/man/man5/pam.d.5"));
    assert!(!is_manpage_rel_path("usr/share/man/man1/ls.1.zst"));
    assert!(!is_manpage_rel_path("usr/share/doc/readme"));
}

#[test]
fn is_elf_file_detects_magic_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = tmp.path().join("elf.bin");
    let text = tmp.path().join("text.txt");
    std::fs::write(&elf, [0x7F, b'E', b'L', b'F', 0x02, 0x01]).unwrap();
    std::fs::write(&text, b"#!/bin/sh\n").unwrap();

    assert!(is_elf_file(&elf).unwrap());
    assert!(!is_elf_file(&text).unwrap());
}

#[test]
fn auto_strip_elf_files_restores_hardlinks_when_strip_replaces_file() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    let bin = dest.join("usr/bin");
    std::fs::create_dir_all(&bin).unwrap();

    let fake_strip = tmp.path().join("fake-strip");
    std::fs::write(
            &fake_strip,
            "#!/bin/sh\nfor arg do path=$arg; done\ntmp=\"$path.tmp\"\ncp \"$path\" \"$tmp\"\nprintf stripped >> \"$tmp\"\nmv \"$tmp\" \"$path\"\n",
        )
        .unwrap();
    let mut perms = std::fs::metadata(&fake_strip).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_strip, perms).unwrap();

    let coreutils = bin.join("coreutils");
    let ls = bin.join("ls");
    std::fs::write(&coreutils, [0x7F, b'E', b'L', b'F', 0x02, 0x01]).unwrap();
    std::fs::hard_link(&coreutils, &ls).unwrap();

    let stripped = auto_strip_elf_files(&dest, fake_strip.to_str().unwrap()).unwrap();

    let coreutils_meta = coreutils.metadata().unwrap();
    let ls_meta = ls.metadata().unwrap();
    assert_eq!(stripped, 2);
    assert_eq!(coreutils_meta.ino(), ls_meta.ino());
    assert_eq!(coreutils_meta.nlink(), 2);
    assert_eq!(ls_meta.nlink(), 2);
    assert_eq!(
        std::fs::read(&coreutils).unwrap(),
        std::fs::read(&ls).unwrap()
    );
}

#[test]
fn compress_manpages_zstd_rewrites_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("dest");
    let man1 = dest.join("usr/share/man/man1");
    std::fs::create_dir_all(&man1).unwrap();

    let page = man1.join("foo.1");
    std::fs::write(&page, b"foo manpage\n").unwrap();
    std::os::unix::fs::symlink("foo.1", man1.join("bar.1")).unwrap();

    let count = compress_manpages_zstd(&dest).unwrap();
    assert_eq!(count, 1);
    assert!(!man1.join("foo.1").exists());
    assert!(man1.join("foo.1.zst").exists());
    assert!(!man1.join("bar.1").exists());

    let link_meta = std::fs::symlink_metadata(man1.join("bar.1.zst")).unwrap();
    assert!(link_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(man1.join("bar.1.zst")).unwrap(),
        PathBuf::from("foo.1.zst")
    );

    let file = std::fs::File::open(man1.join("foo.1.zst")).unwrap();
    let mut decoder = zstd::stream::read::Decoder::new(file).unwrap();
    let mut out = String::new();
    use std::io::Read as _;
    decoder.read_to_string(&mut out).unwrap();
    assert_eq!(out, "foo manpage\n");
}

#[test]
fn install_atomic_rejects_unsafe_keep_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join("etc")).unwrap();
    std::fs::write(destdir.join("etc/locale.gen"), "x").unwrap();

    let keep = vec!["../etc/shadow".to_string()];
    let err = install_atomic(&destdir, &rootfs, &tx_base, &[], &keep)
        .expect_err("expected keep path traversal to be rejected");
    assert!(
        err.to_string()
            .contains("keep paths must not contain traversal")
    );
}

#[test]
fn install_atomic_removes_obsolete_symlink_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join("usr/bin/new"), "ok").unwrap();

    std::os::unix::fs::symlink("../lib/libold.so", rootfs.join("usr/lib/libold.so.link")).unwrap();
    assert!(
        rootfs
            .join("usr/lib/libold.so.link")
            .symlink_metadata()
            .is_ok()
    );

    let remove_paths = vec!["usr/lib/libold.so.link".to_string()];
    let tx = install_atomic(&destdir, &rootfs, &tx_base, &remove_paths, &[]).unwrap();

    assert!(
        rootfs
            .join("usr/lib/libold.so.link")
            .symlink_metadata()
            .is_err()
    );

    tx.rollback().unwrap();
    let restored = rootfs
        .join("usr/lib/libold.so.link")
        .symlink_metadata()
        .expect("symlink should be restored");
    assert!(restored.file_type().is_symlink());
}

#[test]
fn install_atomic_commit_removes_tx_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join("usr/bin/foo"), "x").unwrap();

    let tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();
    let tx_dir = tx.tx_dir.clone();
    assert!(tx_dir.exists());
    tx.commit().unwrap();
    assert!(!tx_dir.exists());
}

#[test]
fn test_install_atomic_symlink_to_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    // Create a symlink bin -> usr/bin in destdir
    std::os::unix::fs::symlink("usr/bin", destdir.join("bin")).unwrap();

    let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    // Verify rootfs/bin is a symlink, not a directory
    let meta = rootfs
        .join("bin")
        .symlink_metadata()
        .expect("bin should exist");
    assert!(meta.file_type().is_symlink(), "bin should be a symlink");
    assert_eq!(
        std::fs::read_link(rootfs.join("bin")).unwrap(),
        std::path::PathBuf::from("usr/bin")
    );
}

#[test]
fn test_walkdir_symlink_behavior() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::os::unix::fs::symlink("target", dir.join("link")).unwrap();

    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        if entry.path().ends_with("link") {
            let ft = entry.file_type();
            assert!(
                !ft.is_dir(),
                "walkdir should NOT report symlink to dir as a directory"
            );
            assert!(ft.is_symlink(), "walkdir SHOULD report it as a symlink");
        }
    }
}

#[test]
fn install_atomic_skips_info_dir_index() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join("usr/info")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/share/info")).unwrap();
    std::fs::write(destdir.join("usr/info/dir"), "legacy index").unwrap();
    std::fs::write(destdir.join("usr/info/dir.bz2"), "legacy index bz2").unwrap();
    std::fs::write(destdir.join("usr/share/info/dir"), "index").unwrap();
    std::fs::write(destdir.join("usr/share/info/dir.gz"), "index gz").unwrap();
    std::fs::write(destdir.join("usr/share/info/ok.info"), "ok").unwrap();

    let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    assert!(!rootfs.join("usr/info/dir").exists());
    assert!(!rootfs.join("usr/info/dir.bz2").exists());
    assert!(!rootfs.join("usr/share/info/dir").exists());
    assert!(!rootfs.join("usr/share/info/dir.gz").exists());
    assert!(rootfs.join("usr/share/info/ok.info").exists());
}

#[test]
fn install_atomic_skips_packlists_and_pod_files() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/core_perl")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/share/doc/perl-error")).unwrap();
    std::fs::write(
        destdir.join("usr/lib/perl5/5.42/core_perl/perllocal.pod"),
        "perllocal",
    )
    .unwrap();
    std::fs::write(
        destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist"),
        "packlist",
    )
    .unwrap();
    std::fs::write(destdir.join("usr/share/doc/perl-error/Error.pod"), "pod").unwrap();
    std::fs::write(destdir.join("usr/share/doc/perl-error/README"), "readme").unwrap();

    let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    assert!(
        !rootfs
            .join("usr/lib/perl5/5.42/core_perl/perllocal.pod")
            .exists()
    );
    assert!(
        !rootfs
            .join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist")
            .exists()
    );
    assert!(!rootfs.join("usr/share/doc/perl-error/Error.pod").exists());
    assert!(rootfs.join("usr/share/doc/perl-error/README").exists());
}

#[test]
fn install_atomic_skips_package_metadata_files() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join(".metadata.toml"), "name='foo'").unwrap();
    std::fs::write(destdir.join(".files.yaml"), "files: []").unwrap();
    std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

    let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    assert!(!rootfs.join(".metadata.toml").exists());
    assert!(!rootfs.join(".files.yaml").exists());
    assert!(rootfs.join("usr/bin/ok").exists());
}

#[test]
fn install_atomic_skips_package_scripts_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join("scripts")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join("scripts/pre_install"), "#!/bin/sh\necho pre\n").unwrap();
    std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

    let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    assert!(!rootfs.join("scripts/pre_install").exists());
    assert!(rootfs.join("usr/bin/ok").exists());
}

#[test]
fn install_atomic_skips_internal_output_staging_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let destdir = tmp.path().join("dest");
    let tx_base = tmp.path().join("tx");
    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(destdir.join(".depot/outputs/clang/usr/bin")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join(".depot/outputs/clang/usr/bin/clang"), "clang").unwrap();
    std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

    let _tx = install_atomic(&destdir, &rootfs, &tx_base, &[], &[]).unwrap();

    assert!(rootfs.join("usr/bin/ok").exists());
    assert!(!rootfs.join(".depot").exists());
}

#[test]
fn generate_manifest_skips_info_dir_index() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(destdir.join("usr/info")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/share/info")).unwrap();
    std::fs::write(destdir.join("usr/info/dir"), "legacy index").unwrap();
    std::fs::write(destdir.join("usr/info/dir.zst"), "legacy index zst").unwrap();
    std::fs::write(destdir.join("usr/share/info/dir"), "index").unwrap();
    std::fs::write(destdir.join("usr/share/info/dir.xz"), "index xz").unwrap();
    std::fs::write(destdir.join("usr/share/info/ok.info"), "ok").unwrap();

    let manifest = generate_manifest_with_dirs(&destdir).unwrap();

    assert!(!manifest.files.contains(&"usr/info/dir".to_string()));
    assert!(!manifest.files.contains(&"usr/info/dir.zst".to_string()));
    assert!(!manifest.files.contains(&"usr/share/info/dir".to_string()));
    assert!(
        !manifest
            .files
            .contains(&"usr/share/info/dir.xz".to_string())
    );
    assert!(
        manifest
            .files
            .contains(&"usr/share/info/ok.info".to_string())
    );
}

#[test]
fn generate_manifest_skips_packlists_and_pod_files() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/core_perl")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/share/doc/perl-error")).unwrap();
    std::fs::write(
        destdir.join("usr/lib/perl5/5.42/core_perl/perllocal.pod"),
        "perllocal",
    )
    .unwrap();
    std::fs::write(
        destdir.join("usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist"),
        "packlist",
    )
    .unwrap();
    std::fs::write(destdir.join("usr/share/doc/perl-error/Error.pod"), "pod").unwrap();
    std::fs::write(destdir.join("usr/share/doc/perl-error/README"), "readme").unwrap();

    let manifest = generate_manifest_with_dirs(&destdir).unwrap();

    assert!(
        !manifest
            .files
            .contains(&"usr/lib/perl5/5.42/core_perl/perllocal.pod".to_string())
    );
    assert!(
        !manifest
            .files
            .contains(&"usr/lib/perl5/5.42/vendor_perl/auto/Error/.packlist".to_string())
    );
    assert!(
        !manifest
            .files
            .contains(&"usr/share/doc/perl-error/Error.pod".to_string())
    );
    assert!(
        manifest
            .files
            .contains(&"usr/share/doc/perl-error/README".to_string())
    );
}

#[test]
fn generate_manifest_skips_package_metadata_files() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(&destdir).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join(".metadata.toml"), "name='foo'").unwrap();
    std::fs::write(destdir.join(".files.yaml"), "files: []").unwrap();
    std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

    let manifest = generate_manifest_with_dirs(&destdir).unwrap();

    assert!(!manifest.files.contains(&".metadata.toml".to_string()));
    assert!(!manifest.files.contains(&".files.yaml".to_string()));
    assert!(manifest.files.contains(&"usr/bin/ok".to_string()));
}

#[test]
fn generate_manifest_skips_package_scripts_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(destdir.join("scripts")).unwrap();
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::write(destdir.join("scripts/pre_install"), "echo pre").unwrap();
    std::fs::write(destdir.join("usr/bin/ok"), "ok").unwrap();

    let manifest = generate_manifest_with_dirs(&destdir).unwrap();

    assert!(!manifest.files.contains(&"scripts/pre_install".to_string()));
    assert!(manifest.files.contains(&"usr/bin/ok".to_string()));
}

#[test]
fn generate_manifest_skips_internal_output_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(destdir.join("usr/bin")).unwrap();
    std::fs::create_dir_all(destdir.join(".depot/outputs/clang/usr/bin")).unwrap();
    std::fs::write(destdir.join("usr/bin/llvm-config"), "ok").unwrap();
    std::fs::write(destdir.join(".depot/outputs/clang/usr/bin/clang"), "clang").unwrap();

    let manifest = generate_manifest_with_dirs(&destdir).unwrap();

    assert!(manifest.files.contains(&"usr/bin/llvm-config".to_string()));
    assert!(
        !manifest
            .files
            .contains(&".depot/outputs/clang/usr/bin/clang".to_string())
    );
}
