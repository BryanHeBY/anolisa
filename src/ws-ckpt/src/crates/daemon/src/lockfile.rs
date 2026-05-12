use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::Context;
use tracing::warn;

/// Lockfile holder: holds file handle + flock lock
pub(crate) struct LockfileHolder {
    _file: std::fs::File,
}

/// Acquire lockfile and perform crash detection.
///
/// - lockfile does not exist → normal startup (first or reboot)
/// - lockfile exists and lock acquired → last crash (process died, kernel released flock, but file remained)
/// - lockfile exists and lock acquisition failed → another instance is running, reject startup
pub(crate) fn acquire(lockfile_path: &Path) -> anyhow::Result<LockfileHolder> {
    // Ensure lockfile directory exists (systemd RuntimeDirectory manages, but fallback creates)
    if let Some(parent) = lockfile_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create lockfile directory: {:?}", parent))?;
    }

    let lockfile_existed = lockfile_path.exists();

    // Open or create lockfile
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lockfile_path)
        .with_context(|| format!("Failed to open lockfile: {:?}", lockfile_path))?;

    // Attempt non-blocking lock acquisition
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::bail!(
                "Another ws-ckpt daemon instance is running (lockfile {:?} is locked)",
                lockfile_path
            );
        }
        return Err(err).with_context(|| format!("flock failed: {:?}", lockfile_path));
    }

    // Lock acquired
    if lockfile_existed {
        warn!(
            "Detected unclean shutdown (lockfile {:?} present from previous run)",
            lockfile_path
        );
    }

    // Write current PID
    let mut file = file;
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.sync_all()?;

    Ok(LockfileHolder { _file: file })
}
