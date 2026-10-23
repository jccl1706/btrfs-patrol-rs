// SPDX-License-Identifier: GPL-3.0-or-later
//! Rolling a managed subvolume back to one of its snapshots.
//!
//! [`prepare`] checks everything it can without changing anything, and
//! returns a plan while the top-level subvolume (ID 5) is mounted.
//! [`Plan::execute`] then rolls back:
//!
//! 1. Create a snapshot entry, kind "rollback" and kept, for the current
//!    state - with the kernel it runs on stored, so the rollback can itself
//!    be rolled back.
//! 2. Move the current subvolume into that entry.
//! 3. Create a writable snapshot of the chosen snapshot in its place.
//! 4. Move nested subvolumes (systemd makes var/lib/portables in root, for
//!    one) from the old subvolume into the new one: snapshots don't contain
//!    them, and the old one could never be deleted with them inside.
//! 5. If root is found through the btrfs default subvolume, point it at the
//!    new root and read it back.
//! 6. Arch: put the snapshot's stored kernel and initramfs back on the ESP,
//!    keeping the current ones until everything else has worked.
//!
//! Every step records how to undo it; if one fails, the steps done are
//! undone newest first. The plan is checked again under the store lock -
//! the prompt is an unbounded wait, and a pacman hook or the timer may run
//! in it. Nothing reboots by itself: root, and subvolumes mounted on their
//! own, stay mounted as they are until the reboot, because a mount holds on
//! to a subvolume, not to its name.

use std::path::{Path, PathBuf};

use crate::btrfs::{self, Subvolume};
use crate::check::{mounted_snapshot, pending_rollback, pending_rollback_message};
use crate::config::{Config, ManagedSubvolume};
use crate::error::Result;
use crate::kernel::{self, ESP, MODULES};
use crate::mounts::{self, containing_mount, find_mount};
use crate::setup::TopLevel;
use crate::snapshot::{BootFiles, ROOT, SUBVOLUME_NAME, Snapshot};
use crate::{App, bail, err, system};

pub const PROC_CMDLINE: &str = "/proc/cmdline";
pub const FSTAB: &str = "/etc/fstab";
const BACKUP_SUFFIX: &str = ".btrfs-patrol-old";

/// How the restored subvolume is found at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundBy {
    /// By its name: rootflags=subvol=@, or subvol= in fstab.
    Name(String),
    /// Root, as the btrfs default subvolume.
    Default,
    /// By its path inside the subvolume that contains it.
    Path(String),
}

impl FoundBy {
    fn describe(&self) -> String {
        match self {
            FoundBy::Name(how) => how.clone(),
            FoundBy::Default => "the btrfs default subvolume".into(),
            FoundBy::Path(parent) => format!("its path inside the {parent:?} subvolume"),
        }
    }
}

fn fstab_options<'a>(fstab: &'a str, mount_point: &str) -> Option<Vec<&'a str>> {
    fstab
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.len() >= 4 && !f[0].starts_with('#') && f[1] == mount_point)
        .map(|f| f[3].split(',').collect())
}

fn rootflags(cmdline: &str) -> Vec<&str> {
    cmdline
        .split_whitespace()
        .filter_map(|t| t.strip_prefix("rootflags="))
        .flat_map(|o| o.split(','))
        .collect()
}

/// How root is found at boot, from the restored system's fstab and the
/// kernel command line: by name, or as the default subvolume. Mounting by
/// ID, or by another name, is refused - the restored root gets a new ID and
/// keeps the configured name.
pub fn boot_method(root_subvolume: &str, fstab: &str, cmdline: &str) -> Result<FoundBy> {
    let mut by_name = false;
    for option in fstab_options(fstab, "/").unwrap_or_default().into_iter().chain(rootflags(cmdline)) {
        let (key, value) = option.split_once('=').unwrap_or((option, ""));
        match key {
            "subvolid" => bail!(
                "the root filesystem is mounted by subvolume ID (subvolid=), which can't follow a rollback; \
                 'btrfs-patrol setup' removes it from fstab - check the boot entry's options too"
            ),
            "subvol" if value.trim_matches('/') != root_subvolume.trim_matches('/') => bail!(
                "the root filesystem is mounted as subvol={value}, not the configured root subvolume {root_subvolume:?}"
            ),
            "subvol" => by_name = true,
            _ => {}
        }
    }
    Ok(if by_name { FoundBy::Name(format!("subvol={root_subvolume}")) } else { FoundBy::Default })
}

