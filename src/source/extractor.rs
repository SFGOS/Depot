//! Archive extraction support

use crate::package::{PackageSpec, Source};
use anyhow::{Context, Result, bail};
use filetime::FileTime;
use flate2::read::GzDecoder;
use lz4_flex::frame::FrameDecoder as Lz4FrameDecoder;
use lzma_rust2::{LzipReader, LzmaReader};
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::{NamedTempFile, tempdir};
use walkdir::WalkDir;
use zstd::stream::read::Decoder as ZstdDecoder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveFormat {
    TarGz,
    TarXz,
    TarBz2,
    TarZst,
    TarLz4,
    TarLzma,
    TarLzip,
    TarCompress,
    Zip,
    Tar,
    Deb,
    Rpm,
    Cpio,
    GzFile,
    XzFile,
    ZstFile,
}

/// Extract an archive source to the build directory.
pub fn extract_archive(
    archive_path: &Path,
    spec: &PackageSpec,
    source: &Source,
    build_dir: &Path,
) -> Result<PathBuf> {
    crate::interrupts::install()?;
    let extract_dir_name = spec.expand_vars(&source.extract_dir);
    let extract_path = build_dir.join(&extract_dir_name);

    // Create build directory
    fs::create_dir_all(build_dir)
        .with_context(|| format!("Failed to create build dir: {}", build_dir.display()))?;

    // Remove existing extraction if present
    if extract_path.exists() {
        fs::remove_dir_all(&extract_path)?;
    }

    let filename = archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    crate::log_info!("Extracting: {}", filename);

    match archive_format_for_filename(filename) {
        Some(ArchiveFormat::TarGz) => extract_tar_gz(archive_path, &extract_path)?,
        Some(ArchiveFormat::TarXz) => extract_tar_xz(archive_path, &extract_path)?,
        Some(ArchiveFormat::TarBz2) => extract_tar_bz2(archive_path, &extract_path)?,
        Some(ArchiveFormat::TarZst) => extract_tar_zst(archive_path, &extract_path)?,
        Some(ArchiveFormat::TarLz4) => extract_tar_lz4(archive_path, &extract_path)?,
        Some(ArchiveFormat::TarLzma) => extract_tar_lzma(archive_path, &extract_path)?,
        Some(ArchiveFormat::TarLzip) => extract_tar_lzip(archive_path, &extract_path)?,
        Some(ArchiveFormat::TarCompress) => extract_tar_compress(archive_path, &extract_path)?,
        Some(ArchiveFormat::Zip) => extract_zip(archive_path, &extract_path)?,
        Some(ArchiveFormat::Tar) => extract_tar(archive_path, &extract_path)?,
        Some(ArchiveFormat::Deb) => extract_deb(archive_path, &extract_path)?,
        Some(ArchiveFormat::Rpm) => extract_rpm(archive_path, &extract_path)?,
        Some(ArchiveFormat::Cpio) => extract_cpio(archive_path, &extract_path)?,
        Some(ArchiveFormat::GzFile) => extract_gz_file(archive_path, &extract_path)?,
        Some(ArchiveFormat::XzFile) => extract_xz_file(archive_path, &extract_path)?,
        Some(ArchiveFormat::ZstFile) => extract_zst_file(archive_path, &extract_path)?,
        None => bail!("Unsupported archive format: {}", filename),
    }

    if !extract_path.exists() {
        bail!(
            "Extraction did not create expected path: {}",
            extract_path.display()
        );
    }

    crate::log_info!("Extracted to: {}", extract_path.display());
    Ok(extract_path)
}

