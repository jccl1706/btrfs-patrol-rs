// SPDX-License-Identifier: GPL-3.0-or-later
//! What is mounted where, from /proc/self/mountinfo.

use std::path::Path;

use crate::err;
use crate::error::Result;

pub const MOUNTINFO: &str = "/proc/self/mountinfo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// The directory of the filesystem that is mounted; for btrfs, the
    /// subvolume, such as "/@".
    pub root: String,
    pub mount_point: String,
    pub fstype: String,
    pub source: String,
}

/// Parse /proc/<pid>/mountinfo; see proc_pid_mountinfo(5).
pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Optional fields vary in number and end with a lone "-".
            let separator = fields.iter().position(|f| *f == "-")?;
            Some(Mount {
                root: unescape(fields.get(3)?),
                mount_point: unescape(fields.get(4)?),
                fstype: (*fields.get(separator + 1)?).to_owned(),
                source: unescape(fields.get(separator + 2)?),
            })
        })
        .collect()
}

/// Undo mountinfo's octal escapes, such as \040 for a space.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..=i + 3].iter().all(|b| (b'0'..=b'7').contains(b))
        {
            let code = bytes[i + 1..=i + 3].iter().fold(0u32, |n, b| n * 8 + u32::from(b - b'0'));
            out.push(code as u8);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn read_mounts() -> Result<Vec<Mount>> {
    let text = std::fs::read_to_string(MOUNTINFO).map_err(|e| err!("{MOUNTINFO}: {e}"))?;
    Ok(parse_mountinfo(&text))
}

/// The mount visible at `mount_point`: the last one listed there, since later
/// mounts stack on top.
pub fn find_mount<'a>(mount_point: &Path, mounts: &'a [Mount]) -> Option<&'a Mount> {
    mounts.iter().rev().find(|m| Path::new(&m.mount_point) == mount_point)
}

/// The mount `path` is on: the deepest mount point that is `path` or one of
/// its parents. Among mounts at the same point, the last one listed wins.
pub fn containing_mount<'a>(path: &Path, mounts: &'a [Mount]) -> Option<&'a Mount> {
    let mut best: Option<&Mount> = None;
    for mount in mounts {
        let point = Path::new(&mount.mount_point);
        if path.starts_with(point)
            && best
                .is_none_or(|b| point.components().count() >= Path::new(&b.mount_point).components().count())
        {
            best = Some(mount);
        }
    }
    best
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// This laptop's own mountinfo, trimmed.
    pub const SAMPLE: &str = "\
26 1 0:24 /@ / rw,noatime shared:1 - btrfs /dev/mapper/cryptroot rw,compress=zstd:1,ssd,space_cache=v2,subvolid=256,subvol=/@
45 26 0:24 /@home /home rw,noatime shared:5 - btrfs /dev/mapper/cryptroot rw,subvolid=257,subvol=/@home
46 26 0:24 /@snapshots /.snapshots rw,noatime shared:7 - btrfs /dev/mapper/cryptroot rw,subvolid=258,subvol=/@snapshots
47 26 0:24 /@var_log /var/log rw,noatime shared:9 - btrfs /dev/mapper/cryptroot rw,subvolid=259,subvol=/@var_log
48 26 259:1 / /boot rw,relatime shared:11 - vfat /dev/nvme0n1p1 rw,fmask=0077,dmask=0077
49 26 0:40 / /my\\040dir rw - tmpfs tmpfs rw
";

    #[test]
    fn parses_and_unescapes() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(mounts.len(), 6);
        assert_eq!(mounts[0].root, "/@");
        assert_eq!(mounts[0].fstype, "btrfs");
        assert_eq!(mounts[0].source, "/dev/mapper/cryptroot");
        assert_eq!(mounts[5].mount_point, "/my dir");
    }

    #[test]
    fn finding_mounts() {
        let mounts = parse_mountinfo(SAMPLE);
        assert_eq!(find_mount(Path::new("/.snapshots"), &mounts).unwrap().root, "/@snapshots");
        assert!(find_mount(Path::new("/srv"), &mounts).is_none());
        assert_eq!(containing_mount(Path::new("/var/log/journal"), &mounts).unwrap().root, "/@var_log");
        assert_eq!(containing_mount(Path::new("/var/lib"), &mounts).unwrap().root, "/@");
    }
}
