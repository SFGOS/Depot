use anyhow::{Context, Result, bail};
use signal_hook::consts::SIGINT;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static INSTALL_HANDLER: Once = Once::new();
static INSTALL_ERROR: OnceLock<String> = OnceLock::new();

pub(crate) fn install() -> Result<()> {
    INSTALL_HANDLER.call_once(|| {
        if let Err(err) = unsafe {
            signal_hook::low_level::register(SIGINT, || {
                INTERRUPTED.store(true, Ordering::Relaxed);
            })
        } {
            let _ = INSTALL_ERROR.set(err.to_string());
        };
    });

    if let Some(err) = INSTALL_ERROR.get() {
        bail!("Failed to register Ctrl-C handler: {}", err);
    }

    Ok(())
}

pub(crate) fn reset() {
    INTERRUPTED.store(false, Ordering::Relaxed);
}

pub(crate) fn was_interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

pub(crate) fn check() -> Result<()> {
    install()?;
    check_with(was_interrupted)
}

pub(crate) fn command_status(cmd: &mut Command) -> io::Result<ExitStatus> {
    install().map_err(io::Error::other)?;
    configure_child_process_group(cmd);
    let mut child = cmd.spawn()?;
    wait_for_child_with(&mut child, was_interrupted)
}

pub(crate) fn copy_interruptibly<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> io::Result<u64> {
    install().map_err(io::Error::other)?;
    copy_interruptibly_with(reader, writer, was_interrupted)
}

pub(crate) fn unpack_tar_archive<R: Read>(
    archive: &mut tar::Archive<R>,
    dest: &Path,
) -> Result<()> {
    install()?;
    unpack_tar_archive_with(archive, dest, was_interrupted)
}

fn check_with(interrupted: impl Fn() -> bool) -> Result<()> {
    if interrupted() {
        bail!("Interrupted by Ctrl-C");
    }
    Ok(())
}

fn copy_interruptibly_with<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    interrupted: impl Fn() -> bool,
) -> io::Result<u64> {
    let mut copied = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        if interrupted() {
            return Err(io::Error::new(
                ErrorKind::Interrupted,
                "Interrupted by Ctrl-C",
            ));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(copied);
        }
        writer.write_all(&buf[..n])?;
        copied += n as u64;
    }
}

fn unpack_tar_archive_with<R: Read>(
    archive: &mut tar::Archive<R>,
    dest: &Path,
    interrupted: impl Fn() -> bool,
) -> Result<()> {
    fs::create_dir_all(dest)
        .with_context(|| format!("Failed to create extraction dir {}", dest.display()))?;
    for entry in archive
        .entries()
        .context("Failed to iterate tar archive entries")?
    {
        check_with(&interrupted)?;
        let mut entry = entry.context("Failed to read tar archive entry")?;
        let path = entry
            .path()
            .context("Failed to inspect tar archive entry path")?
            .into_owned();
        if !entry
            .unpack_in(dest)
            .with_context(|| format!("Failed to unpack tar archive entry {}", path.display()))?
        {
            bail!(
                "Refusing to extract unsafe tar archive entry outside destination: {}",
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(unix)]
fn configure_child_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // Put the child in its own process group so Ctrl-C can be forwarded to the
    // whole build/extract pipeline without killing the depot parent process.
    unsafe {
        cmd.pre_exec(|| {
            if nix::libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_child_process_group(_cmd: &mut Command) {}

fn wait_for_child_with(
    child: &mut Child,
    interrupted: impl Fn() -> bool,
) -> io::Result<ExitStatus> {
    let mut interrupted_at: Option<Instant> = None;
    let mut sent_term = false;
    let mut sent_kill = false;

    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }

        if interrupted() {
            if interrupted_at.is_none() {
                interrupted_at = Some(Instant::now());
                signal_child_process_group(child, nix::libc::SIGINT);
            } else if interrupted_at
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(2) && !sent_term)
            {
                sent_term = true;
                signal_child_process_group(child, nix::libc::SIGTERM);
            } else if interrupted_at
                .is_some_and(|started| started.elapsed() >= Duration::from_secs(4) && !sent_kill)
            {
                sent_kill = true;
                signal_child_process_group(child, nix::libc::SIGKILL);
            }
        }

        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn signal_child_process_group(child: &Child, signal: i32) {
    let pgid = -(child.id() as i32);
    let rc = unsafe { nix::libc::kill(pgid, signal) };
    if rc == 0 {
        return;
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(nix::libc::ESRCH) {
        crate::log_warn!(
            "Failed to forward signal {} to child process group {}: {}",
            signal,
            child.id(),
            err
        );
    }
}

#[cfg(not(unix))]
fn signal_child_process_group(_child: &Child, _signal: i32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Arc;

    #[test]
    fn copy_interruptibly_returns_interrupted_error() {
        let mut reader = Cursor::new(b"hello".to_vec());
        let mut writer = Vec::new();
        let interrupted = AtomicBool::new(true);
        let err = copy_interruptibly_with(&mut reader, &mut writer, || {
            interrupted.load(Ordering::Relaxed)
        })
        .expect_err("copy should stop once interrupted");
        assert_eq!(err.kind(), ErrorKind::Interrupted);
    }

    #[test]
    fn unpack_tar_archive_stops_when_interrupted() {
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            let mut header = tar::Header::new_gnu();
            header.set_path("hello.txt").unwrap();
            header.set_size(5);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append(&header, Cursor::new(b"hello".to_vec()))
                .unwrap();
            builder.finish().unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let mut archive = tar::Archive::new(Cursor::new(tar_data));
        let interrupted = AtomicBool::new(true);
        let err = unpack_tar_archive_with(&mut archive, temp.path(), || {
            interrupted.load(Ordering::Relaxed)
        })
        .expect_err("tar unpack should stop once interrupted");
        assert!(err.to_string().contains("Interrupted by Ctrl-C"));
    }

    #[test]
    #[cfg(unix)]
    fn command_status_interrupts_child_process() {
        use std::os::unix::process::ExitStatusExt;

        let interrupted = Arc::new(AtomicBool::new(false));
        let trigger_flag = interrupted.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            trigger_flag.store(true, Ordering::Relaxed);
        });

        let start = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 10");
        configure_child_process_group(&mut cmd);
        let mut child = cmd.spawn().expect("sleep command should spawn");
        let status = wait_for_child_with(&mut child, || interrupted.load(Ordering::Relaxed))
            .expect("sleep command should be interruptible");
        trigger.join().unwrap();

        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(status.signal(), Some(SIGINT));
    }
}