/// How a subvolume mounted on its own, other than root, is found at boot:
/// subvol= in fstab, by the name it has now.
pub fn fstab_method(mount_point: &Path, subvolume_path: &str, fstab: &str) -> Result<FoundBy> {
    let point = mount_point.to_string_lossy();
    let Some(options) = fstab_options(fstab, &point) else {
        bail!("{FSTAB} has no line for {point}, so nothing would mount the restored subvolume there at boot");
    };
    if options.iter().any(|o| o.starts_with("subvolid=")) {
        bail!(
            "{FSTAB} mounts {point} by subvolume ID (subvolid=), which can't follow a rollback; run 'btrfs-patrol setup'"
        );
    }
    let names: Vec<&str> =
        options.iter().filter_map(|o| o.strip_prefix("subvol=")).map(|v| v.trim_matches('/')).collect();
    if names != [subvolume_path] {
        bail!(
            "{FSTAB} mounts {point} with {}, not subvol={subvolume_path}",
            names.first().map_or("no subvol= option".into(), |n| format!("subvol={n}"))
        );
    }
    Ok(FoundBy::Name(format!("subvol={subvolume_path} in {FSTAB}")))
}

/// The outermost subvolumes inside `parent`, relative to it. Deeper ones
/// move with their parent.
pub fn nested_subvolumes(subvolumes: &[Subvolume], parent: &str) -> Vec<String> {
    let prefix = format!("{}/", parent.trim_matches('/'));
    let mut inside: Vec<&str> = subvolumes.iter().filter_map(|s| s.path.strip_prefix(&prefix)).collect();
    inside.sort_unstable();
    let mut outermost: Vec<String> = Vec::new();
    for path in inside {
        if !outermost.iter().any(|o| path.starts_with(&format!("{o}/"))) {
            outermost.push(path.to_owned());
        }
    }
    outermost
}

/// What happens to the ESP in a root rollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KernelPlan {
    /// Put these stored files back as vmlinuz-<pkgbase> and initramfs-<pkgbase>.img.
    Restore(BootFiles),
    /// The ESP already has the kernel the snapshot needs.
    Keep(String),
}

/// Decide the kernel side of a root rollback, or refuse it.
///
/// The snapshot needs a kernel it has modules for. Its stored one, when
/// there is one; otherwise one already on the ESP will do, if its version
/// is a module directory in the snapshot. Any other kernel on the ESP the
/// snapshot can't run is a warning: picking it in the boot menu would boot
/// without drivers.
pub fn kernel_plan(
    target: &Snapshot,
    snapshot_root: &Path,
    store: &crate::snapshot::Store,
    esp: &Path,
    running: &str,
) -> Result<(KernelPlan, Vec<String>)> {
    let modules = snapshot_root.join("usr/lib/modules");
    let mut warnings = Vec::new();
    let on_esp = kernel::esp_kernels(esp, &modules, running);
    let plan = match &target.boot {
        Some(boot) => {
            if !modules.join(&boot.version).is_dir() {
                bail!("snapshot {} stored kernel {}, but has no modules for it", target.id, boot.version);
            }
            for hash in [&boot.kernel, &boot.initramfs] {
                if !store.boot_file(hash).is_file() {
                    bail!(
                        "snapshot {}'s stored boot file {hash} is missing; 'btrfs-patrol check' says more",
                        target.id
                    );
                }
            }
            let current = esp.join(format!("vmlinuz-{}", boot.pkgbase));
            let same = sha256(&current).ok() == Some(boot.kernel.clone())
                && sha256(&esp.join(format!("initramfs-{}.img", boot.pkgbase))).ok()
                    == Some(boot.initramfs.clone());
            if same { KernelPlan::Keep(boot.version.clone()) } else { KernelPlan::Restore(boot.clone()) }
        }
        None => match on_esp.iter().find(|k| k.pkgbase == "linux").or(on_esp.first()) {
            Some(k) => KernelPlan::Keep(k.version.clone()),
            None => bail!(
                "snapshot {} has no stored kernel, and no kernel on {} has modules in it: the restored system \
                 would boot without its drivers",
                target.id,
                esp.display()
            ),
        },
    };
    let restored_pkgbase = match &plan {
        KernelPlan::Restore(boot) => Some(boot.pkgbase.clone()),
        KernelPlan::Keep(_) => target.boot.as_ref().map(|b| b.pkgbase.clone()).or(Some("linux".into())),
    };
    // Every other kernel on the ESP the restored system has no modules for.
    if let Ok(entries) = std::fs::read_dir(esp) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(pkgbase) = name.strip_prefix("vmlinuz-") else { continue };
            if Some(pkgbase.to_owned()) == restored_pkgbase || on_esp.iter().any(|k| k.pkgbase == pkgbase) {
                continue;
            }
            warnings.push(format!(
                "snapshot {} has no modules for the {pkgbase} kernel on {}; don't pick it in the boot menu",
                target.id,
                esp.display()
            ));
        }
    }
    if let KernelPlan::Restore(boot) = &plan
        && esp.join(format!("initramfs-{}-fallback.img", boot.pkgbase)).is_file()
    {
        warnings.push(format!(
                "the {} fallback initramfs is for the current kernel; after the reboot, 'sudo mkinitcpio -P' rebuilds it for {}",
                boot.pkgbase, boot.version
            ));
    }
    Ok((plan, warnings))
}

