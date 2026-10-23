// SPDX-License-Identifier: GPL-3.0-or-later
//! The commands that change the store: snapshot, describe, keep, delete,
//! prune. Each takes the store lock for its changes, and each shared step -
//! taking snapshots, pruning, collecting unused kernels - is one function
//! the command line, the pacman hook and the TUI all call.

use std::collections::BTreeMap;
use std::path::Path;

use crate::check::{mounted_snapshot, pending_rollback, pending_rollback_message};
use crate::config::{Config, ManagedSubvolume};
use crate::error::Result;
use crate::kernel::{self, ESP, MODULES};
use crate::mounts::{self, Mount, find_mount};
use crate::snapshot::{ROOT, Snapshot};
use crate::{App, bail, btrfs, entries, err, system};

/// What a run of snapshot-taking did, for whoever reports it.
#[derive(Debug, Default)]
pub struct Taken {
    pub created: Vec<Snapshot>,
    pub pruned: Vec<Snapshot>,
    /// Things that went wrong without stopping it, such as a kernel that
    /// could not be stored.
    pub warnings: Vec<String>,
    /// Subvolumes skipped because a rollback of them waits for a reboot.
    pub skipped: Vec<String>,
    /// What changed in the boot menu.
    pub entries: Vec<String>,
}

/// The subvolumes a snapshot command takes: the named ones, or every one it
/// is for (the timer's snapshot only those with timer = true).
pub fn selected_subvolumes(config: &Config, names: &[String], kind: &str) -> Result<Vec<ManagedSubvolume>> {
    let managed = config.managed();
    if names.is_empty() {
        return Ok(managed
            .into_iter()
            .filter(|s| match kind {
                "timer" => s.timer,
                k if k.starts_with("pacman") => s.pacman,
                _ => true,
            })
            .collect());
    }
    let mut chosen: Vec<ManagedSubvolume> = Vec::new();
    for name in names {
        let Some(subvolume) = managed.iter().find(|s| &s.name == name) else {
            let known: Vec<&str> = managed.iter().map(|s| s.name.as_str()).collect();
            bail!("no subvolume named {name:?} in the configuration (there are: {})", known.join(", "));
        };
        if !chosen.contains(subvolume) {
            chosen.push(subvolume.clone());
        }
    }
    Ok(chosen)
}

/// The store has to be its own mounted subvolume before anything is written.
/// Unmounted, snapshots would land in the parent subvolume instead - never
/// listed, never pruned, numbered from a counter of their own - and a
/// scheduled snapshot would report success every day while protecting nothing.
pub fn require_store_mounted(config: &Config, mounts: &[Mount]) -> Result<()> {
    let dir = &config.snapshots_dir;
    match find_mount(dir, mounts) {
        None => bail!(
            "{} is not a mount point; run 'btrfs-patrol setup' to create and mount the top-level {:?} subvolume there",
            dir.display(),
            config.snapshots_subvolume
        ),
        Some(m) if m.root != format!("/{}", config.snapshots_subvolume) => bail!(
            "{} is subvolume {:?}, but filesystem.snapshots_subvolume is {:?}",
            dir.display(),
            m.root,
            config.snapshots_subvolume
        ),
        Some(_) => Ok(()),
    }
}

/// Snapshot IDs the running system is mounted from, by subvolume name.
pub fn in_use(config: &Config, mounts: &[Mount]) -> BTreeMap<String, u64> {
    config
        .managed()
        .into_iter()
        .filter_map(|s| Some((s.name.clone(), mounted_snapshot(config, find_mount(&s.path, mounts))?)))
        .collect()
}

/// Take snapshots of `subvolumes`, store root's kernel, prune, collect
/// unused kernels: the whole of `snapshot`, of the pacman hook and of the
/// timer. A rollback waiting for its reboot is an error for a snapshot
/// someone asked for, and a quiet skip for the automatic ones.
pub fn take_snapshots(
    app: &App,
    subvolumes: &[ManagedSubvolume],
    kind: &str,
    description: &str,
    keep: bool,
) -> Result<Taken> {
    let config = &app.config;
    let mounts = mounts::read_mounts()?;
    let mut taken = Taken::default();
    let mut todo = Vec::new();
    for subvolume in subvolumes {
        match pending_rollback(config, find_mount(&subvolume.path, &mounts), &btrfs::is_read_only) {
            Some(id) if kind == "manual" => {
                bail!("{}; reboot first", pending_rollback_message(id, &subvolume.path))
            }
            Some(_) => taken.skipped.push(subvolume.name.clone()),
            None => todo.push(subvolume),
        }
    }
    if todo.is_empty() {
        return Ok(taken);
    }
    require_store_mounted(config, &mounts)?;
    let _lock = system::lock(&app.store.directory)?;
    for subvolume in todo {
        let mut snapshot = app.store.create(&subvolume.path, kind, description, keep, &subvolume.name)?;
        if subvolume.name == ROOT && config.store_kernels {
            match kernel::store_current(&app.store, Path::new(ESP), Path::new(MODULES), &snapshot.kernel) {
                Ok(boot) => {
                    snapshot.boot = Some(boot);
                    app.store.save(&snapshot)?;
                }
                Err(e) => taken.warnings.push(format!("snapshot {}: {e}", snapshot.id)),
            }
        }
        taken.created.push(snapshot);
        taken.pruned.extend(app.store.prune(subvolume.max_snapshots, &subvolume.name)?);
        if system::interrupted() {
            bail!("interrupted");
        }
    }
    kernel::collect_garbage(&app.store)?;
    let (infos, warnings) = sync_entries(app);
    taken.entries = infos;
    taken.warnings.extend(warnings);
    Ok(taken)
}

