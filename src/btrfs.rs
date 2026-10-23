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

pub fn create_snapshot(source: &Path, destination: &Path, readonly: bool) -> Result<()> {
    let mut args = vec![OsStr::new("subvolume"), OsStr::new("snapshot")];
    if readonly {
        args.push(OsStr::new("-r"));
    }
    args.extend([source.as_os_str(), destination.as_os_str()]);
    run(args).map(drop)
}

pub fn delete_subvolume(path: &Path) -> Result<()> {
    run([OsStr::new("subvolume"), OsStr::new("delete"), path.as_os_str()]).map(drop)
}

/// A subvolume as `btrfs subvolume list` gives it, listed from the top
/// level: its path is relative to the top level ("@", "@/var/lib/portables").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subvolume {
    pub id: u64,
    pub path: String,
}

/// Parse lines such as "ID 256 gen 9123 top level 5 path @".
pub fn parse_subvolume_list(output: &str) -> Result<Vec<Subvolume>> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let (head, path) = line
                .split_once(" path ")
                .ok_or_else(|| err!("unexpected output from btrfs subvolume list: {line:?}"))?;
            let mut fields = head.split_whitespace();
            match (fields.next(), fields.next().and_then(|id| id.parse().ok())) {
                (Some("ID"), Some(id)) => Ok(Subvolume { id, path: path.to_owned() }),
                _ => bail!("unexpected output from btrfs subvolume list: {line:?}"),
            }
        })
        .collect()
}

pub fn list_subvolumes(mount: &Path) -> Result<Vec<Subvolume>> {
    parse_subvolume_list(&run([OsStr::new("subvolume"), OsStr::new("list"), mount.as_os_str()])?)
}

/// The ID of the subvolume that contains `path`.
pub fn subvolume_id(path: &Path) -> Result<u64> {
    let output = run([OsStr::new("inspect-internal"), OsStr::new("rootid"), path.as_os_str()])?;
    output
        .trim()
        .parse()
        .map_err(|_| err!("unexpected output from btrfs inspect-internal rootid: {output:?}"))
}

/// "ID 256 gen 9123 top level 5 path @", or "ID 5 (FS_TREE)" for the top level.
pub fn parse_default(output: &str) -> Result<u64> {
    let mut fields = output.split_whitespace();
    match (fields.next(), fields.next().and_then(|id| id.parse().ok())) {
        (Some("ID"), Some(id)) => Ok(id),
        _ => bail!("unexpected output from btrfs subvolume get-default: {output:?}"),
    }
}

pub fn get_default_subvolume_id(mount: &Path) -> Result<u64> {
    parse_default(&run([OsStr::new("subvolume"), OsStr::new("get-default"), mount.as_os_str()])?)
}

pub fn set_default_subvolume(id: u64, mount: &Path) -> Result<()> {
    run([OsStr::new("subvolume"), OsStr::new("set-default"), OsStr::new(&id.to_string()), mount.as_os_str()])
        .map(drop)
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
    fn subvolume_lists() {
        let list = parse_subvolume_list(
            "ID 256 gen 120 top level 5 path @\nID 257 gen 99 top level 5 path @home\n\
             ID 270 gen 5 top level 256 path @/var/lib/portables\nID 300 gen 7 top level 258 path @snapshots/1/snapshot with space\n",
        )
        .unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list[2], Subvolume { id: 270, path: "@/var/lib/portables".into() });
        assert_eq!(list[3].path, "@snapshots/1/snapshot with space");
        assert!(parse_subvolume_list("garbage").is_err());
        assert_eq!(parse_default("ID 5 (FS_TREE)\n").unwrap(), 5);
        assert_eq!(parse_default("ID 256 gen 9 top level 5 path @").unwrap(), 256);
        assert!(parse_default("").is_err());
    }

    #[test]
    fn read_only_property() {
        assert!(parse_ro("ro=true\n"));
        assert!(!parse_ro("ro=false\n"));
        assert!(!parse_ro(""));
    }
}
