use super::*;
use crate::package::{
    Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, Source,
};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn mk_spec(spec_dir: &Path) -> PackageSpec {
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
        alternatives: Alternatives::default(),
        manual_sources: Vec::new(),
        source: vec![Source {
            url: "https://example.com/foo.tar.gz".into(),
            sha256: "skip".into(),
            extract_dir: "foo".into(),
            patches: Vec::new(),
            post_extract: Vec::new(),
            cherry_pick: Vec::new(),
        }],
        build: Build {
            build_type: BuildType::Custom,
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: spec_dir.to_path_buf(),
    }
}

#[test]
fn stage_scripts_from_spec_dir_copies_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(spec_dir.join("scripts/lib")).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();

    std::fs::write(spec_dir.join("scripts/pre_install"), "echo pre").unwrap();
    std::fs::write(spec_dir.join("scripts/lib/common.sh"), "echo lib").unwrap();

    let spec = mk_spec(&spec_dir);
    let staged = stage_scripts_from_spec_dir(&spec, &destdir).unwrap();
    assert!(staged);
    assert!(destdir.join("scripts/pre_install").exists());
    assert!(destdir.join("scripts/lib/common.sh").exists());
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(destdir.join("scripts/pre_install"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }
}

#[test]
fn run_hook_if_present_executes_script() {
    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("scripts");
    let rootfs = tmp.path().join("root");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&rootfs).unwrap();

    std::fs::write(
        scripts.join("pre_install"),
        "echo \"$DEPOT_ACTION:$DEPOT_PHASE:$DEPOT_PACKAGE\" > \"$DEPOT_ROOTFS/hook.out\"\n",
    )
    .unwrap();

    let ran = run_hook_if_present(&scripts, Hook::PreInstall, &rootfs, "foo").unwrap();
    assert!(ran);
    assert_eq!(
        std::fs::read_to_string(rootfs.join("hook.out")).unwrap(),
        "install:pre:foo\n"
    );
}

#[test]
fn run_hook_if_present_uses_safe_script_path() {
    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("scripts");
    let rootfs = tmp.path().join("root");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&rootfs).unwrap();

    std::fs::write(
        scripts.join("pre_install"),
        "echo \"$PATH\" > \"$DEPOT_ROOTFS/path.out\"\n",
    )
    .unwrap();

    let ran = run_hook_if_present(&scripts, Hook::PreInstall, &rootfs, "foo").unwrap();
    assert!(ran);
    assert_eq!(
        std::fs::read_to_string(rootfs.join("path.out"))
            .unwrap()
            .trim_end(),
        crate::runtime_env::safe_script_path()
    );
}

#[test]
fn run_hook_if_present_accepts_compact_script_name() {
    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("scripts");
    let rootfs = tmp.path().join("root");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&rootfs).unwrap();

    std::fs::write(
        scripts.join("postinstall.sh"),
        "echo compact > \"$DEPOT_ROOTFS/hook.out\"\n",
    )
    .unwrap();

    let ran = run_hook_if_present(&scripts, Hook::PostInstall, &rootfs, "foo").unwrap();
    assert!(ran);
    assert_eq!(
        std::fs::read_to_string(rootfs.join("hook.out")).unwrap(),
        "compact\n"
    );
}

#[test]
fn run_hook_if_present_prefers_lib32_specific_script_name() {
    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("scripts");
    let rootfs = tmp.path().join("root");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&rootfs).unwrap();

    std::fs::write(
        scripts.join("post_install"),
        "echo generic > \"$DEPOT_ROOTFS/hook.out\"\n",
    )
    .unwrap();
    std::fs::write(
        scripts.join("post_install-lib32"),
        "echo lib32 > \"$DEPOT_ROOTFS/hook.out\"\n",
    )
    .unwrap();

    let ran = run_hook_if_present(&scripts, Hook::PostInstall, &rootfs, "lib32-foo").unwrap();
    assert!(ran);
    assert_eq!(
        std::fs::read_to_string(rootfs.join("hook.out")).unwrap(),
        "lib32\n"
    );
}

#[test]
fn run_hook_if_present_rejects_ambiguous_names() {
    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("scripts");
    let rootfs = tmp.path().join("root");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&rootfs).unwrap();

    std::fs::write(scripts.join("pre_update"), "echo one").unwrap();
    std::fs::write(scripts.join("pre-update"), "echo two").unwrap();

    let err = run_hook_if_present(&scripts, Hook::PreUpdate, &rootfs, "foo")
        .expect_err("expected ambiguous script names to fail");
    assert!(err.to_string().contains("Ambiguous lifecycle hook"));
}