fn archive_format_for_filename(filename: &str) -> Option<ArchiveFormat> {
    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
        Some(ArchiveFormat::TarXz)
    } else if filename.ends_with(".tar.bz2") || filename.ends_with(".tbz2") {
        Some(ArchiveFormat::TarBz2)
    } else if filename.ends_with(".tar.zst") || filename.ends_with(".tzst") {
        Some(ArchiveFormat::TarZst)
    } else if filename.ends_with(".tar.lz4") {
        Some(ArchiveFormat::TarLz4)
    } else if filename.ends_with(".tar.lzma") {
        Some(ArchiveFormat::TarLzma)
    } else if filename.ends_with(".tar.lz") {
        Some(ArchiveFormat::TarLzip)
    } else if filename.ends_with(".tar.Z") {
        Some(ArchiveFormat::TarCompress)
    } else if filename.ends_with(".zip") {
        Some(ArchiveFormat::Zip)
    } else if filename.ends_with(".tar") {
        Some(ArchiveFormat::Tar)
    } else if filename.ends_with(".deb") {
        Some(ArchiveFormat::Deb)
    } else if filename.ends_with(".rpm") {
        Some(ArchiveFormat::Rpm)
    } else if filename.ends_with(".cpio") {
        Some(ArchiveFormat::Cpio)
    } else if filename.ends_with(".gz") {
        Some(ArchiveFormat::GzFile)
    } else if filename.ends_with(".xz") {
        Some(ArchiveFormat::XzFile)
    } else if filename.ends_with(".zst") {
        Some(ArchiveFormat::ZstFile)
    } else {
        None
    }
}

fn extract_tar_gz(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = GzDecoder::new(file);
    extract_tar_reader(decoder, dest)
}

fn extract_tar_xz(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = xz2::read::XzDecoder::new(file);
    extract_tar_reader(decoder, dest)
}

fn extract_tar_bz2(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = bzip2::read::BzDecoder::new(file);
    extract_tar_reader(decoder, dest)
}

fn extract_tar(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    extract_tar_reader(file, dest)
}

fn extract_zip(path: &Path, dest: &Path) -> Result<()> {
    let tmp = tempdir()?;
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    extract_zip_archive(&mut archive, tmp.path())?;
    finalize_extracted_tree(tmp.path(), dest)
}

fn extract_tar_zst(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = ZstdDecoder::new(file)?;
    extract_tar_reader(decoder, dest)
}

fn extract_tar_lz4(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = Lz4FrameDecoder::new(file);
    extract_tar_reader(decoder, dest)
}

fn extract_tar_lzma(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = LzmaReader::new_mem_limit(file, u32::MAX, None)?;
    extract_tar_reader(decoder, dest)
}

fn extract_tar_lzip(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = LzipReader::new(file);
    extract_tar_reader(decoder, dest)
}

