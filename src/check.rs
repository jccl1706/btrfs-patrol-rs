// SPDX-License-Identifier: GPL-3.0-or-later
//! `check`: compare the configuration with what is mounted, and look for
//! broken snapshots.

use std::path::Path;

use crate::btrfs;
use crate::config::{Config, ManagedSubvolume};
use crate::error::Result;
use crate::mounts::{Mount, containing_mount, find_mount};
use crate::snapshot::{SUBVOLUME_NAME, Store};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Wrong: `check` fails.
    pub problems: Vec<String>,
    /// Worth knowing, but not wrong.
    pub warnings: Vec<String>,
}

/// How to tell whether a subvolume is read-only; btrfs in real use, a stand-in in tests.
pub type ReadOnly<'a> = &'a dyn Fn(&Path) -> Result<bool>;

/// The ID of the snapshot a mount comes from, whatever put it there: a
/// rollback waiting for its reboot, or a snapshot's own boot entry.
pub fn mounted_snapshot(config: &Config, mount: Option<&Mount>) -> Option<u64> {
    let mount = mount.filter(|m| m.fstype == "btrfs")?;
    let prefix = format!("/{}/", config.snapshots_subvolume.trim_matches('/'));
    let middle = mount.root.strip_prefix(&prefix)?.strip_suffix(&format!("/{SUBVOLUME_NAME}"))?;
    if middle.is_empty() || !middle.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    middle.parse().ok()
}

/// The ID of the snapshot a mount still is while a rollback waits for a
/// reboot. A rollback moves the LIVE, writable subvolume into the store; a
/// boot entry mounts a read-only snapshot - that is how the two are told apart.
pub fn pending_rollback(config: &Config, mount: Option<&Mount>, read_only: ReadOnly) -> Option<u64> {
    let id = mounted_snapshot(config, mount)?;
    let path = config.snapshots_dir.join(id.to_string()).join(SUBVOLUME_NAME);
    match read_only(&path) {
        Ok(true) => None,
        _ => Some(id), // can't tell: keep the cautious answer, "pending"
    }
}

pub fn pending_rollback_message(id: u64, path: &Path) -> String {
    let what = if path == Path::new("/") {
        "/ is still the previous system".to_owned()
    } else {
        format!("{} is still the previous state", path.display())
    };
    format!("a rollback is waiting for a reboot: {what}, kept as snapshot {id}")
}

/// The options of /etc/fstab's line for `mount_point`.
fn fstab_options<'a>(fstab: &'a str, mount_point: &str) -> Option<&'a str> {
    fstab
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .rfind(|f| f.len() >= 4 && f[1] == mount_point)
        .map(|f| f[3])
}

