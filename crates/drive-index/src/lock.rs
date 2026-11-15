//! One process at a time controls drives (TUI, `watch`, device scans): an advisory
//! `flock` on `controller.lock` next to the catalog. Other instances run as viewers.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub struct ControllerLock {
    _file: File,
}

fn lock_path(db: &Path) -> PathBuf {
    db.parent().unwrap_or(Path::new(".")).join("controller.lock")
}

/// Take the controller lock, or report who holds it.
pub fn try_lock(db: &Path) -> Result<std::result::Result<ControllerLock, String>> {
    let path = lock_path(db);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let mut holder = String::new();
        let _ = file.read_to_string(&mut holder);
        let holder = holder.trim();
        return Ok(Err(if holder.is_empty() { "another drive-index process".into() } else { holder.to_string() }));
    }
    file.set_len(0)?;
    file.rewind()?;
    write!(file, "drive-index pid {} on {}", std::process::id(), di_walk::host::hostname())?;
    file.flush()?;
    Ok(Ok(ControllerLock { _file: file }))
}

/// Take the lock or fail with a clear message.
pub fn lock_or_fail(db: &Path) -> Result<ControllerLock> {
    match try_lock(db)? {
        Ok(l) => Ok(l),
        Err(holder) => bail!("{holder} is already controlling drives for this catalog; stop it first"),
    }
}
