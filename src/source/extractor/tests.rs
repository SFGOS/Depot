use super::*;
use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo};
use lz4_flex::frame::FrameEncoder as Lz4FrameEncoder;
use lzma_rust2::{LzipOptions, LzipWriter, LzmaOptions, LzmaWriter};
use std::io::Write;
use std::time::{Duration, SystemTime};
use tempfile::tempdir;

fn test_spec() -> PackageSpec {
    PackageSpec {
        package: PackageInfo {
            name: "pkg".into(),
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
            flags: BuildFlags::default(),
        },
        dependencies: Dependencies::default(),
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    }
}

fn test_source(extract_dir: &str) -> Source {
    Source {
        url: "https://example.test/src.tar".into(),
        sha256: "skip".into(),
        extract_dir: extract_dir.into(),
        patches: Vec::new(),
        post_extract: Vec::new(),
        cherry_pick: Vec::new(),
    }
}

fn simple_tar_bytes(top_dir: &str, file_name: &str, contents: &[u8]) -> Vec<u8> {
    let mut tar_buf = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut tar_buf);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, format!("{top_dir}/{file_name}"), contents)
            .unwrap();
        tar.finish().unwrap();
    }
    tar_buf
}

#[test]
fn test_extract_deb_roundtrip() {
    let tmp = tempdir().unwrap();
    let deb_path = tmp.path().join("test.deb");
    let extract_dir = tmp.path().join("out-deb");

    // create a small tar.gz payload with one file
    let mut tar_buf = Vec::new();
    {
        let gz = flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        let data = b"hello-deb";
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "usr/bin/hello-deb", &data[..])
            .unwrap();
        tar.finish().unwrap();
    }

    // write ar archive with member data.tar.gz (use temp files because ar::Builder expects File)
    {
        let db = tmp.path().join("debian-binary");
        let ct = tmp.path().join("control.tar.gz");
        let dt = tmp.path().join("data.tar.gz");
        std::fs::write(&db, b"2.0\n").unwrap();
        std::fs::write(&ct, b"").unwrap();
        std::fs::write(&dt, &tar_buf[..]).unwrap();

        let mut f = File::create(&deb_path).unwrap();
        let mut builder = ar::Builder::new(&mut f);
        let mut dbf = File::open(&db).unwrap();
        let mut ctf = File::open(&ct).unwrap();
        let mut dtf = File::open(&dt).unwrap();
        builder.append_file(b"debian-binary", &mut dbf).unwrap();
        builder.append_file(b"control.tar.gz", &mut ctf).unwrap();
        builder.append_file(b"data.tar.gz", &mut dtf).unwrap();
    }

    fs::create_dir_all(&extract_dir).unwrap();
    extract_deb(&deb_path, &extract_dir).unwrap();
    assert!(extract_dir.join("usr/bin/hello-deb").exists());
}

#[test]
fn test_archive_format_for_new_extensions() {
    assert_eq!(
        archive_format_for_filename("pkg.tar.lz4"),
        Some(ArchiveFormat::TarLz4)
    );
    assert_eq!(
        archive_format_for_filename("pkg.tar.lzma"),
        Some(ArchiveFormat::TarLzma)
    );
    assert_eq!(
        archive_format_for_filename("pkg.tar.lz"),
        Some(ArchiveFormat::TarLzip)
    );
    assert_eq!(
        archive_format_for_filename("pkg.tar.Z"),
        Some(ArchiveFormat::TarCompress)
    );
    assert_eq!(
        archive_format_for_filename("pkg.cpio"),
        Some(ArchiveFormat::Cpio)
    );
}

