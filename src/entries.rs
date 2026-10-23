// SPDX-License-Identifier: GPL-3.0-or-later
//! systemd-boot entries for snapshots, so a system too broken to log in to
//! can boot one of them, read-only, and be rolled back from there.
//!
//! systemd-boot only reads the ESP, and a snapshot's kernel is stored in
//! btrfs - so the entry's kernel and initramfs are copied to
//! `<ESP>/btrfs-patrol/<sha256>`, shared by every entry that uses them and
//! removed when none does. Entries are `btrfs-patrol-<id>.conf`, and only
//! files of those two names are ever written or removed: the rest of the
//! ESP belongs to the boot loader and to mkinitcpio.

use std::collections::BTreeSet;
use std::path::Path;

use crate::App;
use crate::error::Result;
use crate::snapshot::{ROOT, SUBVOLUME_NAME, Snapshot};
use crate::{err, system};

/// systemd-boot orders entries without a sort-key by file name, newest
/// version first, with systemd's version comparison: digits rank above
/// letters, and `a` is about as low as a valid name can start (`~` sorts
/// lower still, but bootctl and `systemctl reboot --boot-loader-entry`
/// reject it). `arch-btrfs-patrol-28` sorts below `arch.conf` and
/// `arch-fallback.conf` - `b` before `f` - and below kernel-install's
/// machine-id entries, newest snapshot first among ours (28 > 9).
pub const ENTRY_PREFIX: &str = "arch-btrfs-patrol-";
/// Names entries had before, still recognised for cleaning up.
const OLD_PREFIXES: [&str; 3] = ["btrfs-patrol-", "00-btrfs-patrol-", "~btrfs-patrol-"];
/// Where entries' kernels and initramfs images go on the ESP.
pub const ESP_DIR: &str = "btrfs-patrol";

/// The kernel command line that boots a snapshot, from the running one.
///
/// Derived rather than built, so everything that reaches the disk - the
/// rd.luks options, root=UUID - is carried over untouched. What changes:
/// rootflags' subvol=/subvolid= become the snapshot's path (the rest, such
/// as compress=, stays); `ro`, never `rw`; resume= goes (resuming the
/// normal system's hibernation on a snapshot would restore memory that does
/// not match the disk); initrd= goes (the entry names its initrds itself).
pub fn snapshot_options(cmdline: &str, subvolume_path: &str) -> String {
    let mut words = Vec::new();
    let mut flags = Vec::new();
    for word in cmdline.split_whitespace() {
        let key = word.split('=').next().unwrap_or(word);
        if let Some(value) = word.strip_prefix("rootflags=") {
            flags.extend(
                value
                    .split(',')
                    .filter(|o| !o.is_empty() && !matches!(o.split('=').next(), Some("subvol" | "subvolid"))),
            );
        } else if matches!(
            key,
            "initrd" | "resume" | "resume_offset" | "systemd.volatile" | "systemd.machine_id"
        ) || word == "ro"
            || word == "rw"
        {
            continue;
        } else {
            words.push(word);
        }
    }
    let flags =
        flags.into_iter().map(str::to_owned).chain([format!("subvol={subvolume_path}")]).collect::<Vec<_>>();
    words.push("ro");
    format!("{} rootflags={}", words.join(" "), flags.join(","))
}

pub fn entry_text(title: &str, version: &str, linux: &str, initrds: &[String], options: &str) -> String {
    let mut text = format!("title      {title}\nversion    {version}\nlinux      {linux}\n");
    for initrd in initrds {
        text.push_str(&format!("initrd     {initrd}\n"));
    }
    // No sort-key: systemd-boot lists entries that have one BEFORE those
    // that don't, so one would put every snapshot above Arch's own entries.
    // Without it they sort by file name, after arch.conf.
    text.push_str(&format!("options    {options}\n"));
    text
}

/// Microcode images on the ESP, which load before any initramfs.
fn microcode(esp: &Path) -> Vec<String> {
    ["intel-ucode.img", "amd-ucode.img"]
        .into_iter()
        .filter(|n| esp.join(n).is_file())
        .map(|n| format!("/{n}"))
        .collect()
}

/// Our entries in the loader's directory, by snapshot ID.
fn ours(entries: &Path) -> Vec<(u64, std::path::PathBuf)> {
    let Ok(dir) = std::fs::read_dir(entries) else { return Vec::new() };
    dir.filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let rest =
                std::iter::once(ENTRY_PREFIX).chain(OLD_PREFIXES).find_map(|p| name.strip_prefix(p))?;
            let id = rest.strip_suffix(".conf")?;
            id.bytes().all(|b| b.is_ascii_digit()).then(|| Some((id.parse().ok()?, e.path())))?
        })
        .collect()
}

/// The snapshots that get an entry: the newest root snapshots that have a
/// stored kernel and the modules for it.
pub fn wanted<'a>(app: &App, snapshots: &'a [Snapshot]) -> Vec<&'a Snapshot> {
    let mut roots: Vec<&Snapshot> = snapshots
        .iter()
        .filter(|s| s.subvolume == ROOT)
        .filter(|s| {
            s.boot
                .as_ref()
                .is_some_and(|b| app.store.subvolume(s.id).join("usr/lib/modules").join(&b.version).is_dir())
        })
        .collect();
    roots.sort_by_key(|s| std::cmp::Reverse(s.id));
    roots.truncate(app.config.boot_entries);
    roots
}

