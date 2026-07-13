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
    crate::fs_copy::copy_tree_preserving_links(src, dst)
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
mod tests;