#[test]
fn test_extract_archive_tar_lz4_roundtrip() {
    let tmp = tempdir().unwrap();
    let archive_path = tmp.path().join("pkg.tar.lz4");
    let build_dir = tmp.path().join("build");
    let tar_buf = simple_tar_bytes("pkg-1.0", "hello.txt", b"hello-lz4");

    let mut compressed = Vec::new();
    {
        let mut encoder = Lz4FrameEncoder::new(&mut compressed);
        encoder.write_all(&tar_buf).unwrap();
        encoder.finish().unwrap();
    }
    fs::write(&archive_path, compressed).unwrap();

    let extracted = extract_archive(
        &archive_path,
        &test_spec(),
        &test_source("pkg-1.0"),
        &build_dir,
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(extracted.join("hello.txt")).unwrap(),
        "hello-lz4"
    );
}

#[test]
fn test_extract_archive_tar_lzma_roundtrip() {
    let tmp = tempdir().unwrap();
    let archive_path = tmp.path().join("pkg.tar.lzma");
    let build_dir = tmp.path().join("build");
    let tar_buf = simple_tar_bytes("pkg-1.0", "hello.txt", b"hello-lzma");

    let mut compressed = Vec::new();
    {
        let options = LzmaOptions::default();
        let mut writer =
            LzmaWriter::new_use_header(&mut compressed, &options, Some(tar_buf.len() as u64))
                .unwrap();
        writer.write_all(&tar_buf).unwrap();
        writer.finish().unwrap();
    }
    fs::write(&archive_path, compressed).unwrap();

    let extracted = extract_archive(
        &archive_path,
        &test_spec(),
        &test_source("pkg-1.0"),
        &build_dir,
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(extracted.join("hello.txt")).unwrap(),
        "hello-lzma"
    );
}

#[test]
fn test_extract_archive_tar_lzip_roundtrip() {
    let tmp = tempdir().unwrap();
    let archive_path = tmp.path().join("pkg.tar.lz");
    let build_dir = tmp.path().join("build");
    let tar_buf = simple_tar_bytes("pkg-1.0", "hello.txt", b"hello-lzip");

    let mut compressed = Vec::new();
    {
        let mut writer = LzipWriter::new(&mut compressed, LzipOptions::default());
        writer.write_all(&tar_buf).unwrap();
        writer.finish().unwrap();
    }
    fs::write(&archive_path, compressed).unwrap();

    let extracted = extract_archive(
        &archive_path,
        &test_spec(),
        &test_source("pkg-1.0"),
        &build_dir,
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(extracted.join("hello.txt")).unwrap(),
        "hello-lzip"
    );
}

#[test]
fn test_extract_archive_cpio_roundtrip() {
    let tmp = tempdir().unwrap();
    let archive_path = tmp.path().join("pkg.cpio");
    let build_dir = tmp.path().join("build");

    let mut cpio = Vec::new();
    write_cpio_newc_one_file(&mut cpio, "pkg-1.0/hello.txt", b"hello-cpio");
    write_cpio_trailer(&mut cpio);
    fs::write(&archive_path, cpio).unwrap();

    let extracted = extract_archive(
        &archive_path,
        &test_spec(),
        &test_source("pkg-1.0"),
        &build_dir,
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(extracted.join("hello.txt")).unwrap(),
        "hello-cpio"
    );
}

#[test]
fn copy_file_preserve_metadata_keeps_mtime() {
    let tmp = tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    std::fs::write(&src, b"hello").unwrap();

    let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(946684800); // 2000-01-01 UTC
    let ts = FileTime::from_system_time(fixed);
    filetime::set_file_times(&src, ts, ts).unwrap();

    copy_file_preserve_metadata(&src, &dst).unwrap();
    let src_meta = std::fs::metadata(&src).unwrap();
    let dst_meta = std::fs::metadata(&dst).unwrap();
    assert_eq!(
        FileTime::from_last_modification_time(&dst_meta),
        FileTime::from_last_modification_time(&src_meta)
    );
}