#[derive(Debug, Default)]
pub struct Synced {
    pub written: Vec<u64>,
    pub removed: Vec<u64>,
}

/// Make the boot menu match the snapshots. Hold the store lock.
pub fn sync(app: &App, esp: &Path, cmdline: &str) -> Result<Synced> {
    let entries = esp.join("loader/entries");
    let mut synced = Synced::default();
    if !entries.is_dir() {
        if app.config.boot_entries > 0 {
            return Err(err!(
                "{} not found: is systemd-boot installed on {}?",
                entries.display(),
                esp.display()
            ));
        }
        return Ok(synced);
    }
    let all = app.store.load_all()?;
    let wanted = wanted(app, &all);
    let files = esp.join(ESP_DIR);
    let mut needed = BTreeSet::new();
    for s in &wanted {
        let boot = s.boot.as_ref().expect("wanted only has stored kernels");
        for hash in [&boot.kernel, &boot.initramfs] {
            needed.insert(hash.clone());
            let target = files.join(hash);
            if !target.is_file() {
                std::fs::create_dir_all(&files).map_err(|e| err!("{}: {e}", files.display()))?;
                let tmp = files.join(format!("{hash}.tmp"));
                std::fs::copy(app.store.boot_file(hash), &tmp)
                    .map_err(|e| err!("copy to {}: {e}", tmp.display()))?;
                std::fs::rename(&tmp, &target).map_err(|e| err!("{}: {e}", target.display()))?;
            }
        }
        let mut initrds = microcode(esp);
        initrds.push(format!("/{ESP_DIR}/{}", boot.initramfs));
        let description =
            if s.description.is_empty() { s.kind.clone() } else { crate::table::printable(&s.description) };
        let title = format!("Snapshot {} - {} - {}", s.id, s.created.format("%Y-%m-%d %H:%M"), description);
        let subvolume =
            format!("{}/{}/{SUBVOLUME_NAME}", app.config.snapshots_subvolume.trim_matches('/'), s.id);
        let text = entry_text(
            &title,
            &boot.version,
            &format!("/{ESP_DIR}/{}", boot.kernel),
            &initrds,
            &snapshot_options(cmdline, &subvolume),
        );
        let path = entries.join(format!("{ENTRY_PREFIX}{}.conf", s.id));
        if std::fs::read_to_string(&path).ok().as_deref() != Some(&text) {
            system::write_atomic(&path, text.as_bytes(), Some(0o600))?;
        }
        synced.written.push(s.id);
    }
    for (id, path) in ours(&entries) {
        let current = path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(ENTRY_PREFIX));
        if !synced.written.contains(&id) || !current {
            std::fs::remove_file(&path).map_err(|e| err!("{}: {e}", path.display()))?;
            synced.removed.push(id);
        }
    }
    if let Ok(dir) = std::fs::read_dir(&files) {
        for entry in dir.filter_map(|e| e.ok()) {
            if !needed.contains(&entry.file_name().to_string_lossy().into_owned()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(synced)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMDLINE: &str = "initrd=\\intel-ucode.img initrd=\\initramfs-linux.img rd.luks.name=fe18=cryptroot \
                           rd.luks.options=discard root=UUID=ecdb rootflags=subvol=@,compress=zstd rootfstype=btrfs rw quiet resume=UUID=x";

    #[test]
    fn options_for_a_snapshot() {
        assert_eq!(
            snapshot_options(CMDLINE, "@snapshots/24/snapshot"),
            "rd.luks.name=fe18=cryptroot rd.luks.options=discard root=UUID=ecdb rootfstype=btrfs quiet ro \
             rootflags=compress=zstd,subvol=@snapshots/24/snapshot"
        );
        assert_eq!(
            snapshot_options("root=UUID=x rw", "@snapshots/1/snapshot"),
            "root=UUID=x ro rootflags=subvol=@snapshots/1/snapshot"
        );
    }

    #[test]
    fn entry() {
        let text = entry_text(
            "Snapshot 3",
            "7.2.11-arch1-1",
            "/btrfs-patrol/k",
            &["/intel-ucode.img".into(), "/btrfs-patrol/i".into()],
            "ro",
        );
        assert_eq!(
            text,
            "title      Snapshot 3\nversion    7.2.11-arch1-1\nlinux      /btrfs-patrol/k\ninitrd     /intel-ucode.img\n\
             initrd     /btrfs-patrol/i\noptions    ro\n"
        );
    }

    #[test]
    fn only_our_entries_are_ours() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["arch.conf", "btrfs-patrol-12.conf", "btrfs-patrol-x.conf", "btrfs-patrol-3.conf.bak"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        std::fs::write(dir.path().join("arch-btrfs-patrol-7.conf"), "").unwrap();
        std::fs::write(dir.path().join("~btrfs-patrol-5.conf"), "").unwrap();
        std::fs::write(dir.path().join("arch-fallback.conf"), "").unwrap();
        let mut ids: Vec<u64> = ours(dir.path()).into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, [5, 7, 12], "the current name and the old ones");
    }
}