pub fn check(
    config: &Config,
    store: &Store,
    mounts: &[Mount],
    fstab: Option<&str>,
    read_only: ReadOnly,
) -> Report {
    let mut report = Report::default();
    let root = find_mount(Path::new("/"), mounts);
    let booted = mounted_snapshot(config, root);

    match root {
        None => report.problems.push("/ is not a btrfs filesystem".into()),
        Some(r) if r.fstype != "btrfs" => report.problems.push("/ is not a btrfs filesystem".into()),
        Some(_) if booted.is_some() => {
            let id = booted.expect("checked");
            match read_only(&store.subvolume(id)) {
                Err(_) => report.warnings.push(format!(
                    "/ is mounted from snapshot {id}; run as root to tell whether a rollback is \
                     waiting for a reboot or this is the snapshot's own boot entry"
                )),
                Ok(true) => report.warnings.push(format!(
                    "booted from snapshot {id}, read-only: the system's own root subvolume is not \
                     running. 'btrfs-patrol rollback <ID>' restores it, then reboot"
                )),
                Ok(false) => report.warnings.push(format!(
                    "{}; reboot to start the restored system",
                    pending_rollback_message(id, Path::new("/"))
                )),
            }
        }
        Some(r) if r.root != format!("/{}", config.root_subvolume) => report.problems.push(format!(
            "/ is subvolume {:?}, but filesystem.root_subvolume is {:?}",
            r.root, config.root_subvolume
        )),
        Some(_) => {}
    }

    // Arch: genfstab writes subvolid= next to subvol=. After a rollback the
    // name points at the restored subvolume but the ID still points at the
    // old one, and mount obeys the ID: the system would come back up on the
    // root it was rolled back FROM.
    if let Some(options) = fstab.and_then(|f| fstab_options(f, "/"))
        && options.split(',').any(|o| o.starts_with("subvolid="))
    {
        report.warnings.push(
            "/etc/fstab mounts / with subvolid=: after a rollback that ID is the old root, so the \
                 rolled-back system would never start; remove subvolid= and keep subvol="
                .into(),
        );
    }

    let dir = &config.snapshots_dir;
    match find_mount(dir, mounts) {
        None => report.problems.push(format!(
            "{} is not a mount point; run 'btrfs-patrol setup' to create and mount the top-level \
             {:?} subvolume there",
            dir.display(),
            config.snapshots_subvolume
        )),
        Some(m) if m.root != format!("/{}", config.snapshots_subvolume) => report.problems.push(format!(
            "{} is subvolume {:?}, but filesystem.snapshots_subvolume is {:?}",
            dir.display(),
            m.root,
            config.snapshots_subvolume
        )),
        Some(m) if root.is_some_and(|r| r.source != m.source) => {
            report.problems.push(format!("{} is not on the same filesystem as /", dir.display()))
        }
        Some(_) => check_store(config, store, &mut report),
    }

    if let Some(root) = root.filter(|r| r.fstype == "btrfs") {
        for subvolume in &config.subvolumes {
            check_subvolume(config, subvolume, root, mounts, read_only, &mut report);
        }
    }
    report
}

fn check_store(config: &Config, store: &Store, report: &mut Report) {
    let ids = match store.ids() {
        Ok(ids) => ids,
        Err(e) => return report.problems.push(e.0),
    };
    let managed: Vec<String> = config.managed().into_iter().map(|s| s.name).collect();
    let mut unmanaged: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for id in ids {
        let snapshot = match store.load(id) {
            Ok(s) => s,
            Err(e) => {
                report.problems.push(e.0);
                continue;
            }
        };
        if !store.subvolume(id).is_dir() {
            report.problems.push(format!("snapshot {id} has metadata but no subvolume"));
        }
        if let Some(boot) = &snapshot.boot {
            for hash in [&boot.kernel, &boot.initramfs] {
                if !store.boot_file(hash).is_file() {
                    report.problems.push(format!("snapshot {id}'s stored boot file {hash} is missing"));
                }
            }
        }
        if !managed.contains(&snapshot.subvolume) {
            unmanaged.entry(snapshot.subvolume).or_default().push(id.to_string());
        }
    }
    for (name, ids) in unmanaged {
        report.warnings.push(format!(
            "snapshot(s) {} are of the subvolume {name:?}, which isn't in the configuration: they are \
             never pruned and can't be rolled back",
            ids.join(", ")
        ));
    }
}