fn sha256(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Refuse a configured device that is not the one / is mounted from. Subvolume
/// IDs are not unique across filesystems - every btrfs starts at 256 - so with
/// device names that moved, a rollback could rename another system's root.
fn check_device(configured: &str, mounted: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if configured == mounted {
        return Ok(());
    }
    let rdev = |p: &str| std::fs::metadata(p).map(|m| m.rdev()).ok();
    if rdev(configured).is_some() && rdev(configured) == rdev(mounted) {
        return Ok(());
    }
    bail!(
        "filesystem.device is {configured}, but / is mounted from {mounted}; point it at this system's filesystem, or remove it"
    )
}

pub struct Plan {
    pub target: Snapshot,
    top: TopLevel,
    device: String,
    current_id: u64,
    found_by: FoundBy,
    nested: Vec<String>,
    pub name: String,
    path: PathBuf,
    /// Where it is from the top level: "@", "@home", "@/var/lib/machines".
    location: String,
    /// Whether it stays mounted as it is until the reboot, as root does.
    mounted: bool,
    kernel: Option<KernelPlan>,
    /// Booted from a snapshot's read-only boot entry, to repair a system.
    rescue: bool,
    pub warnings: Vec<String>,
}

pub fn prepare(app: &App, target: &Snapshot) -> Result<Plan> {
    let config = &app.config;
    let Some(subvolume) = config.managed().into_iter().find(|s| s.name == target.subvolume) else {
        bail!(
            "snapshot {} is of the subvolume {:?}, which isn't in the configuration",
            target.id,
            target.subvolume
        );
    };
    let snapshot_root = app.store.subvolume(target.id);
    if !snapshot_root.is_dir() {
        bail!("snapshot {} has no subvolume", target.id);
    }
    let mounts = mounts::read_mounts()?;
    let Some(root_mount) = find_mount(Path::new("/"), &mounts).filter(|m| m.fstype == "btrfs").cloned()
    else {
        bail!("/ is not a btrfs filesystem");
    };
    if let Some(device) = &config.device {
        check_device(device, &root_mount.source)?;
    }
    let device = config.device.clone().unwrap_or_else(|| root_mount.source.clone());
    if subvolume.name == ROOT {
        prepare_root(app, target, &snapshot_root, &root_mount, device)
    } else {
        prepare_other(app, target, &subvolume, &mounts, &root_mount, device)
    }
}

fn prepare_root(
    app: &App,
    target: &Snapshot,
    snapshot_root: &Path,
    root_mount: &mounts::Mount,
    device: String,
) -> Result<Plan> {
    let config = &app.config;
    let booted_from = mounted_snapshot(config, Some(root_mount));
    let rescue =
        booted_from.is_some() && pending_rollback(config, Some(root_mount), &btrfs::is_read_only).is_none();
    let (kernel, mut warnings) =
        kernel_plan(target, snapshot_root, &app.store, Path::new(ESP), &system::kernel_release())?;
    let fstab = std::fs::read_to_string(snapshot_root.join("etc/fstab")).unwrap_or_default();
    // In rescue the command line describes THIS boot, the snapshot's own
    // entry; only the restored system's fstab says how it will be found.
    let cmdline =
        if rescue { String::new() } else { std::fs::read_to_string(PROC_CMDLINE).unwrap_or_default() };
    let found_by = boot_method(&config.root_subvolume, &fstab, &cmdline)?;
    if !config.store_kernels {
        warnings.push("[boot] store_kernels is off: the state you leave keeps no kernel, so rolling back to it later may need one from the ESP".into());
    }

    let top = TopLevel::mount(&device)?;
    let root = top.path.join(&config.root_subvolume);
    if !root.is_dir() {
        bail!("{:?} does not exist on {device}", config.root_subvolume);
    }
    let current_id = if rescue {
        btrfs::subvolume_id(&root)?
    } else {
        let id = btrfs::subvolume_id(Path::new("/"))?;
        if btrfs::subvolume_id(&root)? != id {
            bail!(
                "{:?} on {device} is not the subvolume mounted at /; if you already rolled back, reboot first",
                config.root_subvolume
            );
        }
        id
    };
    check_snapshots_subvolume(config, &top, &device)?;
    if found_by == FoundBy::Default && btrfs::get_default_subvolume_id(&top.path)? != current_id {
        bail!(
            "the system finds its root through the default subvolume, but the default subvolume isn't the root subvolume"
        );
    }
    let nested = nested_subvolumes(&btrfs::list_subvolumes(&top.path)?, &config.root_subvolume);
    Ok(Plan {
        target: target.clone(),
        top,
        device,
        current_id,
        found_by,
        nested,
        name: ROOT.into(),
        path: "/".into(),
        location: config.root_subvolume.clone(),
        mounted: true,
        kernel: Some(kernel),
        rescue,
        warnings,
    })
}

fn prepare_other(
    app: &App,
    target: &Snapshot,
    subvolume: &ManagedSubvolume,
    mounts: &[mounts::Mount],
    root_mount: &mounts::Mount,
    device: String,
) -> Result<Plan> {
    let config = &app.config;
    if let Some(id) = pending_rollback(config, Some(root_mount), &btrfs::is_read_only) {
        bail!("{}; reboot first", pending_rollback_message(id, Path::new("/")));
    }
    let path = &subvolume.path;
    let own = find_mount(path, mounts);
    let (location, found_by) = match own {
        Some(m) => {
            if m.fstype != "btrfs" || m.source != root_mount.source {
                bail!("{} is not on the btrfs filesystem mounted at /", path.display());
            }
            if let Some(id) = pending_rollback(config, Some(m), &btrfs::is_read_only) {
                bail!("{}; reboot first", pending_rollback_message(id, path));
            }
            let location = m.root.trim_matches('/').to_owned();
            let fstab = std::fs::read_to_string(FSTAB).map_err(|e| err!("{FSTAB}: {e}"))?;
            let found = fstab_method(path, &location, &fstab)?;
            (location, found)
        }
        None => {
            let Some(outer) = containing_mount(path, mounts)
                .filter(|o| o.fstype == "btrfs" && o.source == root_mount.source)
            else {
                bail!("{} is not on the btrfs filesystem mounted at /", path.display());
            };
            if !btrfs::is_subvolume(path) {
                bail!("{} is not a btrfs subvolume", path.display());
            }
            let parent = outer.root.trim_matches('/').to_owned();
            let inside = path.strip_prefix(&outer.mount_point).unwrap_or(path).to_string_lossy().into_owned();
            (format!("{parent}/{inside}").trim_matches('/').to_owned(), FoundBy::Path(parent))
        }
    };
    if location.is_empty() {
        bail!("{} is the top-level subvolume, which can't be rolled back", path.display());
    }
    let current_id = btrfs::subvolume_id(path)?;
    let top = TopLevel::mount(&device)?;
    let at = top.path.join(&location);
    if !at.is_dir() || btrfs::subvolume_id(&at)? != current_id {
        bail!("{location:?} on {device} is not the subvolume at {}", path.display());
    }
    check_snapshots_subvolume(config, &top, &device)?;
    let nested = nested_subvolumes(&btrfs::list_subvolumes(&top.path)?, &location);
    Ok(Plan {
        target: target.clone(),
        top,
        device,
        current_id,
        found_by,
        nested,
        name: subvolume.name.clone(),
        path: path.clone(),
        location,
        mounted: own.is_some(),
        kernel: None,
        rescue: false,
        warnings: Vec::new(),
    })
}

fn check_snapshots_subvolume(config: &Config, top: &TopLevel, device: &str) -> Result<()> {
    let at = top.path.join(&config.snapshots_subvolume);
    if !at.is_dir() || btrfs::subvolume_id(&at)? != btrfs::subvolume_id(&config.snapshots_dir)? {
        bail!(
            "{:?} on {device} is not the subvolume mounted at {}",
            config.snapshots_subvolume,
            config.snapshots_dir.display()
        );
    }
    Ok(())
}

type Undo = Vec<(String, Box<dyn FnOnce() -> Result<()>>)>;

fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(|e| err!("move {} to {}: {e}", from.display(), to.display()))
}