fn extract_tar_compress(path: &Path, dest: &Path) -> Result<()> {
    let tmp = tempdir()?;
    let mut child = Command::new("gzip");
    child.arg("-cd").arg(path);
    child.stdout(Stdio::piped());
    crate::interrupts::check()?;
    // Run gzip in its own process group so Ctrl-C can interrupt decompression too.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            child.pre_exec(|| {
                if nix::libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = child.spawn().with_context(|| {
        format!(
            "Failed to spawn gzip for .Z decompression: {}",
            path.display()
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture gzip stdout")?;
    let mut archive = tar::Archive::new(stdout);
    let unpack_result = crate::interrupts::unpack_tar_archive(&mut archive, tmp.path())
        .with_context(|| format!("Failed to unpack .tar.Z archive {}", path.display()));
    drop(archive);
    let status = wait_for_child_interruptibly(&mut child)
        .with_context(|| format!("Failed waiting for gzip on {}", path.display()))?;
    unpack_result?;
    if !status.success() {
        bail!("gzip failed while decompressing {}", path.display());
    }
    finalize_extracted_tree(tmp.path(), dest)
}

fn extract_cpio(path: &Path, dest: &Path) -> Result<()> {
    let tmp = tempdir()?;
    let file = File::open(path)?;
    extract_cpio_newc_from_reader(file, tmp.path())?;
    finalize_extracted_tree(tmp.path(), dest)
}

fn extract_tar_reader<R: Read>(reader: R, dest: &Path) -> Result<()> {
    let tmp = tempdir()?;
    let mut archive = tar::Archive::new(reader);
    crate::interrupts::unpack_tar_archive(&mut archive, tmp.path())?;
    finalize_extracted_tree(tmp.path(), dest)
}

fn finalize_extracted_tree(src_root: &Path, dest: &Path) -> Result<()> {
    let top = fs::read_dir(src_root)?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();
    if top.len() == 1 && top[0].path().is_dir() {
        let top_name = top[0].file_name().to_string_lossy().to_string();
        let expected_basename = dest.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let sys_blacklist = [
            "usr", "bin", "sbin", "lib", "lib64", "etc", "share", "opt", "var", "run", "dev",
            "proc", "sys", "boot", "srv", "home",
        ];

        let looks_like_versioned =
            |s: &str| s.contains('-') && s.chars().any(|c| c.is_ascii_digit());
        let should_strip = (!sys_blacklist.contains(&top_name.as_str()))
            && (top_name == expected_basename
                || (!expected_basename.is_empty() && top_name.contains(expected_basename))
                || looks_like_versioned(&top_name));

        if should_strip {
            move_dir_contents(&top[0].path(), dest)?;
        } else {
            fs::create_dir_all(dest)?;
            let dest_top = dest.join(top_name);
            if fs::rename(top[0].path(), &dest_top).is_err() {
                copy_dir_recursive_local(&top[0].path(), &dest_top)?;
                fs::remove_dir_all(top[0].path())?;
            }
        }
    } else {
        move_dir_contents(src_root, dest)?;
    }
    Ok(())
}

fn extract_gz_file(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let mut decoder = GzDecoder::new(file);

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");

    let out_name = if let Some(stripped) = filename.strip_suffix(".gz") {
        stripped
    } else {
        filename
    };

    fs::create_dir_all(dest)?;
    let mut out = File::create(dest.join(out_name))?;
    crate::interrupts::copy_interruptibly(&mut decoder, &mut out)?;
    Ok(())
}

fn extract_xz_file(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let mut decoder = xz2::read::XzDecoder::new(file);

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");

    let out_name = if let Some(stripped) = filename.strip_suffix(".xz") {
        stripped
    } else {
        filename
    };

    fs::create_dir_all(dest)?;
    let mut out = File::create(dest.join(out_name))?;
    crate::interrupts::copy_interruptibly(&mut decoder, &mut out)?;
    Ok(())
}

fn extract_zst_file(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let mut decoder = ZstdDecoder::new(file)?;

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");

    let out_name = if let Some(stripped) = filename.strip_suffix(".zst") {
        stripped
    } else {
        filename
    };

    fs::create_dir_all(dest)?;
    let mut out = File::create(dest.join(out_name))?;
    crate::interrupts::copy_interruptibly(&mut decoder, &mut out)?;
    Ok(())
}

fn extract_deb(path: &Path, dest: &Path) -> Result<()> {
    // Debian packages are ar archives containing (among others) a data.tar.* member
    let file = File::open(path)?;
    let mut ar = ar::Archive::new(file);

    while let Some(entry_result) = ar.next_entry() {
        crate::interrupts::check()?;
        let mut entry = entry_result?;
        let id = String::from_utf8_lossy(entry.header().identifier()).to_string();
        let lower = id.to_ascii_lowercase();
        if lower.starts_with("data.tar") {
            // write the inner member to a temporary file and reuse tar extraction logic
            let mut tmpf = NamedTempFile::new()?;
            crate::interrupts::copy_interruptibly(&mut entry, &mut tmpf)?;
            let tmp_path = tmpf.path().to_path_buf();

            if lower.ends_with(".gz") {
                extract_tar_gz(&tmp_path, dest)?;
                return Ok(());
            } else if lower.ends_with(".xz") {
                extract_tar_xz(&tmp_path, dest)?;
                return Ok(());
            } else if lower.ends_with(".zst") {
                extract_tar_zst(&tmp_path, dest)?;
                return Ok(());
            } else {
                extract_tar(&tmp_path, dest)?;
                return Ok(());
            }
        }
    }

    anyhow::bail!("No data.tar.* member found in deb: {}", path.display());
}

fn extract_cpio_newc_from_reader<R: Read>(mut r: R, dest: &Path) -> Result<()> {
    use std::str;
    loop {
        crate::interrupts::check()?;
        // read 6-byte magic
        let mut magic = [0u8; 6];
        if let Err(e) = r.read_exact(&mut magic) {
            // EOF
            return Err(e).with_context(|| "Failed reading cpio magic")?;
        }
        if &magic != b"070701" {
            anyhow::bail!("Unsupported cpio magic: {:?}", &magic);
        }

        // read remaining 104 bytes of header (total header is 110)
        let mut rest = [0u8; 104];
        r.read_exact(&mut rest)?;
        let header_str = str::from_utf8(&rest).context("Invalid cpio header encoding")?;
        // parse 13 hex fields of 8 chars each
        if header_str.len() < 8 * 13 {
            anyhow::bail!("Truncated cpio header");
        }
        let mut fields = [0u64; 13];
        for i in 0..13 {
            let part = &header_str[i * 8..i * 8 + 8];
            fields[i] = u64::from_str_radix(part, 16)
                .with_context(|| format!("Invalid header field hex: {}", part))?;
        }

        let namesize = fields[11] as usize;
        let filesize = fields[6] as usize;

        // read name
        let mut name_buf = vec![0u8; namesize];
        r.read_exact(&mut name_buf)?;
        let name = match name_buf.iter().position(|&b| b == 0) {
            Some(p) => String::from_utf8_lossy(&name_buf[..p]).to_string(),
            None => String::from_utf8_lossy(&name_buf).to_string(),
        };

        // skip header padding to 4 bytes
        let header_total = 110 + namesize;
        let header_pad = (4 - (header_total % 4)) % 4;
        if header_pad > 0 {
            let mut tmp = vec![0u8; header_pad];
            r.read_exact(&mut tmp)?;
        }

        if name == "TRAILER!!!" {
            break;
        }

        let mode = fields[1];
        let file_path = dest.join(&name);

        if (mode & 0o170000) == 0o040000 {
            // directory
            fs::create_dir_all(&file_path)?;
        } else if (mode & 0o170000) == 0o120000 {
            // symlink
            let mut link_buf = vec![0u8; filesize];
            r.read_exact(&mut link_buf)?;
            let target = String::from_utf8_lossy(&link_buf).to_string();
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let _ = fs::remove_file(&file_path);
            unix_fs::symlink(target, &file_path)?;
        } else {
            // regular file
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&file_path)?;
            let mut remaining = filesize;
            let mut buf = [0u8; 8192];
            while remaining > 0 {
                crate::interrupts::check()?;
                let to_read = std::cmp::min(remaining, buf.len());
                let n = r.read(&mut buf[..to_read])?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                remaining -= n;
            }
            // set permissions
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &file_path,
                fs::Permissions::from_mode((mode & 0o7777) as u32),
            )?;
        }

        // skip file data padding
        let data_pad = (4 - (filesize % 4)) % 4;
        if data_pad > 0 {
            let mut tmp = vec![0u8; data_pad];
            r.read_exact(&mut tmp)?;
        }
    }

    Ok(())
}

fn extract_rpm(path: &Path, dest: &Path) -> Result<()> {
    // Read entire file and search for compression/cpio magic
    crate::interrupts::check()?;
    let data = std::fs::read(path)?;

    // search for known signatures
    let gz_sig = b"\x1f\x8b";
    let xz_sig = b"\xfd7zXZ\x00";
    let zst_sig = b"\x28\xb5\x2f\xfd";
    let cpio_sig = b"070701";

    // Extract into a temporary directory first, then move/strip into `dest`.
    let tmp = tempdir()?;

    if let Some(pos) = find_subslice(&data, gz_sig) {
        let cursor = Cursor::new(&data[pos..]);
        let decoder = GzDecoder::new(cursor);
        extract_cpio_newc_from_reader(decoder, tmp.path())?;
        move_dir_contents(tmp.path(), dest)?;
        return Ok(());
    }
    if let Some(pos) = find_subslice(&data, xz_sig) {
        let cursor = Cursor::new(&data[pos..]);
        let decoder = xz2::read::XzDecoder::new(cursor);
        extract_cpio_newc_from_reader(decoder, tmp.path())?;
        move_dir_contents(tmp.path(), dest)?;
        return Ok(());
    }
    if let Some(pos) = find_subslice(&data, zst_sig) {
        let cursor = Cursor::new(&data[pos..]);
        let decoder = ZstdDecoder::new(cursor)?;
        extract_cpio_newc_from_reader(decoder, tmp.path())?;
        move_dir_contents(tmp.path(), dest)?;
        return Ok(());
    }
    if let Some(pos) = find_subslice(&data, cpio_sig) {
        let cursor = Cursor::new(&data[pos..]);
        extract_cpio_newc_from_reader(cursor, tmp.path())?;
        move_dir_contents(tmp.path(), dest)?;
        return Ok(());
    }

    anyhow::bail!(
        "No recognizable cpio payload found in rpm: {}",
        path.display()
    );
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Move contents of `src` into `dest`. If `src` contains exactly one directory
/// and `strip_single_top` is intended, callers can pass the inner dir instead.
fn move_dir_contents(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        crate::interrupts::check()?;
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let file_type = entry.file_type()?;
        if fs::rename(&src_path, &dest_path).is_err() {
            // fallback to copy-and-remove across filesystems
            copy_entry_fallback(&src_path, &dest_path, file_type)?;
        }
    }
    Ok(())
}

fn copy_entry_fallback(src_path: &Path, dest_path: &Path, file_type: fs::FileType) -> Result<()> {
    if file_type.is_dir() {
        copy_dir_recursive_local(src_path, dest_path)?;
        fs::remove_dir_all(src_path)?;
    } else if file_type.is_symlink() {
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let link_target = fs::read_link(src_path)
            .with_context(|| format!("Failed to read symlink {}", src_path.display()))?;
        unix_fs::symlink(&link_target, dest_path).with_context(|| {
            format!(
                "Failed to create symlink {} -> {}",
                dest_path.display(),
                link_target.display()
            )
        })?;
        fs::remove_file(src_path)?;
    } else {
        copy_file_preserve_metadata(src_path, dest_path)?;
        fs::remove_file(src_path)?;
    }
    Ok(())
}

fn copy_file_preserve_metadata(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(src, dst)
        .with_context(|| format!("Failed to copy {} -> {}", src.display(), dst.display()))?;

    let src_meta = fs::metadata(src)
        .with_context(|| format!("Failed to read metadata for {}", src.display()))?;
    fs::set_permissions(dst, src_meta.permissions())
        .with_context(|| format!("Failed to set permissions for {}", dst.display()))?;

    let atime = FileTime::from_last_access_time(&src_meta);
    let mtime = FileTime::from_last_modification_time(&src_meta);
    filetime::set_file_times(dst, atime, mtime)
        .with_context(|| format!("Failed to preserve file timestamps for {}", dst.display()))?;

    Ok(())
}

fn copy_dir_recursive_local(src: &Path, dst: &Path) -> Result<()> {
    for entry in WalkDir::new(src) {
        crate::interrupts::check()?;
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap();
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_symlink() {
            let target_link = fs::read_link(entry.path())?;
            unix_fs::symlink(target_link, &target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            copy_file_preserve_metadata(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn extract_zip_archive<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    dest: &Path,
) -> Result<()> {
    for index in 0..archive.len() {
        crate::interrupts::check()?;
        let mut entry = archive.by_index(index)?;
        let enclosed = entry
            .enclosed_name()
            .with_context(|| format!("Zip archive entry has unsafe path: {}", entry.name()))?;
        let out_path = dest.join(enclosed);

        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }

        let mode = entry.unix_mode().unwrap_or(0);
        if (mode & 0o170000) == 0o120000 {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut target = Vec::new();
            crate::interrupts::copy_interruptibly(&mut entry, &mut target)?;
            let target = String::from_utf8(target).context("Zip symlink target was not UTF-8")?;
            let _ = fs::remove_file(&out_path);
            unix_fs::symlink(target, &out_path)?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&out_path)?;
        crate::interrupts::copy_interruptibly(&mut entry, &mut out)?;
        if mode != 0 {
            fs::set_permissions(&out_path, fs::Permissions::from_mode(mode & 0o7777))?;
        }
    }

    Ok(())
}

fn wait_for_child_interruptibly(
    child: &mut std::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    let mut interrupted_at = None;
    let mut sent_term = false;
    let mut sent_kill = false;

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }

        if crate::interrupts::was_interrupted() {
            if interrupted_at.is_none() {
                interrupted_at = Some(std::time::Instant::now());
                signal_child_group(child, nix::libc::SIGINT);
            } else if interrupted_at.is_some_and(|started| {
                started.elapsed() >= std::time::Duration::from_secs(2) && !sent_term
            }) {
                sent_term = true;
                signal_child_group(child, nix::libc::SIGTERM);
            } else if interrupted_at.is_some_and(|started| {
                started.elapsed() >= std::time::Duration::from_secs(4) && !sent_kill
            }) {
                sent_kill = true;
                signal_child_group(child, nix::libc::SIGKILL);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn signal_child_group(child: &std::process::Child, signal: i32) {
    let rc = unsafe { nix::libc::kill(-(child.id() as i32), signal) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(nix::libc::ESRCH) {
            crate::log_warn!(
                "Failed to forward signal {} to extractor child process group {}: {}",
                signal,
                child.id(),
                err
            );
        }
    }
}

#[cfg(test)]
mod tests {
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
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
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
}
