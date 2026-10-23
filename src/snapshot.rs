// SPDX-License-Identifier: GPL-3.0-or-later
//! Snapshot metadata and the on-disk snapshot store.
//!
//! The same layout as btrfs-patrol's, so either tool reads the other's store:
//!
//! ```text
//! <id>/info.json   metadata
//! <id>/snapshot    read-only btrfs snapshot of a managed subvolume
//! .next-id         the ID the next snapshot gets, so IDs are never reused
//! .lock            held while the store is being changed
//! .boot/<sha256>   kernels and initramfs images, stored once however many
//!                  snapshots need them (new here: on Arch they live on the
//!                  ESP, outside every snapshot)
//! ```
//!
//! info.json gains one optional field, `boot`, naming a snapshot's stored
//! kernel and initramfs. btrfs-patrol ignores fields it doesn't know.

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone, Timelike};
use serde_json::{Map, Value, json};

use crate::error::Result;
use crate::{bail, btrfs, err, system};

/// The name the root subvolume goes by in metadata, listings and selectors.
pub const ROOT: &str = "root";
pub const INFO_FILE: &str = "info.json";
pub const SUBVOLUME_NAME: &str = "snapshot";
pub const BOOT_DIR: &str = ".boot";
pub const COUNTER_FILE: &str = ".next-id";
/// Metadata of a root snapshot, which names no subvolume.
pub const FORMAT_VERSION: u64 = 1;
/// Metadata of any other subvolume's snapshot, which names the subvolume.
/// Older btrfs-patrol refuses anything but 1, so it can never mistake a
/// snapshot of /home for one of / - root snapshots keep format 1.
pub const FORMAT_WITH_SUBVOLUME: u64 = 2;

/// The kernel and initramfs a snapshot boots with, by content hash, in the
/// store's `.boot` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootFiles {
    /// The kernel package they belong to, which names them on the ESP:
    /// vmlinuz-<pkgbase>, initramfs-<pkgbase>.img.
    pub pkgbase: String,
    /// The release in the kernel image, which the snapshot has modules for.
    pub version: String,
    pub kernel: String,
    pub initramfs: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: u64,
    pub created: DateTime<FixedOffset>,
    /// The running kernel's release when it was taken (uname -r).
    pub kernel: String,
    /// How it was taken: manual, timer, pacman-pre, pacman-post or rollback
    /// (and dnf-pre/dnf-post in a store that came from Fedora).
    pub kind: String,
    pub description: String,
    /// Kept snapshots are never pruned and don't count toward max_snapshots.
    pub keep: bool,
    /// The managed subvolume it is a snapshot of.
    pub subvolume: String,
    pub boot: Option<BootFiles>,
}

impl Snapshot {
    pub fn to_json(&self) -> Value {
        let mut data = Map::new();
        let format = if self.subvolume == ROOT { FORMAT_VERSION } else { FORMAT_WITH_SUBVOLUME };
        data.insert("format".into(), json!(format));
        data.insert("created".into(), json!(self.created.to_rfc3339()));
        data.insert("kernel".into(), json!(self.kernel));
        data.insert("kind".into(), json!(self.kind));
        data.insert("description".into(), json!(self.description));
        data.insert("keep".into(), json!(self.keep));
        if self.subvolume != ROOT {
            data.insert("subvolume".into(), json!(self.subvolume));
        }
        if let Some(boot) = &self.boot {
            data.insert(
                "boot".into(),
                json!({"pkgbase": boot.pkgbase, "version": boot.version,
                       "kernel": boot.kernel, "initramfs": boot.initramfs}),
            );
        }
        Value::Object(data)
    }