#[test]
fn copy_entry_fallback_preserves_symlink_when_target_is_missing() {
    let tmp = tempdir().unwrap();
    let src_dir = tmp.path().join("src");
    let dst_dir = tmp.path().join("dst");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dst_dir).unwrap();

    let src_link = src_dir.join("RELEASE-NOTES");
    std::os::unix::fs::symlink("doc/RelNotes/v1.47.3.txt", &src_link).unwrap();

    // Simulate target already moved/removed before copying this symlink entry.
    let src_target_dir = src_dir.join("doc");
    std::fs::create_dir_all(src_target_dir.join("RelNotes")).unwrap();
    std::fs::write(src_target_dir.join("RelNotes/v1.47.3.txt"), "notes").unwrap();
    std::fs::remove_dir_all(&src_target_dir).unwrap();

    let dst_link = dst_dir.join("RELEASE-NOTES");
    let file_type = std::fs::symlink_metadata(&src_link).unwrap().file_type();
    copy_entry_fallback(&src_link, &dst_link, file_type).unwrap();

    assert!(std::fs::symlink_metadata(&src_link).is_err());
    let dst_meta = std::fs::symlink_metadata(&dst_link).unwrap();
    assert!(dst_meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(&dst_link).unwrap(),
        PathBuf::from("doc/RelNotes/v1.47.3.txt")
    );
}

fn write_cpio_newc_one_file(w: &mut Vec<u8>, name: &str, data: &[u8]) {
    // magic + 13 fields of 8 hex chars each
    fn h8(v: u64) -> String {
        format!("{:08x}", v)
    }
    let namesize = name.len() + 1;
    let filesize = data.len();
    let mut header = Vec::new();
    header.extend_from_slice(b"070701");
    // ino, mode, uid, gid, nlink, mtime, filesize, devmajor, devminor, rdevmajor, rdevminor, namesize, check
    header.extend_from_slice(h8(0).as_bytes()); // ino
    header.extend_from_slice(h8(0o100644).as_bytes()); // mode regular file with perms
    header.extend_from_slice(h8(0).as_bytes()); // uid
    header.extend_from_slice(h8(0).as_bytes()); // gid
    header.extend_from_slice(h8(1).as_bytes()); // nlink
    header.extend_from_slice(h8(0).as_bytes()); // mtime
    header.extend_from_slice(h8(filesize as u64).as_bytes());
    header.extend_from_slice(h8(0).as_bytes()); // devmajor
    header.extend_from_slice(h8(0).as_bytes()); // devminor
    header.extend_from_slice(h8(0).as_bytes()); // rdevmajor
    header.extend_from_slice(h8(0).as_bytes()); // rdevminor
    header.extend_from_slice(h8(namesize as u64).as_bytes());
    header.extend_from_slice(h8(0).as_bytes()); // check
    w.extend_from_slice(&header);
    w.extend_from_slice(name.as_bytes());
    w.push(0);
    // pad to 4
    let pad = (4 - ((110 + namesize) % 4)) % 4;
    for _ in 0..pad {
        w.push(0);
    }
    // file data
    w.extend_from_slice(data);
    let dpad = (4 - (filesize % 4)) % 4;
    for _ in 0..dpad {
        w.push(0);
    }
}

fn write_cpio_trailer(w: &mut Vec<u8>) {
    write_cpio_newc_one_file(w, "TRAILER!!!", &[]);
}

#[test]
fn test_extract_rpm_roundtrip() {
    let tmp = tempdir().unwrap();
    let rpm_path = tmp.path().join("test.rpm");
    let extract_dir = tmp.path().join("out-rpm");

    // build cpio newc stream with one file
    let mut cpio = Vec::new();
    write_cpio_newc_one_file(&mut cpio, "usr/bin/hello-rpm", b"hello-rpm");
    write_cpio_trailer(&mut cpio);

    // gzip compress
    let mut gz = Vec::new();
    {
        let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
        enc.write_all(&cpio).unwrap();
        enc.finish().unwrap();
    }

    // write fake rpm: some header bytes then gz payload
    {
        let mut f = File::create(&rpm_path).unwrap();
        f.write_all(b"RPMHEAD").unwrap();
        f.write_all(&gz).unwrap();
    }

    fs::create_dir_all(&extract_dir).unwrap();
    extract_rpm(&rpm_path, &extract_dir).unwrap();
    assert!(extract_dir.join("usr/bin/hello-rpm").exists());
}
