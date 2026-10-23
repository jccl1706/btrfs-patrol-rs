// SPDX-License-Identifier: GPL-3.0-or-later
//! Files, locks and signals: the pieces every command that changes
//! something leans on.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Result;
use crate::{bail, err};

/// Replace `path` with `bytes`, so a crash never leaves a half-written file:
/// write a temporary file next to it, flush it to disk, then rename.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let tmp = path
        .with_file_name(format!("{}.tmp", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    let io = |e: std::io::Error| err!("{}: {e}", path.display());
    let mut file = File::create(&tmp).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    if let Some(mode) = mode {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(io)?;
    }
    std::fs::rename(&tmp, path).map_err(io)
}

/// An exclusive lock on the store, held until dropped, so concurrent runs -
/// a pacman hook and the timer, say - can't pick the same ID or prune each
/// other's snapshots.
pub struct Lock {
    _file: File,
}

pub fn lock(directory: &Path) -> Result<Lock> {
    if !directory.is_dir() {
        bail!("snapshots directory not found: {}", directory.display());
    }
    let path = directory.join(".lock");
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| err!("{}: {e}", path.display()))?;
    let fd = file.as_raw_fd();
    // SAFETY: flock on a file descriptor we own; the lock goes when the file closes.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if std::io::stderr().is_terminal() {
            eprintln!(":: waiting for another btrfs-patrol to finish...");
        }
        // SAFETY: as above; this one blocks until the other run lets go.
        if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
            bail!("{}: {}", path.display(), std::io::Error::last_os_error());
        }
    }
    Ok(Lock { _file: file })
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn note_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// From here on, Ctrl+C and SIGTERM don't kill btrfs-patrol on the spot:
/// they are noted, the child btrfs (which gets Ctrl+C itself) fails, and the
/// command cleans up after itself on the error path - instead of leaving
/// metadata with no subvolume, which a later prune would count as a real
/// snapshot and evict a real one for.
pub fn defer_interrupts() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, note_interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, note_interrupt as *const () as libc::sighandler_t);
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// The running kernel's release, as uname -r says it.
pub fn kernel_release() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_owned()).unwrap_or_default()
}

/// Ask a yes/no question on the terminal; only "y" or "yes" is yes.
pub fn confirm(question: &str, assume_yes: bool) -> Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    require_confirmation_possible(false)?;
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| err!("{e}"))?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Fail early, before showing anything, when a prompt couldn't be answered.
pub fn require_confirmation_possible(assume_yes: bool) -> Result<()> {
    if !assume_yes && !std::io::stdin().is_terminal() {
        bail!("confirmation needed but input is not a terminal; pass --yes");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("info.json");
        write_atomic(&path, b"one", None).unwrap();
        write_atomic(&path, b"two", Some(0o600)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn locks_are_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let held = lock(dir.path()).unwrap();
        let other = File::open(dir.path().join(".lock")).unwrap();
        // SAFETY: a test-local descriptor.
        assert_ne!(unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        drop(held);
        assert_eq!(unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert!(lock(&dir.path().join("missing")).is_err());
    }
}