/// Make the boot menu match the snapshots, after anything that adds or
/// removes them. Never fails the caller - a full or missing ESP is no reason
/// to lose the snapshot just taken - so problems come back as warnings.
pub fn sync_entries(app: &App) -> (Vec<String>, Vec<String>) {
    let mut infos = Vec::new();
    let has_ours = Path::new(ESP).join("loader/entries").is_dir()
        && std::fs::read_dir(Path::new(ESP).join("loader/entries")).is_ok_and(|d| {
            d.filter_map(|e| e.ok())
                // Any name ours ever had, so old entries are cleaned up too.
                .any(|e| e.file_name().to_string_lossy().contains("btrfs-patrol-"))
        });
    if app.config.boot_entries == 0 && !has_ours {
        return (infos, Vec::new());
    }
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    match entries::sync(app, Path::new(ESP), &cmdline) {
        Ok(synced) => {
            infos.extend(synced.removed.iter().map(|id| format!("removed the boot entry for snapshot {id}")));
            if !synced.written.is_empty() {
                let ids: Vec<String> = synced.written.iter().map(|id| id.to_string()).collect();
                infos.push(format!("boot menu: snapshot {}", ids.join(", ")));
            }
            (infos, Vec::new())
        }
        Err(e) => (infos, vec![format!("could not update the snapshot boot entries: {e}")]),
    }
}

fn report_entries(app: &App) {
    let (infos, warnings) = sync_entries(app);
    for info in infos {
        app.info(&info);
    }
    for warning in warnings {
        crate::warn(app.color, &warning);
    }
}

fn of(app: &App, snapshot: &Snapshot) -> String {
    if app.config.subvolumes.is_empty() { String::new() } else { format!(" of {}", snapshot.subvolume) }
}

pub fn cmd_snapshot(app: &App, description: &str, keep: bool, names: &[String], kind: &str) -> Result<u8> {
    let subvolumes = selected_subvolumes(&app.config, names, kind)?;
    let taken = take_snapshots(app, &subvolumes, kind, description, keep)?;
    for name in &taken.skipped {
        app.info(&format!("a rollback of {name} is waiting for a reboot; skipping its scheduled snapshot"));
    }
    for snapshot in &taken.created {
        let kernel = match &snapshot.boot {
            Some(boot) => format!(", with kernel {}", boot.version),
            None => String::new(),
        };
        app.info(&format!("created snapshot {}{}{kernel}", snapshot.id, of(app, snapshot)));
    }
    for snapshot in &taken.pruned {
        app.info(&format!("pruned snapshot {}", snapshot.id));
    }
    for info in &taken.entries {
        app.info(info);
    }
    for warning in &taken.warnings {
        crate::warn(app.color, warning);
    }
    Ok(0)
}

pub fn describe(app: &App, id: u64, description: &str) -> Result<()> {
    let _lock = system::lock(&app.store.directory)?;
    let mut snapshot = app.store.load(id).map_err(|_| err!("no snapshot with ID {id}"))?;
    snapshot.description = description.into();
    app.store.save(&snapshot)
}

pub fn cmd_describe(app: &App, id: u64, description: &str) -> Result<u8> {
    describe(app, id, description)?;
    Ok(0)
}

pub fn set_keep(app: &App, ids: &[u64], keep: bool) -> Result<Vec<u64>> {
    let _lock = system::lock(&app.store.directory)?;
    let mut changed = Vec::new();
    for &id in ids {
        let mut snapshot = app.store.load(id)?;
        if snapshot.keep != keep {
            snapshot.keep = keep;
            app.store.save(&snapshot)?;
            changed.push(id);
        }
    }
    Ok(changed)
}

