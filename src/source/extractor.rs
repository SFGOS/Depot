//! Archive extraction support

use crate::package::{PackageSpec, Source};
use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use zstd::stream::read::Decoder as ZstdDecoder;

/// Extract an archive source to the build directory.
pub fn extract_archive(
    archive_path: &Path,
    spec: &PackageSpec,
    source: &Source,
    build_dir: &Path,
) -> Result<PathBuf> {
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

    println!("Extracting: {}", filename);

    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        extract_tar_gz(archive_path, build_dir)?;
    } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
        extract_tar_xz(archive_path, build_dir)?;
    } else if filename.ends_with(".tar.bz2") || filename.ends_with(".tbz2") {
        extract_tar_bz2(archive_path, build_dir)?;
    } else if filename.ends_with(".tar.zst") || filename.ends_with(".tzst") {
        extract_tar_zst(archive_path, build_dir)?;
    } else if filename.ends_with(".zip") {
        extract_zip(archive_path, build_dir)?;
    } else if filename.ends_with(".tar") {
        extract_tar(archive_path, build_dir)?;
    } else if filename.ends_with(".deb") {
        extract_deb(archive_path, build_dir)?;
    } else if filename.ends_with(".rpm") {
        extract_rpm(archive_path, build_dir)?;
    } else if filename.ends_with(".gz") {
        extract_gz_file(archive_path, build_dir)?;
    } else if filename.ends_with(".xz") {
        extract_xz_file(archive_path, build_dir)?;
    } else if filename.ends_with(".zst") {
        extract_zst_file(archive_path, build_dir)?;
    } else {
        bail!("Unsupported archive format: {}", filename);
    }

    if !extract_path.exists() {
        bail!(
            "Expected extraction directory not found: {}",
            extract_path.display()
        );
    }

    println!("Extracted to: {}", extract_path.display());
    Ok(extract_path)
}

fn extract_tar_gz(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest)?;
    Ok(())
}

fn extract_tar_xz(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = xz2::read::XzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest)?;
    Ok(())
}

fn extract_tar_bz2(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = bzip2::read::BzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest)?;
    Ok(())
}

fn extract_tar(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let mut archive = tar::Archive::new(file);
    archive.unpack(dest)?;
    Ok(())
}

fn extract_zip(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    archive.extract(dest)?;
    Ok(())
}

fn extract_tar_zst(path: &Path, dest: &Path) -> Result<()> {
    let file = File::open(path)?;
    let decoder = ZstdDecoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest)?;
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
    std::io::copy(&mut decoder, &mut out)?;
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
    std::io::copy(&mut decoder, &mut out)?;
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
    std::io::copy(&mut decoder, &mut out)?;
    Ok(())
}

fn extract_deb(path: &Path, dest: &Path) -> Result<()> {
    // Debian packages are ar archives containing (among others) a data.tar.* member
    let file = File::open(path)?;
    let mut ar = ar::Archive::new(file);

    // Iterate members and look for data.tar, data.tar.gz, data.tar.xz, data.tar.zst
    while let Some(entry_result) = ar.next_entry() {
        let entry = entry_result?;
        let id = String::from_utf8_lossy(entry.header().identifier()).to_string();
        let lower = id.to_ascii_lowercase();
        if lower.starts_with("data.tar") {
            // Determine compression
            if lower.ends_with(".gz") {
                let decoder = GzDecoder::new(entry);
                let mut archive = tar::Archive::new(decoder);
                archive.unpack(dest)?;
                return Ok(());
            } else if lower.ends_with(".xz") {
                let decoder = xz2::read::XzDecoder::new(entry);
                let mut archive = tar::Archive::new(decoder);
                archive.unpack(dest)?;
                return Ok(());
            } else if lower.ends_with(".zst") {
                let decoder = ZstdDecoder::new(entry)?;
                let mut archive = tar::Archive::new(decoder);
                archive.unpack(dest)?;
                return Ok(());
            } else {
                // plain tar
                let mut archive = tar::Archive::new(entry);
                archive.unpack(dest)?;
                return Ok(());
            }
        }
    }

    // No data member found
    anyhow::bail!("No data.tar.* member found in deb: {}", path.display());
}

fn extract_cpio_newc_from_reader<R: Read>(mut r: R, dest: &Path) -> Result<()> {
    use std::str;
    loop {
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
    let data = std::fs::read(path)?;

    // search for known signatures
    let gz_sig = b"\x1f\x8b";
    let xz_sig = b"\xfd7zXZ\x00";
    let zst_sig = b"\x28\xb5\x2f\xfd";
    let cpio_sig = b"070701";

    if let Some(pos) = find_subslice(&data, gz_sig) {
        let cursor = Cursor::new(&data[pos..]);
        let decoder = GzDecoder::new(cursor);
        extract_cpio_newc_from_reader(decoder, dest)?;
        return Ok(());
    }
    if let Some(pos) = find_subslice(&data, xz_sig) {
        let cursor = Cursor::new(&data[pos..]);
        let decoder = xz2::read::XzDecoder::new(cursor);
        extract_cpio_newc_from_reader(decoder, dest)?;
        return Ok(());
    }
    if let Some(pos) = find_subslice(&data, zst_sig) {
        let cursor = Cursor::new(&data[pos..]);
        let decoder = ZstdDecoder::new(cursor)?;
        extract_cpio_newc_from_reader(decoder, dest)?;
        return Ok(());
    }
    if let Some(pos) = find_subslice(&data, cpio_sig) {
        let cursor = Cursor::new(&data[pos..]);
        extract_cpio_newc_from_reader(cursor, dest)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

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