    pub fn from_json(id: u64, data: &Value) -> Result<Snapshot> {
        let invalid = |what: &str| err!("snapshot {id}: invalid metadata ({what})");
        let text = |key: &str| -> Result<String> {
            data.get(key).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| invalid(key))
        };
        let format = data.get("format").ok_or_else(|| invalid("format"))?;
        let subvolume = match format.as_u64() {
            Some(FORMAT_VERSION) => ROOT.to_owned(),
            Some(FORMAT_WITH_SUBVOLUME) => {
                let name = text("subvolume")?;
                if name.is_empty() {
                    return Err(invalid("subvolume"));
                }
                name
            }
            _ => bail!("snapshot {id}: unsupported metadata format {format}"),
        };
        let boot = match data.get("boot") {
            None | Some(Value::Null) => None,
            Some(boot) => {
                let hash = |key: &str| {
                    boot.get(key)
                        .and_then(Value::as_str)
                        .filter(|h| is_hash(h))
                        .map(str::to_owned)
                        .ok_or_else(|| invalid(&format!("boot.{key}")))
                };
                let name = |key: &str| {
                    boot.get(key)
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty() && !v.contains('/') && !v.starts_with('.'))
                        .map(str::to_owned)
                        .ok_or_else(|| invalid(&format!("boot.{key}")))
                };
                Some(BootFiles {
                    pkgbase: name("pkgbase")?,
                    version: name("version")?,
                    kernel: hash("kernel")?,
                    initramfs: hash("initramfs")?,
                })
            }
        };
        Ok(Snapshot {
            id,
            created: parse_created(&text("created")?).ok_or_else(|| invalid("created"))?,
            kernel: text("kernel")?,
            kind: text("kind")?,
            description: text("description")?,
            keep: data.get("keep") == Some(&Value::Bool(true)),
            subvolume,
            boot,
        })
    }
}

/// A stored boot file's name: a lowercase hex SHA-256, so it can never be a
/// path that leaves the .boot directory.
fn is_hash(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// ISO 8601 as btrfs-patrol writes it, "2026-09-21T22:25:56+02:00", or
/// without an offset, which Python's fromisoformat also accepts: local time.
fn parse_created(text: &str) -> Option<DateTime<FixedOffset>> {
    if let Ok(created) = DateTime::parse_from_rfc3339(text) {
        return Some(created);
    }
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(Local.from_local_datetime(&naive).earliest()?.fixed_offset())
}

/// The oldest snapshots of `subvolume` that aren't kept, beyond the newest
/// `max_snapshots` of them.
pub fn select_for_pruning<'a>(
    snapshots: &'a [Snapshot],
    max_snapshots: usize,
    subvolume: &str,
) -> Vec<&'a Snapshot> {
    let mut candidates: Vec<&Snapshot> =
        snapshots.iter().filter(|s| s.subvolume == subvolume && !s.keep).collect();
    candidates.sort_by_key(|s| s.id);
    let excess = candidates.len().saturating_sub(max_snapshots);
    candidates.truncate(excess);
    candidates
}

pub struct Store {
    pub directory: PathBuf,
}

impl Store {
    pub fn new(directory: impl Into<PathBuf>) -> Store {
        Store { directory: directory.into() }
    }

    pub fn path(&self, id: u64) -> PathBuf {
        self.directory.join(id.to_string())
    }

    pub fn subvolume(&self, id: u64) -> PathBuf {
        self.path(id).join(SUBVOLUME_NAME)
    }

    pub fn boot_file(&self, hash: &str) -> PathBuf {
        self.directory.join(BOOT_DIR).join(hash)
    }

