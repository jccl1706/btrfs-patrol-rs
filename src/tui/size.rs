// SPDX-License-Identifier: GPL-3.0-or-later
//! How much space snapshots take, worked out off the UI thread.
//!
//! A snapshot's EXCLUSIVE size is what deleting it would give back: the data
//! no other subvolume shares. `btrfs filesystem du` reads it, but by walking
//! every file in the snapshot - seconds for a root filesystem - and it needs
//! root. So one worker thread measures them in turn, newest first, and the
//! screen fills them in as they arrive.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use crate::btrfs;
use crate::err;
use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Size {
    Measuring,
    Bytes(u64),
    Failed(String),
}

pub struct Sizer {
    requests: Sender<(u64, PathBuf)>,
    results: Receiver<(u64, Result<u64>)>,
}

impl Sizer {
    pub fn start() -> Sizer {
        let (requests, jobs) = channel::<(u64, PathBuf)>();
        let (done, results) = channel();
        std::thread::spawn(move || {
            for (id, path) in jobs {
                if done.send((id, exclusive(&path))).is_err() {
                    break; // the screen is gone
                }
            }
        });
        Sizer { requests, results }
    }

    pub fn measure(&self, id: u64, path: PathBuf) {
        let _ = self.requests.send((id, path));
    }

    /// Results that have arrived, without waiting.
    pub fn ready(&self) -> Vec<(u64, Result<u64>)> {
        let mut out = Vec::new();
        loop {
            match self.results.try_recv() {
                Ok(result) => out.push(result),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }
}

fn exclusive(path: &Path) -> Result<u64> {
    let output = btrfs::run([
        "filesystem".as_ref(),
        "du".as_ref(),
        "-s".as_ref(),
        "--raw".as_ref(),
        path.as_os_str(),
    ])?;
    parse_du(&output).ok_or_else(|| err!("unexpected output from btrfs filesystem du"))
}

/// The Exclusive column of `btrfs filesystem du -s --raw`:
///
/// ```text
///      Total   Exclusive  Set shared  Filename
/// 9663676416   327576371  9336100045  /.snapshots/6/snapshot
/// ```
pub fn parse_du(output: &str) -> Option<u64> {
    output
        .lines()
        .filter(|l| !l.trim_start().starts_with("Total"))
        .find_map(|l| l.split_whitespace().nth(1)?.parse().ok())
}

/// Used and total bytes of the filesystem `path` is on. btrfs's own
/// accounting is subtler, but statfs is what `df` shows, and needs no root.
pub fn disk_usage(path: &Path) -> Option<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs only writes into the struct it is given, which is plain data.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * block;
    let free = stat.f_bfree as u64 * block;
    Some((total.saturating_sub(free), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn du_output() {
        let out = "     Total   Exclusive  Set shared  Filename\n9663676416   327576371  9336100045  /.snapshots/6/snapshot\n";
        assert_eq!(parse_du(out), Some(327_576_371));
        assert_eq!(parse_du("     Total   Exclusive  Set shared  Filename\n"), None);
    }

    #[test]
    fn usage_of_a_real_filesystem() {
        let (used, total) = disk_usage(Path::new("/")).unwrap();
        assert!(total > 0 && used <= total);
        assert!(disk_usage(Path::new("/nonexistent/x")).is_none());
    }
}