pub fn cmd_keep(app: &App, selector: &str, keep: bool) -> Result<u8> {
    let ids: Vec<u64> = app.matching(selector)?.iter().map(|s| s.id).collect();
    for id in set_keep(app, &ids, keep)? {
        app.info(&format!("snapshot {id} {}", if keep { "will be kept" } else { "can be pruned again" }));
    }
    Ok(0)
}

/// Delete snapshots - never one the running system is mounted from. That
/// happens for two reasons, a rollback waiting for its reboot or a snapshot
/// booted from its boot entry, and in both 'btrfs subvolume delete' would
/// be aimed at the running system.
pub fn delete(app: &App, ids: &[u64]) -> Result<Vec<u64>> {
    let busy: Vec<u64> = in_use(&app.config, &mounts::read_mounts()?).into_values().collect();
    let blocked: Vec<String> = ids.iter().filter(|id| busy.contains(id)).map(|id| id.to_string()).collect();
    if !blocked.is_empty() {
        bail!(
            "snapshot {} is what the system is running from; reboot into the normal system first, then delete it",
            blocked.join(", ")
        );
    }
    let _lock = system::lock(&app.store.directory)?;
    let existing = app.store.ids()?;
    let mut deleted = Vec::new();
    for &id in ids {
        if existing.contains(&id) {
            app.store.delete(id)?;
            deleted.push(id);
        }
        if system::interrupted() {
            break;
        }
    }
    kernel::collect_garbage(&app.store)?;
    Ok(deleted)
}

pub fn cmd_delete(app: &App, selector: &str, yes: bool) -> Result<u8> {
    let snapshots = app.matching(selector)?;
    system::require_confirmation_possible(yes)?;
    app.print_table(&snapshots);
    if !system::confirm(&format!("Delete {} snapshot(s)?", snapshots.len()), yes)? {
        app.info("nothing deleted");
        return Ok(1);
    }
    let ids: Vec<u64> = snapshots.iter().map(|s| s.id).collect();
    for id in delete(app, &ids)? {
        app.info(&format!("deleted snapshot {id}"));
    }
    report_entries(app);
    Ok(0)
}

pub fn prune_all(app: &App) -> Result<Vec<Snapshot>> {
    let _lock = system::lock(&app.store.directory)?;
    let mut pruned = Vec::new();
    for subvolume in app.config.managed() {
        pruned.extend(app.store.prune(subvolume.max_snapshots, &subvolume.name)?);
    }
    kernel::collect_garbage(&app.store)?;
    Ok(pruned)
}

pub fn cmd_prune(app: &App) -> Result<u8> {
    let pruned = prune_all(app)?;
    for snapshot in &pruned {
        app.info(&format!("pruned snapshot {}", snapshot.id));
    }
    if pruned.is_empty() {
        app.info("nothing to prune");
    }
    report_entries(app);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mounts::{parse_mountinfo, tests::SAMPLE};

    #[test]
    fn which_subvolumes() {
        let config =
            crate::config::parse("[subvolumes.home]\npath = \"/home\"\ntimer = false\npacman = true\n")
                .unwrap();
        let names = |v: Vec<ManagedSubvolume>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(selected_subvolumes(&config, &[], "manual").unwrap()), ["root", "home"]);
        assert_eq!(names(selected_subvolumes(&config, &[], "timer").unwrap()), ["root"]);
        assert_eq!(names(selected_subvolumes(&config, &[], "pacman-pre").unwrap()), ["root", "home"]);
        let asked = vec!["home".to_owned(), "home".to_owned()];
        assert_eq!(names(selected_subvolumes(&config, &asked, "manual").unwrap()), ["home"]);
        let wrong = vec!["srv".to_owned()];
        assert!(
            selected_subvolumes(&config, &wrong, "manual").unwrap_err().0.contains("there are: root, home")
        );
    }

    #[test]
    fn the_store_must_be_mounted() {
        let config = Config::default();
        assert!(require_store_mounted(&config, &parse_mountinfo(SAMPLE)).is_ok());
        let unmounted = SAMPLE.replace(" /.snapshots ", " /elsewhere ");
        assert!(
            require_store_mounted(&config, &parse_mountinfo(&unmounted))
                .unwrap_err()
                .0
                .contains("not a mount point")
        );
        let wrong = SAMPLE.replace("/@snapshots /.snapshots", "/@ /.snapshots");
        assert!(
            require_store_mounted(&config, &parse_mountinfo(&wrong)).unwrap_err().0.contains("is subvolume")
        );
    }

    #[test]
    fn what_the_system_runs_from() {
        let config = Config::default();
        assert!(in_use(&config, &parse_mountinfo(SAMPLE)).is_empty());
        let booted = SAMPLE.replace("/@ / ", "/@snapshots/7/snapshot / ");
        assert_eq!(in_use(&config, &parse_mountinfo(&booted)).get(ROOT), Some(&7));
    }
}
