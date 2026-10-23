// SPDX-License-Identifier: GPL-3.0-or-later
//! A thin wrapper around btrfs(8). Commands are always argument lists, never
//! a shell, and a failure carries btrfs's own message.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use crate::error::Result;
use crate::{bail, err};

pub const BTRFS: &str = "btrfs";
/// The top directory of every btrfs subvolume has this inode number.
pub const SUBVOLUME_ROOT_INODE: u64 = 256;

pub fn run<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(BTRFS).args(args).output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => err!("{BTRFS} not found; install btrfs-progs"),
        _ => err!("{BTRFS}: {e}"),
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            bail!("{BTRFS}: {}", output.status);
        }
        bail!("{detail}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `path`, on a btrfs filesystem, is the top directory of a subvolume.
pub fn is_subvolume(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.ino() == SUBVOLUME_ROOT_INODE)
}

/// Whether a subvolume's read-only property is set. Not the same as the
/// mount being read-only: a read-only snapshot mounted at / still reports
/// 'rw' in the mount options, and btrfs refuses the writes anyway.
pub fn is_read_only(path: &Path) -> Result<bool> {
    let output = run([
        OsStr::new("property"),
        "get".as_ref(),
        "-t".as_ref(),
        "subvol".as_ref(),
        path.as_os_str(),
        "ro".as_ref(),
    ])?;
    Ok(parse_ro(&output))
}

fn parse_ro(output: &str) -> bool {
    output.trim().rsplit('=').next().is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_property() {
        assert!(parse_ro("ro=true\n"));
        assert!(!parse_ro("ro=false\n"));
        assert!(!parse_ro(""));
    }
}