fn check_subvolume(
    config: &Config,
    subvolume: &ManagedSubvolume,
    root: &Mount,
    mounts: &[Mount],
    read_only: ReadOnly,
    report: &mut Report,
) {
    let path = subvolume.path.as_path();
    let label = format!("{} (subvolume {:?})", path.display(), subvolume.name);
    let location = if let Some(own) = find_mount(path, mounts) {
        if own.fstype != "btrfs" || own.source != root.source {
            return report.problems.push(format!("{label} is not on the btrfs filesystem mounted at /"));
        }
        if let Some(id) = pending_rollback(config, Some(own), read_only) {
            return report.warnings.push(format!(
                "{}; reboot to use the restored {}",
                pending_rollback_message(id, path),
                path.display()
            ));
        }
        own.root.trim_matches('/').to_owned()
    } else {
        if !path.is_dir() {
            return report.problems.push(format!("{label} doesn't exist"));
        }
        let outer = containing_mount(path, mounts);
        let Some(outer) = outer.filter(|o| o.fstype == "btrfs" && o.source == root.source) else {
            return report.problems.push(format!("{label} is not on the btrfs filesystem mounted at /"));
        };
        if !btrfs::is_subvolume(path) {
            return report.problems.push(format!("{label} is a directory, not a btrfs subvolume"));
        }
        let inside = path.strip_prefix(&outer.mount_point).unwrap_or(path).to_string_lossy();
        format!("{}/{inside}", outer.root.trim_matches('/')).trim_matches('/').to_owned()
    };
    if location.starts_with(&format!("{}/", config.root_subvolume.trim_matches('/'))) {
        report.warnings.push(format!(
            "{} is a subvolume inside the root subvolume, so snapshots and rollbacks of root don't include it",
            path.display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::{parse_mountinfo, tests::SAMPLE};
    use crate::snapshot::tests::snapshot;

    const FSTAB: &str = "\
# /dev/mapper/cryptroot
UUID=91e8 / btrfs rw,noatime,compress=zstd:1,subvol=/@ 0 0
UUID=91e8 /.snapshots btrfs rw,noatime,subvol=/@snapshots 0 0
";

    fn writable(_: &Path) -> Result<bool> {
        Ok(false)
    }

    fn run(dir: &Path, mountinfo: &str, fstab: &str) -> Report {
        let config = Config { snapshots_dir: dir.to_path_buf(), ..Config::default() };
        let mounts: Vec<Mount> =
            parse_mountinfo(&mountinfo.replace(" /.snapshots ", &format!(" {} ", dir.display())));
        check(&config, &Store::new(dir), &mounts, Some(fstab), &writable)
    }

    #[test]
    fn this_laptop_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(dir.path(), SAMPLE, FSTAB), Report::default());
    }

    #[test]
    fn broken_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        std::fs::create_dir(store.path(1)).unwrap();
        std::fs::create_dir(store.path(2)).unwrap();
        let mut home = snapshot(2);
        home.subvolume = "home".into();
        std::fs::write(store.path(2).join("info.json"), home.to_json().to_string()).unwrap();
        let report = run(dir.path(), SAMPLE, FSTAB);
        assert_eq!(
            report.problems,
            ["snapshot 1 has no info.json", "snapshot 2 has metadata but no subvolume"]
        );
        assert!(report.warnings[0].contains("\"home\", which isn't in the configuration"));
    }

    #[test]
    fn subvolid_in_fstab() {
        let dir = tempfile::tempdir().unwrap();
        let fstab = FSTAB.replace("subvol=/@ ", "subvolid=256,subvol=/@ ");
        let report = run(dir.path(), SAMPLE, &fstab);
        assert!(report.warnings[0].contains("subvolid="), "{report:?}");
    }

    #[test]
    fn wrong_or_missing_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let report = run(dir.path(), &SAMPLE.replace("/@ / ", "/root / "), FSTAB);
        assert!(report.problems[0].contains("filesystem.root_subvolume is \"@\""));
        let report = run(dir.path(), &SAMPLE.replace(" /.snapshots ", " /elsewhere "), FSTAB);
        assert!(report.problems[0].contains("is not a mount point"));
    }

    #[test]
    fn rollback_waiting_for_reboot() {
        let dir = tempfile::tempdir().unwrap();
        let report = run(dir.path(), &SAMPLE.replace("/@ / ", "/@snapshots/7/snapshot / "), FSTAB);
        assert!(report.problems.is_empty());
        assert!(report.warnings[0].contains("waiting for a reboot"), "{report:?}");
        assert!(report.warnings[0].contains("snapshot 7"));
    }

    #[test]
    fn snapshot_ids_from_mounts() {
        let config = Config::default();
        let mount = |root: &str| Mount {
            root: root.into(),
            mount_point: "/".into(),
            fstype: "btrfs".into(),
            source: "x".into(),
        };
        assert_eq!(mounted_snapshot(&config, Some(&mount("/@snapshots/12/snapshot"))), Some(12));
        assert_eq!(mounted_snapshot(&config, Some(&mount("/@"))), None);
        assert_eq!(mounted_snapshot(&config, Some(&mount("/@snapshots/x/snapshot"))), None);
        let read_only = |_: &Path| Ok(true);
        assert_eq!(pending_rollback(&config, Some(&mount("/@snapshots/12/snapshot")), &read_only), None);
    }
}
