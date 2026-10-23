// SPDX-License-Identifier: GPL-3.0-or-later
//! `setup`: get a system ready for btrfs-patrol.
//!
//! [`prepare`] works out what is missing and returns a plan, changing
//! nothing except a temporary mount of the top-level subvolume. The plan
//! then does only the missing steps, so running setup again is safe - also
//! after a step failed:
//!
//! 1. Write the configuration, only when / is not the default `@`.
//! 2. Create the snapshots subvolume next to the root subvolume, readable
//!    only by root: old snapshots can hold setuid programs with known holes.
//! 3. Create the mount point, add it to /etc/fstab (saving a backup), mount it.
//! 4. Arch: drop `subvolid=` from fstab lines that also name `subvol=`.
//!    genfstab writes both; after a rollback the ID still names the old
//!    subvolume and mount obeys it, so the restored system would never start.
//! 5. Enable the daily timer, when its unit is installed and disabled.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::btrfs::{self, SUBVOLUME_ROOT_INODE};
use crate::config::{self, Config};
use crate::error::Result;
use crate::mounts::{self, find_mount};
use crate::{bail, err, system};

pub const FSTAB: &str = "/etc/fstab";
pub const FSTAB_BACKUP_SUFFIX: &str = ".btrfs-patrol-backup";
pub const TIMER: &str = "btrfs-patrol-snapshot.timer";

/// The fields of the fstab line that mounts `mount_point`.
pub fn fstab_entry<'a>(fstab: &'a str, mount_point: &Path) -> Option<Vec<&'a str>> {
    fstab
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.len() >= 4 && !f[0].starts_with('#') && Path::new(f[1]) == mount_point)
}

/// Mount options for the snapshots subvolume: root's, with its own subvol=.
pub fn snapshot_mount_options(root: Option<&[&str]>, snapshots_subvolume: &str) -> String {
    let mut options = vec![format!("subvol=/{snapshots_subvolume}")];
    let inherited: Vec<String> = root
        .map(|f| f[3].split(',').collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|o| !matches!(o.split('=').next(), Some("subvol" | "subvolid")) && *o != "defaults")
        .map(str::to_owned)
        .collect();
    if inherited.is_empty() {
        options.push("noatime".into());
    }
    options.extend(inherited);
    options.join(",")
}

/// fstab with `subvolid=` taken out of every btrfs line that also has
/// `subvol=`, and the lines changed - or None when there are none.
pub fn without_subvolids(fstab: &str) -> Option<(String, Vec<String>)> {
    let mut changed = Vec::new();
    let lines: Vec<String> = fstab
        .lines()
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 4 || fields[0].starts_with('#') || fields[2] != "btrfs" {
                return line.to_owned();
            }
            let options: Vec<&str> = fields[3].split(',').collect();
            if !options.iter().any(|o| o.starts_with("subvol="))
                || !options.iter().any(|o| o.starts_with("subvolid="))
            {
                return line.to_owned();
            }
            let kept: Vec<&str> = options.into_iter().filter(|o| !o.starts_with("subvolid=")).collect();
            changed.push(fields[1].to_owned());
            // Only the options field changes; the rest of the line - its
            // spacing, its other fields - stays exactly as it was.
            line.replacen(fields[3], &kept.join(","), 1)
        })
        .collect();
    if changed.is_empty() {
        return None;
    }
    let mut text = lines.join("\n");
    if fstab.ends_with('\n') {
        text.push('\n');
    }
    Some((text, changed))
}

/// The top-level subvolume (ID 5) mounted on a private directory, unmounted on drop.
struct TopLevel {
    path: PathBuf,
}

impl TopLevel {
    fn mount(device: &str) -> Result<TopLevel> {
        let path = PathBuf::from(format!("/run/btrfs-patrol-{}", std::process::id()));
        std::fs::create_dir(&path).map_err(|e| err!("{}: {e}", path.display()))?;
        if let Err(e) = run("mount", &["-o", "subvolid=5", device, &path.to_string_lossy()]) {
            let _ = std::fs::remove_dir(&path);
            return Err(e);
        }
        Ok(TopLevel { path })
    }
}