/// Run the undo steps newest first; the ones that failed, described.
fn unwind(undo: Undo) -> Vec<String> {
    undo.into_iter()
        .rev()
        .filter_map(|(what, step)| step().err().map(|e| format!("could not {what}: {e}")))
        .collect()
}

/// Copy a stored boot file onto the ESP as `to`: next to it first, flushed,
/// then renamed over it, so a crash leaves the old file or the new one.
fn copy_to_esp(from: &Path, to: &Path) -> Result<()> {
    let tmp = to.with_extension("btrfs-patrol-new");
    std::fs::copy(from, &tmp).map_err(|e| err!("copy {} to {}: {e}", from.display(), tmp.display()))?;
    std::fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(|e| err!("{}: {e}", tmp.display()))?;
    rename(&tmp, to)
}

impl Plan {
    pub fn describe(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "{:<20}{} (ID {}) on {}",
                if self.name == ROOT { "root subvolume:" } else { "subvolume:" },
                self.location,
                self.current_id,
                self.device
            ),
            format!("{:<20}{}", "found at boot by:", self.found_by.describe()),
        ];
        match &self.kernel {
            Some(KernelPlan::Restore(boot)) => lines.push(format!(
                "{:<20}{} {} goes back on the ESP (the current one stays with the saved state)",
                "kernel:", boot.pkgbase, boot.version
            )),
            Some(KernelPlan::Keep(version)) => {
                lines.push(format!("{:<20}{version}, already on the ESP", "kernel:"))
            }
            None => {}
        }
        lines.push(format!(
            "{:<20}{} (moved into the restored subvolume)",
            "nested subvolumes:",
            if self.nested.is_empty() { "none".into() } else { self.nested.join(", ") }
        ));
        lines.push(format!("{:<20}kept as a new snapshot of kind 'rollback'", "current state:"));
        if self.name == ROOT {
            lines.push(format!(
                "{:<20}/var/log, /home and /var/cache are their own subvolumes and are left alone",
                "untouched:"
            ));
        }
        let effect = if self.mounted {
            format!("after a reboot; until then {} is still the current state", self.path.display())
        } else {
            format!(
                "at once; programs with files open in {} keep the current ones until they restart",
                self.path.display()
            )
        };
        lines.push(format!("{:<20}{effect}", "takes effect:"));
        if self.rescue {
            lines.push(format!("{:<20}booted from a snapshot's entry; @ is not in use", "rescue:"));
        }
        lines
    }

    /// Check again, under the lock, what prepare checked before the prompt.
    fn revalidate(&self, app: &App) -> Result<()> {
        if !app.store.subvolume(self.target.id).is_dir() {
            bail!(
                "snapshot {} is gone: pruned or deleted while the rollback waited. Nothing has changed",
                self.target.id
            );
        }
        let current = self.top.path.join(&self.location);
        if btrfs::subvolume_id(&current)? != self.current_id {
            bail!(
                "{} changed while the rollback waited. Nothing has changed; run it again",
                current.display()
            );
        }
        let nested = nested_subvolumes(&btrfs::list_subvolumes(&self.top.path)?, &self.location);
        if nested != self.nested {
            bail!(
                "the subvolumes inside {} changed while the rollback waited ({} now). Nothing has changed; run it again",
                self.location,
                if nested.is_empty() { "none".into() } else { nested.join(", ") }
            );
        }
        Ok(())
    }

    /// Roll back; returns the snapshot that keeps the previous state.
    pub fn execute(&self, app: &App) -> Result<Snapshot> {
        let config = &app.config;
        let _lock = system::lock(&app.store.directory)?;
        self.revalidate(app)?;
        let snapshots = self.top.path.join(&config.snapshots_subvolume);
        let current = self.top.path.join(&self.location);
        let mut undo: Undo = Vec::new();
        let result = (|| -> Result<Snapshot> {
            let mut saved = app.store.new_entry(
                "rollback",
                &format!("state before rollback to {}", self.target.id),
                true,
                &self.name,
            )?;
            let (store_dir, id) = (app.store.directory.clone(), saved.id);
            undo.push((
                "remove the new snapshot entry".into(),
                Box::new(move || crate::snapshot::Store::new(store_dir).remove_entry(id)),
            ));

            // The kernel the current state runs on, so it can be rolled back to as well.
            if self.name == ROOT && config.store_kernels && !self.rescue {
                match kernel::store_current(
                    &app.store,
                    Path::new(ESP),
                    Path::new(MODULES),
                    &system::kernel_release(),
                ) {
                    Ok(boot) => {
                        // The kernel the saved system has, which is not the
                        // running one if a kernel update came without a reboot.
                        saved.kernel = boot.version.clone();
                        saved.boot = Some(boot);
                        app.store.save(&saved)?;
                    }
                    Err(e) => eprintln!(
                        "warning: the current kernel was not stored with snapshot {}: {e}",
                        saved.id
                    ),
                }
            }

            let old = snapshots.join(saved.id.to_string()).join(SUBVOLUME_NAME);
            rename(&current, &old)?;
            let (o, c) = (old.clone(), current.clone());
            undo.push((
                format!("move the old {} subvolume back", self.name),
                Box::new(move || rename(&o, &c)),
            ));

            btrfs::create_snapshot(
                &snapshots.join(self.target.id.to_string()).join(SUBVOLUME_NAME),
                &current,
                false,
            )?;
            let c = current.clone();
            undo.push((
                format!("delete the new {} subvolume", self.name),
                Box::new(move || btrfs::delete_subvolume(&c)),
            ));
            // Into an existing DIRECTORY, 'btrfs subvolume snapshot' puts the
            // snapshot one level down and still succeeds.
            if !btrfs::is_subvolume(&current) {
                bail!("{} is not a subvolume after restoring snapshot {}", current.display(), self.target.id);
            }

            for path in &self.nested {
                let (from, to) = (old.join(path), current.join(path));
                // The restored subvolume has an empty directory where the nested one was.
                if to.is_dir() && std::fs::read_dir(&to).is_ok_and(|mut d| d.next().is_none()) {
                    let _ = std::fs::remove_dir(&to);
                }
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| err!("{}: {e}", parent.display()))?;
                }
                rename(&from, &to)?;
                undo.push((format!("move {path} back"), Box::new(move || rename(&to, &from))));
            }

            if self.name == ROOT && self.found_by == FoundBy::Default {
                let new_id = btrfs::subvolume_id(&current)?;
                btrfs::set_default_subvolume(new_id, &self.top.path)?;
                let (old_id, top) = (self.current_id, self.top.path.clone());
                undo.push((
                    "restore the default subvolume".into(),
                    Box::new(move || btrfs::set_default_subvolume(old_id, &top)),
                ));
                if btrfs::get_default_subvolume_id(&self.top.path)? != new_id {
                    bail!("the default subvolume did not change");
                }
            }

            // Last, so the ESP changes only once the subvolumes are in place.
            if let Some(KernelPlan::Restore(boot)) = &self.kernel {
                let esp = Path::new(ESP);
                for (hash, name) in [
                    (&boot.kernel, format!("vmlinuz-{}", boot.pkgbase)),
                    (&boot.initramfs, format!("initramfs-{}.img", boot.pkgbase)),
                ] {
                    let target = esp.join(&name);
                    let backup = esp.join(format!("{name}{BACKUP_SUFFIX}"));
                    if target.exists() {
                        rename(&target, &backup)?;
                        let (b, t) = (backup.clone(), target.clone());
                        undo.push((format!("put {name} back"), Box::new(move || rename(&b, &t))));
                    }
                    copy_to_esp(&app.store.boot_file(hash), &target)?;
                    if sha256(&target).ok().as_ref() != Some(hash) {
                        bail!("{} does not match the stored copy after copying it", target.display());
                    }
                }
            }
            if system::interrupted() {
                bail!("interrupted");
            }
            Ok(saved)
        })();
        match result {
            Ok(saved) => {
                // The previous kernel is stored with the saved state (or was
                // not stored, and then the backups stay, and say so).
                if let Some(KernelPlan::Restore(boot)) = &self.kernel {
                    for name in
                        [format!("vmlinuz-{}", boot.pkgbase), format!("initramfs-{}.img", boot.pkgbase)]
                    {
                        let backup = Path::new(ESP).join(format!("{name}{BACKUP_SUFFIX}"));
                        if saved.boot.is_some() {
                            let _ = std::fs::remove_file(&backup);
                        }
                    }
                }
                // SAFETY: sync has no preconditions.
                unsafe { libc::sync() };
                Ok(saved)
            }
            Err(e) => {
                let failed = unwind(undo);
                if failed.is_empty() {
                    Err(e)
                } else {
                    Err(err!(
                        "rollback failed ({e}), and undoing it failed too: {}. Check 'btrfs subvolume list /' and {ESP} before rebooting",
                        failed.join("; ")
                    ))
                }
            }
        }
    }
}