#[test]
fn run_hook_if_present_with_relative_rootfs_uses_correct_script_and_env_paths() {
    let cwd = std::env::current_dir().unwrap();
    let tmp = tempfile::Builder::new()
        .prefix("depot-hook-rel-rootfs-")
        .tempdir_in(&cwd)
        .unwrap();
    let rootfs_abs = tmp.path().join("root");
    std::fs::create_dir_all(&rootfs_abs).unwrap();
    let rootfs_rel = rootfs_abs.strip_prefix(&cwd).unwrap().to_path_buf();
    let scripts = rootfs_rel.join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();

    std::fs::write(
        scripts.join("pre_install"),
        "echo ok > \"$DEPOT_ROOTFS/hook.out\"\n",
    )
    .unwrap();

    let ran = run_hook_if_present(&scripts, Hook::PreInstall, &rootfs_rel, "foo").unwrap();
    assert!(ran);
    assert_eq!(
        std::fs::read_to_string(rootfs_abs.join("hook.out")).unwrap(),
        "ok\n"
    );
}

#[test]
fn should_bootstrap_host_shell_only_for_chrooted_root_installs_without_shell() {
    assert!(should_bootstrap_host_shell(true, true, false));
    assert!(!should_bootstrap_host_shell(true, true, true));
    assert!(!should_bootstrap_host_shell(true, false, false));
    assert!(!should_bootstrap_host_shell(false, true, false));
}

#[test]
fn parse_ldd_dependency_paths_extracts_absolute_paths() {
    let parsed = parse_ldd_dependency_paths(
            "linux-vdso.so.1 (0x0000)\nlibc.so.6 => /lib/libc.so.6 (0x0000)\n/lib64/ld-linux-x86-64.so.2 (0x0000)\nlibc.so.6 => /lib/libc.so.6 (0x0001)\n",
        )
        .unwrap();
    assert_eq!(
        parsed,
        vec![
            PathBuf::from("/lib/libc.so.6"),
            PathBuf::from("/lib64/ld-linux-x86-64.so.2")
        ]
    );
}

#[test]
fn parse_ldd_dependency_paths_rejects_missing_dependencies() {
    let err = parse_ldd_dependency_paths("libedit.so.0 => not found\n")
        .expect_err("expected ldd parse to fail when a dependency is missing");
    assert!(
        err.to_string()
            .contains("Missing shared-library dependency")
    );
}

#[test]
fn parse_hook_command_candidates_finds_commands_after_assignments_and_operators() {
    let commands = parse_hook_command_candidates(
        "PATH=/tmp:$PATH grep -q foo etc/shells || echo foo >> etc/shells\ncat \"$DEPOT_ROOTFS/usr/bin/find\" | sed 's/x/y/'\n",
    );
    assert_eq!(
        commands,
        vec!["cat".to_string(), "grep".to_string(), "sed".to_string()]
    );
}

#[test]
fn parse_hook_command_candidates_ignores_builtins_and_control_words() {
    let commands = parse_hook_command_candidates("if true; then export FOO=bar; echo hi; fi\n");
    assert!(commands.is_empty());
}

#[test]
fn deferred_hooks_file_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hooks.tsv");
    let hooks = vec![
        DeferredHook {
            pkg_name: "foo".into(),
            hook: Hook::PostInstall,
            script_rel: PathBuf::from("usr/share/depot/foo/scripts/post_install"),
        },
        DeferredHook {
            pkg_name: "bar".into(),
            hook: Hook::PostUpdate,
            script_rel: PathBuf::from("usr/share/depot/bar/scripts/post_update"),
        },
    ];
    write_deferred_hooks(&path, &hooks).unwrap();
    let loaded = read_deferred_hooks(&path).unwrap();
    assert_eq!(loaded.len(), hooks.len());
    assert_eq!(loaded[0].pkg_name, hooks[0].pkg_name);
    assert_eq!(loaded[0].hook, hooks[0].hook);
    assert_eq!(loaded[0].script_rel, hooks[0].script_rel);
    assert_eq!(loaded[1].pkg_name, hooks[1].pkg_name);
    assert_eq!(loaded[1].hook, hooks[1].hook);
    assert_eq!(loaded[1].script_rel, hooks[1].script_rel);
}