impl Drop for TopLevel {
    fn drop(&mut self) {
        if run("umount", &[&self.path.to_string_lossy()]).is_ok() {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program).args(args).output().map_err(|e| err!("{program}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {}: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 'systemctl is-enabled' for a unit, or None when it isn't installed.
fn unit_state(unit: &str) -> Option<String> {
    let output = Command::new("systemctl").args(["is-enabled", unit]).output().ok()?;
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!state.is_empty() && state != "not-found").then_some(state)
}

pub struct Plan {
    pub config: Config,
    config_path: PathBuf,
    new_config: Option<String>,
    top: TopLevel,
    create_subvolume: bool,
    fix_subvolume_mode: bool,
    create_mount_point: bool,
    fstab_line: Option<String>,
    mount: bool,
    fstab_without_subvolids: Option<(String, Vec<String>)>,
    enable_timer: bool,
}

/// Work out what setup has to do. Holds the top-level subvolume mounted
/// until the plan is dropped.
pub fn prepare(config_path: Option<&Path>) -> Result<Plan> {
    let mounts = mounts::read_mounts()?;
    let Some(root) = find_mount(Path::new("/"), &mounts).filter(|m| m.fstype == "btrfs") else {
        bail!("/ is not a btrfs filesystem");
    };
    let root_subvolume = root.root.trim_matches('/').to_owned();
    if root_subvolume.is_empty() {
        bail!(
            "/ is the top-level subvolume; btrfs-patrol needs the root in a subvolume of its own, such as @"
        );
    }
    let (path, _) = config::resolve_path(config_path);
    let (config, new_config) = if path.exists() {
        let config = config::load(Some(&path))?;
        if config.root_subvolume != root_subvolume {
            bail!(
                "{} says the root subvolume is {:?}, but / is {root_subvolume:?}",
                path.display(),
                config.root_subvolume
            );
        }
        (config, None)
    } else if root_subvolume == Config::default().root_subvolume {
        (Config::default(), None)
    } else {
        let text = config::EXAMPLE.replacen(
            "root_subvolume = \"@\"",
            &format!("root_subvolume = {root_subvolume:?}"),
            1,
        );
        (config::parse(&text)?, Some(text))
    };

    let dir = config.snapshots_dir.clone();
    let mounted = find_mount(&dir, &mounts);
    if let Some(m) = mounted.filter(|m| m.root != format!("/{}", config.snapshots_subvolume)) {
        bail!(
            "{} already has {:?} mounted, not the subvolume {:?}",
            dir.display(),
            m.root,
            config.snapshots_subvolume
        );
    }
    if mounted.is_none()
        && dir.exists()
        && (!dir.is_dir() || std::fs::read_dir(&dir).map_or(true, |mut d| d.next().is_some()))
    {
        bail!("{} exists and isn't an empty directory", dir.display());
    }

    let fstab = std::fs::read_to_string(FSTAB).map_err(|e| err!("{FSTAB}: {e}"))?;
    let fstab_line = match fstab_entry(&fstab, &dir) {
        Some(entry) => {
            let subvols: Vec<&str> = entry[3]
                .split(',')
                .filter_map(|o| o.strip_prefix("subvol="))
                .map(|v| v.trim_matches('/'))
                .collect();
            if entry[2] != "btrfs" || subvols != [config.snapshots_subvolume.as_str()] {
                bail!(
                    "{FSTAB} already mounts {}, but not as btrfs subvol={}: {}",
                    dir.display(),
                    config.snapshots_subvolume,
                    entry.join(" ")
                );
            }
            None
        }
        None => {
            let uuid = run("findmnt", &["-no", "UUID", "/"])?.trim().to_owned();
            if uuid.is_empty() {
                bail!("could not find the UUID of the filesystem mounted at /");
            }
            let options = snapshot_mount_options(
                fstab_entry(&fstab, Path::new("/")).as_deref(),
                &config.snapshots_subvolume,
            );
            Some(format!("UUID={uuid}  {}  btrfs  {options}  0 0", dir.display()))
        }
    };

    let device = config.device.clone().unwrap_or_else(|| root.source.clone());
    let top = TopLevel::mount(&device)?;
    let subvolume = top.path.join(&config.snapshots_subvolume);
    let exists = subvolume.exists();
    if exists && std::fs::metadata(&subvolume).map(|m| m.ino()).unwrap_or(0) != SUBVOLUME_ROOT_INODE {
        bail!("{:?} on {device} exists but isn't a subvolume", config.snapshots_subvolume);
    }
    let mode = std::fs::metadata(&subvolume).map(|m| m.mode() & 0o777).unwrap_or(0o700);
    Ok(Plan {
        create_subvolume: !exists,
        fix_subvolume_mode: exists && mode != 0o700,
        create_mount_point: mounted.is_none() && !dir.exists(),
        mount: mounted.is_none(),
        fstab_without_subvolids: without_subvolids(&fstab),
        enable_timer: unit_state(TIMER).as_deref() == Some("disabled"),
        fstab_line,
        config,
        config_path: path,
        new_config,
        top,
    })
}

impl Plan {
    pub fn actions(&self) -> Vec<String> {
        let c = &self.config;
        let mut actions = Vec::new();
        if self.new_config.is_some() {
            actions.push(format!(
                "write {} (root subvolume {:?})",
                self.config_path.display(),
                c.root_subvolume
            ));
        }
        if self.create_subvolume {
            actions.push(format!(
                "create the top-level subvolume {:?}, readable only by root",
                c.snapshots_subvolume
            ));
        }
        if self.fix_subvolume_mode {
            actions.push(format!(
                "make {:?} readable only by root (old snapshots can hold setuid programs with known holes)",
                c.snapshots_subvolume
            ));
        }
        if self.create_mount_point {
            actions.push(format!("create the directory {}", c.snapshots_dir.display()));
        }
        if let Some(line) = &self.fstab_line {
            actions.push(format!("add to {FSTAB}, saving a copy as {FSTAB}{FSTAB_BACKUP_SUFFIX}: {line}"));
        }
        if let Some((_, points)) = &self.fstab_without_subvolids {
            actions.push(format!(
                "remove subvolid= from {FSTAB} for {} (it would undo a rollback; subvol= stays)",
                points.join(", ")
            ));
        }
        if self.mount {
            actions.push(format!("mount {}", c.snapshots_dir.display()));
        }
        if self.enable_timer {
            actions.push(format!("enable and start the daily snapshot timer ({TIMER})"));
        }
        actions
    }

    pub fn execute(&self) -> Result<()> {
        let c = &self.config;
        if let Some(text) = &self.new_config {
            if let Some(parent) = self.config_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| err!("{}: {e}", parent.display()))?;
            }
            system::write_atomic(&self.config_path, text.as_bytes(), Some(0o644))?;
        }
        let subvolume = self.top.path.join(&c.snapshots_subvolume);
        if self.create_subvolume {
            btrfs::run([std::ffi::OsStr::new("subvolume"), "create".as_ref(), subvolume.as_os_str()])?;
        }
        if self.create_subvolume || self.fix_subvolume_mode {
            std::fs::set_permissions(&subvolume, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| err!("{}: {e}", subvolume.display()))?;
        }
        if self.create_mount_point {
            std::fs::create_dir(&c.snapshots_dir).map_err(|e| err!("{}: {e}", c.snapshots_dir.display()))?;
        }
        if self.fstab_line.is_some() || self.fstab_without_subvolids.is_some() {
            let current = std::fs::read_to_string(FSTAB).map_err(|e| err!("{FSTAB}: {e}"))?;
            std::fs::copy(FSTAB, format!("{FSTAB}{FSTAB_BACKUP_SUFFIX}"))
                .map_err(|e| err!("{FSTAB}: {e}"))?;
            let mut text = match without_subvolids(&current) {
                Some((text, _)) if self.fstab_without_subvolids.is_some() => text,
                _ => current,
            };
            if let Some(line) = &self.fstab_line {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(line);
                text.push('\n');
            }
            let mode = std::fs::metadata(FSTAB).map(|m| m.mode() & 0o777).unwrap_or(0o644);
            system::write_atomic(Path::new(FSTAB), text.as_bytes(), Some(mode))?;
            // systemd turns fstab into mount units; reload so they match the file.
            run("systemctl", &["daemon-reload"])?;
        }
        if self.mount {
            run("mount", &[&c.snapshots_dir.to_string_lossy()])?;
        }
        // Last, so the first scheduled run finds the store mounted.
        if self.enable_timer {
            run("systemctl", &["enable", "--now", TIMER])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FSTAB: &str = "\
# /dev/mapper/cryptroot
UUID=91e8\t/         \tbtrfs     \trw,noatime,compress=zstd:1,ssd,space_cache=v2,subvolid=256,subvol=/@\t0 0

UUID=91e8\t/home     \tbtrfs     \trw,noatime,compress=zstd:1,ssd,space_cache=v2,subvolid=257,subvol=/@home\t0 0
UUID=AB12\t/boot     \tvfat      \trw,relatime,fmask=0077\t0 2
";

    #[test]
    fn subvolids_go_and_nothing_else_moves() {
        let (text, changed) = without_subvolids(FSTAB).unwrap();
        assert_eq!(changed, ["/", "/home"]);
        assert!(!text.contains("subvolid="));
        assert!(text.contains(
            "UUID=91e8\t/         \tbtrfs     \trw,noatime,compress=zstd:1,ssd,space_cache=v2,subvol=/@\t0 0"
        ));
        assert_eq!(text.lines().count(), FSTAB.lines().count());
        assert!(text.ends_with('\n'));
        assert!(without_subvolids(&text).is_none(), "and a second run changes nothing");
    }

    #[test]
    fn a_subvolid_alone_is_left_alone() {
        // Without subvol= the ID is the only name the line has.
        assert!(without_subvolids("UUID=x / btrfs rw,subvolid=256 0 0\n").is_none());
    }

    #[test]
    fn finding_lines_and_options() {
        let root = fstab_entry(FSTAB, Path::new("/")).unwrap();
        assert_eq!(root[2], "btrfs");
        assert!(fstab_entry(FSTAB, Path::new("/.snapshots")).is_none());
        assert_eq!(
            snapshot_mount_options(Some(&root), "@snapshots"),
            "subvol=/@snapshots,rw,noatime,compress=zstd:1,ssd,space_cache=v2"
        );
        assert_eq!(snapshot_mount_options(None, "@snapshots"), "subvol=/@snapshots,noatime");
    }
}