pub fn cmd_rollback(app: &App, id: u64, yes: bool, dry_run: bool) -> Result<u8> {
    let target = app.store.load(id).map_err(|_| err!("no snapshot with ID {id}"))?;
    system::require_confirmation_possible(yes || dry_run)?;
    let plan = prepare(app, &target)?;
    println!(
        "roll {} back to snapshot {}, {} ({})",
        plan.name,
        target.id,
        target.created.format("%Y-%m-%d %H:%M"),
        if target.description.is_empty() { &target.kind } else { &target.description }
    );
    for line in plan.describe() {
        println!("  {line}");
    }
    for warning in &plan.warnings {
        crate::warn(app.color, warning);
    }
    if dry_run {
        app.info("dry run: every check passed, nothing changed");
        return Ok(0);
    }
    if !system::confirm("Roll back?", yes)? {
        app.info("nothing changed");
        return Ok(1);
    }
    let saved = plan.execute(app)?;
    let (infos, warnings) = crate::manage::sync_entries(app);
    for info in infos {
        app.info(&info);
    }
    for warning in warnings {
        crate::warn(app.color, &warning);
    }
    app.info(&format!(
        "rolled back to snapshot {}. The previous state is snapshot {saved_id} (kept). Reboot to start the restored system; \
         'btrfs-patrol rollback {saved_id}' undoes this",
        target.id,
        saved_id = saved.id
    ));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::tests::fake_kernel;
    use crate::snapshot::Store;
    use crate::snapshot::tests::snapshot;

    const FSTAB_TEXT: &str =
        "UUID=x / btrfs rw,noatime,subvol=/@ 0 0\nUUID=x /home btrfs rw,subvol=/@home 0 0\n";
    const CMDLINE: &str =
        "initrd=\\initramfs-linux.img rd.luks.name=a=cryptroot root=UUID=x rootflags=subvol=@ rw quiet";

    #[test]
    fn how_root_is_found() {
        assert_eq!(boot_method("@", FSTAB_TEXT, CMDLINE).unwrap(), FoundBy::Name("subvol=@".into()));
        assert_eq!(boot_method("@", "", "root=UUID=x rw").unwrap(), FoundBy::Default);
        assert!(boot_method("@", FSTAB_TEXT, "rootflags=subvolid=256").unwrap_err().0.contains("subvolid"));
        assert!(boot_method("@", "UUID=x / btrfs subvolid=256,subvol=/@ 0 0", CMDLINE).is_err());
        assert!(
            boot_method("@", FSTAB_TEXT, "rootflags=subvol=@old")
                .unwrap_err()
                .0
                .contains("not the configured")
        );
    }

    #[test]
    fn how_others_are_found() {
        assert!(fstab_method(Path::new("/home"), "@home", FSTAB_TEXT).is_ok());
        assert!(fstab_method(Path::new("/srv"), "@srv", FSTAB_TEXT).unwrap_err().0.contains("no line"));
        assert!(
            fstab_method(Path::new("/home"), "@home", "x /home btrfs subvolid=257,subvol=/@home 0 0")
                .is_err()
        );
        assert!(
            fstab_method(Path::new("/home"), "@home", "x /home btrfs subvol=/@h2 0 0")
                .unwrap_err()
                .0
                .contains("subvol=@h2")
        );
    }

    #[test]
    fn nested() {
        let s = |p: &str| Subvolume { id: 1, path: p.into() };
        let all = [
            s("@"),
            s("@/var/lib/portables"),
            s("@/var/lib/machines"),
            s("@/var/lib/machines/x"),
            s("@home"),
            s("@snapshots/1/snapshot"),
        ];
        assert_eq!(nested_subvolumes(&all, "@"), ["var/lib/machines", "var/lib/portables"]);
        assert!(nested_subvolumes(&all, "@home").is_empty());
    }

    /// A store with snapshot 1's subvolume (modules for `modules`), stored
    /// boot files for `stored`, and an ESP holding `esp`.
    fn world(
        dir: &Path,
        modules: &[&str],
        stored: Option<&str>,
        esp: &[(&str, &str)],
    ) -> (Store, Snapshot, PathBuf) {
        let store = Store::new(dir.join("store"));
        let root = store.subvolume(1);
        for m in modules {
            std::fs::create_dir_all(root.join("usr/lib/modules").join(m)).unwrap();
            std::fs::write(root.join("usr/lib/modules").join(m).join("pkgbase"), "linux").unwrap();
        }
        let mut target = snapshot(1);
        if let Some(version) = stored {
            let src = dir.join("src");
            std::fs::create_dir_all(&src).unwrap();
            std::fs::write(src.join("k"), fake_kernel(version)).unwrap();
            std::fs::write(src.join("i"), format!("initramfs {version}")).unwrap();
            target.boot = Some(BootFiles {
                pkgbase: "linux".into(),
                version: version.into(),
                kernel: kernel::put(&store, &src.join("k")).unwrap(),
                initramfs: kernel::put(&store, &src.join("i")).unwrap(),
            });
        }
        let esp_dir = dir.join("esp");
        std::fs::create_dir_all(&esp_dir).unwrap();
        for (pkgbase, version) in esp {
            std::fs::write(esp_dir.join(format!("vmlinuz-{pkgbase}")), fake_kernel(version)).unwrap();
            std::fs::write(esp_dir.join(format!("initramfs-{pkgbase}.img")), format!("initramfs {version}"))
                .unwrap();
        }
        (store, target, esp_dir)
    }

    #[test]
    fn across_a_kernel_update_the_stored_kernel_goes_back() {
        let dir = tempfile::tempdir().unwrap();
        let (store, target, esp) =
            world(dir.path(), &["7.2.10-arch1-1"], Some("7.2.10-arch1-1"), &[("linux", "7.2.11-arch1-1")]);
        let (plan, warnings) =
            kernel_plan(&target, &store.subvolume(1), &store, &esp, "7.2.11-arch1-1").unwrap();
        assert!(matches!(plan, KernelPlan::Restore(ref b) if b.version == "7.2.10-arch1-1"));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn the_same_kernel_stays() {
        let dir = tempfile::tempdir().unwrap();
        let (store, target, esp) =
            world(dir.path(), &["7.2.11-arch1-1"], Some("7.2.11-arch1-1"), &[("linux", "7.2.11-arch1-1")]);
        let (plan, _) = kernel_plan(&target, &store.subvolume(1), &store, &esp, "7.2.11-arch1-1").unwrap();
        assert_eq!(plan, KernelPlan::Keep("7.2.11-arch1-1".into()));
    }

    #[test]
    fn without_a_stored_kernel_the_esp_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let (store, target, esp) =
            world(dir.path(), &["7.2.11-arch1-1"], None, &[("linux", "7.2.11-arch1-1")]);
        assert_eq!(
            kernel_plan(&target, &store.subvolume(1), &store, &esp, "x").unwrap().0,
            KernelPlan::Keep("7.2.11-arch1-1".into())
        );
        let dir = tempfile::tempdir().unwrap();
        let (store, target, esp) =
            world(dir.path(), &["7.2.10-arch1-1"], None, &[("linux", "7.2.11-arch1-1")]);
        let error = kernel_plan(&target, &store.subvolume(1), &store, &esp, "x").unwrap_err().0;
        assert!(error.contains("without its drivers"), "{error}");
    }

    #[test]
    fn refusals_and_warnings() {
        // Stored, but the snapshot has no modules for it.
        let dir = tempfile::tempdir().unwrap();
        let (store, target, esp) =
            world(dir.path(), &["7.2.9-arch1-1"], Some("7.2.10-arch1-1"), &[("linux", "7.2.11-arch1-1")]);
        assert!(
            kernel_plan(&target, &store.subvolume(1), &store, &esp, "x")
                .unwrap_err()
                .0
                .contains("no modules for it")
        );
        // Stored file gone.
        let dir = tempfile::tempdir().unwrap();
        let (store, target, esp) =
            world(dir.path(), &["7.2.10-arch1-1"], Some("7.2.10-arch1-1"), &[("linux", "7.2.11-arch1-1")]);
        std::fs::remove_file(store.boot_file(&target.boot.as_ref().unwrap().kernel)).unwrap();
        assert!(
            kernel_plan(&target, &store.subvolume(1), &store, &esp, "x").unwrap_err().0.contains("missing")
        );
        // Another kernel it can't run, and a fallback image to rebuild.
        let dir = tempfile::tempdir().unwrap();
        let (store, target, esp) = world(
            dir.path(),
            &["7.2.10-arch1-1"],
            Some("7.2.10-arch1-1"),
            &[("linux", "7.2.11-arch1-1"), ("linux-lts", "6.12.50-1-lts")],
        );
        std::fs::write(esp.join("initramfs-linux-fallback.img"), "x").unwrap();
        let (_, warnings) = kernel_plan(&target, &store.subvolume(1), &store, &esp, "x").unwrap();
        assert!(warnings.iter().any(|w| w.contains("linux-lts")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("mkinitcpio -P")), "{warnings:?}");
    }
}