#[test]
fn queue_deferred_hook_dedupes_entries() {
    let tmp = tempfile::tempdir().unwrap();
    queue_deferred_hook(
        tmp.path(),
        "foo",
        Hook::PostInstall,
        Path::new("usr/share/depot/foo/scripts/post_install"),
    )
    .unwrap();
    queue_deferred_hook(
        tmp.path(),
        "foo",
        Hook::PostInstall,
        Path::new("usr/share/depot/foo/scripts/post_install"),
    )
    .unwrap();

    let loaded = read_deferred_hooks(&deferred_hooks_file(tmp.path())).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].pkg_name, "foo");
    assert_eq!(loaded[0].hook, Hook::PostInstall);
    assert_eq!(
        loaded[0].script_rel,
        PathBuf::from("usr/share/depot/foo/scripts/post_install")
    );
}

#[test]
fn sync_staged_scripts_to_rootfs_replaces_existing_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let staged = tmp.path().join("staged");
    std::fs::create_dir_all(staged.join("scripts")).unwrap();

    let installed = installed_scripts_dir(&rootfs, "foo");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(installed.join("old"), "old").unwrap();

    std::fs::write(staged.join("scripts/post_install"), "echo ok").unwrap();
    let has_scripts =
        sync_staged_scripts_to_rootfs(&staged.join("scripts"), &rootfs, "foo").unwrap();

    assert!(has_scripts);
    let installed = installed_scripts_dir(&rootfs, "foo");
    assert!(!installed.join("old").exists());
    assert!(installed.join("post_install").exists());
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(installed.join("post_install"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }
}

#[test]
fn sync_staged_scripts_to_rootfs_removes_old_when_none_staged() {
    let tmp = tempfile::tempdir().unwrap();
    let rootfs = tmp.path().join("root");
    let staged = tmp.path().join("staged");
    std::fs::create_dir_all(&staged).unwrap();

    let installed = installed_scripts_dir(&rootfs, "foo");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(installed.join("pre_remove"), "echo old").unwrap();

    let has_scripts =
        sync_staged_scripts_to_rootfs(&staged.join("scripts"), &rootfs, "foo").unwrap();
    assert!(!has_scripts);
    assert!(!installed_scripts_dir(&rootfs, "foo").exists());
}

#[test]
fn stage_scripts_from_spec_dir_stages_legacy_root_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();

    std::fs::write(spec_dir.join("postinstall.sh"), "echo post").unwrap();

    let spec = mk_spec(&spec_dir);
    let staged = stage_scripts_from_spec_dir(&spec, &destdir).unwrap();
    assert!(staged);
    assert!(destdir.join("scripts/post_install").exists());
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(destdir.join("scripts/post_install"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }
}

#[test]
fn stage_scripts_from_spec_dir_stages_lib32_prefixed_legacy_root_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();

    std::fs::write(spec_dir.join("lib32-postinstall.sh"), "echo lib32-post").unwrap();

    let mut spec = mk_spec(&spec_dir);
    spec.package.name = "lib32-foo".into();
    let staged = stage_scripts_from_spec_dir(&spec, &destdir).unwrap();
    assert!(staged);
    assert!(destdir.join("scripts/post_install").exists());
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(destdir.join("scripts/post_install"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);
    }
}

#[test]
fn stage_scripts_from_spec_dir_lib32_ignores_generic_legacy_root_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let spec_dir = tmp.path().join("spec");
    let destdir = tmp.path().join("dest");
    std::fs::create_dir_all(&spec_dir).unwrap();
    std::fs::create_dir_all(&destdir).unwrap();

    // No lib32-prefixed hook; native-only scripts must NOT be staged for lib32 packages.
    std::fs::write(spec_dir.join("postinstall.sh"), "echo fallback").unwrap();

    let mut spec = mk_spec(&spec_dir);
    spec.package.name = "lib32-foo".into();
    let staged = stage_scripts_from_spec_dir(&spec, &destdir).unwrap();
    assert!(!staged);
    assert!(!destdir.join("scripts/post_install").exists());
}

#[test]
fn run_hook_if_present_lib32_ignores_generic_script() {
    let tmp = tempfile::tempdir().unwrap();
    let scripts = tmp.path().join("scripts");
    let rootfs = tmp.path().join("root");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::create_dir_all(&rootfs).unwrap();

    // Only a generic script exists; lib32 package must NOT execute it.
    std::fs::write(
        scripts.join("post_install"),
        "echo generic > \"$DEPOT_ROOTFS/hook.out\"\n",
    )
    .unwrap();

    let ran = run_hook_if_present(&scripts, Hook::PostInstall, &rootfs, "lib32-foo").unwrap();
    assert!(!ran);
    assert!(!rootfs.join("hook.out").exists());
}