    pub fn ids(&self) -> Result<Vec<u64>> {
        let entries = std::fs::read_dir(&self.directory).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                err!("snapshots directory not found: {}", self.directory.display())
            }
            std::io::ErrorKind::PermissionDenied => {
                err!("{} is readable only by root; run it with sudo", self.directory.display())
            }
            _ => err!("{}: {e}", self.directory.display()),
        })?;
        let mut ids: Vec<u64> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                name.bytes().all(|b| b.is_ascii_digit()).then(|| name.parse().ok())?
            })
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    pub fn load(&self, id: u64) -> Result<Snapshot> {
        let info = self.path(id).join(INFO_FILE);
        let text = std::fs::read_to_string(&info).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => err!("snapshot {id} has no {INFO_FILE}"),
            _ => err!("{}: {e}", info.display()),
        })?;
        let data: Value =
            serde_json::from_str(&text).map_err(|e| err!("{}: invalid JSON ({e})", info.display()))?;
        Snapshot::from_json(id, &data)
    }

    /// Every snapshot whose metadata can be read.
    ///
    /// An entry with no readable info.json is skipped, not fatal: it is being
    /// created right now (readers hold no lock) or was left by a crash.
    /// `check` walks the ids itself and reports those, so they aren't hidden.
    pub fn load_all(&self) -> Result<Vec<Snapshot>> {
        Ok(self.ids()?.into_iter().filter_map(|id| self.load(id).ok()).collect())
    }

    pub fn save(&self, snapshot: &Snapshot) -> Result<()> {
        let text = serde_json::to_string_pretty(&snapshot.to_json()).expect("plain JSON") + "\n";
        system::write_atomic(&self.path(snapshot.id).join(INFO_FILE), text.as_bytes(), None)
    }

    /// Take the next snapshot ID and advance the counter past it. The
    /// counter only moves forward, so an ID is never reused, even after the
    /// newest snapshot is deleted - and it never falls behind what is on
    /// disk, as in a store btrfs-patrol made before it had a counter.
    fn reserve_id(&self) -> Result<u64> {
        let counter = self.directory.join(COUNTER_FILE);
        let stored = match std::fs::read_to_string(&counter) {
            Ok(text) => text
                .trim()
                .parse::<u64>()
                .map_err(|_| err!("{}: expected a snapshot ID", counter.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 1,
            Err(e) => bail!("{}: {e}", counter.display()),
        };
        let id = stored.max(self.ids()?.last().map_or(1, |last| last + 1));
        // Advanced before anything is created, so a failure can't hand it out twice.
        system::write_atomic(&counter, format!("{}\n", id + 1).as_bytes(), None)?;
        Ok(id)
    }

    /// Reserve an ID and write the metadata for a snapshot whose subvolume
    /// the caller then puts in place. Hold the lock.
    pub fn new_entry(&self, kind: &str, description: &str, keep: bool, subvolume: &str) -> Result<Snapshot> {
        let now = chrono::Local::now().fixed_offset();
        let snapshot = Snapshot {
            id: self.reserve_id()?,
            // Metadata keeps whole seconds; so does this, so the two are equal.
            created: now.with_nanosecond(0).unwrap_or(now),
            kernel: system::kernel_release(),
            kind: kind.into(),
            description: description.into(),
            keep,
            subvolume: subvolume.into(),
            boot: None,
        };
        let path = self.path(snapshot.id);
        std::fs::create_dir(&path).map_err(|e| err!("{}: {e}", path.display()))?;
        if let Err(e) = self.save(&snapshot) {
            let _ = self.remove_entry(snapshot.id);
            return Err(e);
        }
        Ok(snapshot)
    }

    /// Remove a snapshot's metadata and its directory, which must hold nothing else.
    pub fn remove_entry(&self, id: u64) -> Result<()> {
        let path = self.path(id);
        for name in [INFO_FILE.to_owned(), format!("{INFO_FILE}.tmp")] {
            let _ = std::fs::remove_file(path.join(name));
        }
        std::fs::remove_dir(&path).map_err(|e| err!("{}: {e}", path.display()))
    }

    /// Take a read-only snapshot of `source`, the managed subvolume named
    /// `subvolume`. Metadata first, so a subvolume never exists without it;
    /// any failure - Ctrl+C included, which reaches btrfs and fails it -
    /// removes the entry again. Hold the lock.
    pub fn create(
        &self,
        source: &Path,
        kind: &str,
        description: &str,
        keep: bool,
        subvolume: &str,
    ) -> Result<Snapshot> {
        let snapshot = self.new_entry(kind, description, keep, subvolume)?;
        let made = btrfs::create_snapshot(source, &self.subvolume(snapshot.id), true);
        if let Err(e) = made.and_then(|()| if system::interrupted() { bail!("interrupted") } else { Ok(()) })
        {
            if self.subvolume(snapshot.id).exists() {
                let _ = btrfs::delete_subvolume(&self.subvolume(snapshot.id));
            }
            let _ = self.remove_entry(snapshot.id);
            return Err(e);
        }
        Ok(snapshot)
    }

    /// Delete a snapshot's subvolume, then its metadata. Hold the lock.
    pub fn delete(&self, id: u64) -> Result<()> {
        let subvolume = self.subvolume(id);
        if subvolume.exists() {
            btrfs::delete_subvolume(&subvolume)?;
        }
        self.remove_entry(id)
    }

    /// Delete `subvolume`'s snapshots beyond `max_snapshots`, and return them. Hold the lock.
    pub fn prune(&self, max_snapshots: usize, subvolume: &str) -> Result<Vec<Snapshot>> {
        let all = self.load_all()?;
        let pruned: Vec<Snapshot> =
            select_for_pruning(&all, max_snapshots, subvolume).into_iter().cloned().collect();
        for snapshot in &pruned {
            self.delete(snapshot.id)?;
        }
        Ok(pruned)
    }

    /// When the store last changed: snapshots coming and going touch the
    /// directory. The TUI reloads when it moves.
    pub fn modified(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(&self.directory).and_then(|m| m.modified()).ok()
    }
}

impl AsRef<Path> for Store {
    fn as_ref(&self) -> &Path {
        &self.directory
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn snapshot(id: u64) -> Snapshot {
        Snapshot {
            id,
            created: DateTime::parse_from_rfc3339("2026-09-21T22:25:56+02:00").unwrap(),
            kernel: "7.2.11-arch1-1".into(),
            kind: "manual".into(),
            description: String::new(),
            keep: false,
            subvolume: ROOT.into(),
            boot: None,
        }
    }

    #[test]
    fn reads_btrfs_patrols_own_metadata() {
        // Exactly what btrfs-patrol 0.8 writes for a root and a /home snapshot.
        let root = json!({"format": 1, "created": "2026-09-21T22:25:56+02:00",
            "kernel": "6.17.1-300.fc43.x86_64", "kind": "dnf-pre",
            "description": "upgrade mesa", "keep": true});
        let s = Snapshot::from_json(3, &root).unwrap();
        assert_eq!((s.id, s.subvolume.as_str(), s.kind.as_str(), s.keep), (3, ROOT, "dnf-pre", true));
        assert_eq!(s.created.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-09-21 22:25:56");
        let home = json!({"format": 2, "created": "2026-09-21T22:25:56+02:00", "kernel": "k",
            "kind": "timer", "description": "", "keep": false, "subvolume": "home"});
        assert_eq!(Snapshot::from_json(4, &home).unwrap().subvolume, "home");
    }

    #[test]
    fn writes_what_btrfs_patrol_reads() {
        let mut s = snapshot(7);
        assert_eq!(
            serde_json::to_string(&s.to_json()).unwrap(),
            r#"{"format":1,"created":"2026-09-21T22:25:56+02:00","kernel":"7.2.11-arch1-1","kind":"manual","description":"","keep":false}"#
        );
        s.subvolume = "home".into();
        s.boot = Some(BootFiles {
            pkgbase: "linux".into(),
            version: "7.2.11-arch1-1".into(),
            kernel: "a".repeat(64),
            initramfs: "b".repeat(64),
        });
        let data = s.to_json();
        assert_eq!(data["format"], 2);
        assert_eq!(Snapshot::from_json(7, &data).unwrap(), s);
    }

    #[test]
    fn rejects_bad_metadata() {
        let good = snapshot(1).to_json();
        for (key, value) in [
            ("format", json!(3)),
            ("created", json!("yesterday")),
            ("kernel", json!(5)),
            (
                "boot",
                json!({"pkgbase": "linux", "version": "v", "kernel": "../../etc/shadow", "initramfs": "b".repeat(64)}),
            ),
            (
                "boot",
                json!({"pkgbase": "../x", "version": "v", "kernel": "a".repeat(64), "initramfs": "b".repeat(64)}),
            ),
        ] {
            let mut data = good.clone();
            data[key] = value;
            assert!(Snapshot::from_json(1, &data).is_err(), "{key}");
        }
        let mut data = good.clone();
        data["format"] = json!(2);
        assert!(Snapshot::from_json(1, &data).is_err(), "format 2 without a subvolume");
        let mut data = good;
        data.as_object_mut().unwrap().remove("kind");
        assert!(Snapshot::from_json(1, &data).is_err());
    }

    #[test]
    fn keep_must_be_true_itself() {
        let mut data = snapshot(1).to_json();
        data["keep"] = json!("yes");
        assert!(!Snapshot::from_json(1, &data).unwrap().keep);
    }

    #[test]
    fn pruning_skips_kept_and_other_subvolumes() {
        let mut all: Vec<Snapshot> = (1..=6).map(snapshot).collect();
        all[0].keep = true;
        all[1].subvolume = "home".into();
        let ids: Vec<u64> = select_for_pruning(&all, 2, ROOT).iter().map(|s| s.id).collect();
        assert_eq!(ids, [3, 4]);
        assert!(select_for_pruning(&all, 10, ROOT).is_empty());
    }

    #[test]
    fn store_lists_numbered_directories_and_skips_unreadable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        for id in [10u64, 2, 3] {
            std::fs::create_dir(store.path(id)).unwrap();
        }
        std::fs::write(store.path(2).join(INFO_FILE), snapshot(2).to_json().to_string()).unwrap();
        std::fs::write(store.path(10).join(INFO_FILE), snapshot(10).to_json().to_string()).unwrap();
        std::fs::create_dir(dir.path().join(".boot")).unwrap();
        std::fs::create_dir(dir.path().join("x1")).unwrap();
        std::fs::write(dir.path().join("5"), "").unwrap();
        std::fs::write(dir.path().join(".next-id"), "11\n").unwrap();
        assert_eq!(store.ids().unwrap(), [2, 3, 10]);
        let ids: Vec<u64> = store.load_all().unwrap().iter().map(|s| s.id).collect();
        assert_eq!(ids, [2, 10]);
        assert!(store.load(3).unwrap_err().0.contains("has no info.json"));
    }

    #[test]
    fn ids_are_never_reused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let a = store.new_entry("manual", "", false, ROOT).unwrap();
        let b = store.new_entry("timer", "", false, "home").unwrap();
        assert_eq!((a.id, b.id), (1, 2));
        store.remove_entry(b.id).unwrap();
        assert_eq!(store.new_entry("manual", "", false, ROOT).unwrap().id, 3, "2 is gone, not free");
        // A store from before the counter: it starts past what is there.
        std::fs::remove_file(dir.path().join(COUNTER_FILE)).unwrap();
        std::fs::create_dir(store.path(40)).unwrap();
        assert_eq!(store.new_entry("manual", "", false, ROOT).unwrap().id, 41);
        let loaded = store.load(1).unwrap();
        assert_eq!(loaded, a);
        std::fs::write(dir.path().join(COUNTER_FILE), "x").unwrap();
        assert!(store.new_entry("manual", "", false, ROOT).is_err());
    }

    #[test]
    fn missing_store() {
        let store = Store::new("/nonexistent/snapshots");
        assert!(store.ids().unwrap_err().0.contains("not found"));
    }
}
