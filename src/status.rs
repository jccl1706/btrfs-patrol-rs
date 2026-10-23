// SPDX-License-Identifier: GPL-3.0-or-later
//! A summary of the store anyone may read, for desktop widgets.
//!
//! The store is readable only by root (old snapshots can hold setuid
//! programs with known holes), so a panel running as the user can't look
//! inside it. After every change, btrfs-patrol writes what such a panel
//! needs - the newest snapshot, the counts, whether a rollback waits for a
//! reboot - to /run/btrfs-patrol/status.json, mode 0644. Nothing in it
//! reaches into a snapshot. /run is emptied at boot, so the
//! btrfs-patrol-status service writes it once at startup.
//!
//! Publishing never fails the command that changed the store: a widget
//! that is a moment out of date is no reason to lose a snapshot.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serde_json::json;

use crate::check::pending_rollback;
use crate::error::Result;
use crate::mounts::{self, find_mount};
use crate::{App, btrfs, err, system};

pub const DIR: &str = "/run/btrfs-patrol";
pub const FILE: &str = "status.json";

pub fn summary(app: &App) -> Result<serde_json::Value> {
    let snapshots = app.store.load_all()?;
    let newest = snapshots.iter().max_by_key(|s| s.id);
    let mounts = mounts::read_mounts()?;
    let pending = pending_rollback(&app.config, find_mount(Path::new("/"), &mounts), &btrfs::is_read_only);
    Ok(json!({
        "version": 1,
        "updated": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "count": snapshots.len(),
        "kept": snapshots.iter().filter(|s| s.keep).count(),
        "last": newest.map(|s| json!({
            "id": s.id,
            "created": s.created.to_rfc3339(),
            "kind": s.kind,
            "description": crate::table::printable(&s.description),
            "subvolume": s.subvolume,
        })),
        "pending_rollback": pending,
    }))
}

fn write(app: &App, dir: &Path) -> Result<()> {
    let text = serde_json::to_string_pretty(&summary(app)?).expect("plain JSON") + "\n";
    if !dir.is_dir() {
        std::fs::create_dir_all(dir).map_err(|e| err!("{}: {e}", dir.display()))?;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| err!("{}: {e}", dir.display()))?;
    system::write_atomic(&dir.join(FILE), text.as_bytes(), Some(0o644))
}

/// Write the status file, saying nothing if it can't be written.
pub fn publish(app: &App) {
    let _ = write(app, Path::new(DIR));
}

/// For the boot service: write it, and say so if it can't be.
pub fn cmd_publish(app: &App) -> Result<u8> {
    write(app, Path::new(DIR))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ColorChoice, Config};
    use crate::snapshot::Store;
    use crate::snapshot::tests::snapshot;

    #[test]
    fn summary_of_a_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        for id in [3u64, 7] {
            std::fs::create_dir(store.path(id)).unwrap();
            let mut s = snapshot(id);
            s.keep = id == 3;
            s.kind = "pacman-pre".into();
            s.description = "pacman -Syu: linux\x1b[2J".into();
            store.save(&s).unwrap();
        }
        let app = App { config: Config::default(), store, color: ColorChoice::Never };
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("btrfs-patrol");
        write(&app, &target).unwrap();
        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(target.join(FILE)).unwrap()).unwrap();
        assert_eq!((data["count"].as_u64(), data["kept"].as_u64()), (Some(2), Some(1)));
        assert_eq!(data["last"]["id"], 7);
        assert_eq!(data["last"]["kind"], "pacman-pre");
        assert_eq!(data["last"]["description"], "pacman -Syu: linux [2J", "no escapes for a widget to print");
        assert_eq!(std::fs::metadata(target.join(FILE)).unwrap().permissions().mode() & 0o777, 0o644);
    }
}
