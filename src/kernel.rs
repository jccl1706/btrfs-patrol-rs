// SPDX-License-Identifier: GPL-3.0-or-later
//! Kernels stored with snapshots.
//!
//! On Arch the kernel and initramfs live on the ESP (/boot, vfat), outside
//! every btrfs snapshot, and the ESP only ever holds the kernel installed
//! NOW. Roll back to a snapshot from before a kernel update and the new
//! kernel would boot a system that only has the old kernel's modules - no
//! drivers, perhaps no boot. So each root snapshot records which kernel
//! matches its modules, and a copy of it and its initramfs goes in the
//! store's `.boot/` directory, named by SHA-256 so that the many snapshots
//! sharing one kernel share one copy.
//!
//! "Matches" is checked, not assumed: the version is read out of the kernel
//! image itself (the bzImage header) and must be a directory in
//! /usr/lib/modules. A kernel updated without a reboot is still handled -
//! the ESP kernel and the modules on disk are the new pair, whatever is
//! running.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::snapshot::{BOOT_DIR, BootFiles, Snapshot, Store};
use crate::{bail, err};

pub const ESP: &str = "/boot";
pub const MODULES: &str = "/usr/lib/modules";

/// A kernel on the ESP and the module directory that belongs to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EspKernel {
    /// The package it comes from, which names its files: linux, linux-lts.
    pub pkgbase: String,
    pub version: String,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
}

/// The release in an x86 bzImage's header: the "HdrS" setup header has, at
/// 0x20E, a pointer (from 0x200) to "7.2.11-arch1-1 (linux@archlinux) #1 ...".
pub fn image_version(header: &[u8]) -> Option<String> {
    if header.get(0x202..0x206)? != b"HdrS" {
        return None;
    }
    let pointer = u16::from_le_bytes([*header.get(0x20E)?, *header.get(0x20F)?]) as usize + 0x200;
    let rest = header.get(pointer..)?;
    let end = rest.iter().position(|&b| b == 0 || b == b' ')?;
    let version = std::str::from_utf8(&rest[..end]).ok()?;
    (!version.is_empty()).then(|| version.to_owned())
}

fn read_version(kernel: &Path) -> Option<String> {
    let mut header = vec![0; 64 * 1024];
    let mut file = File::open(kernel).ok()?;
    let n = file.read(&mut header).ok()?;
    image_version(&header[..n])
}

fn valid_pkgbase(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
}

/// Every kernel on `esp` whose modules are in `modules`, the running one's
/// package first, then "linux", then the rest by name.
pub fn esp_kernels(esp: &Path, modules: &Path, running: &str) -> Vec<EspKernel> {
    let Ok(entries) = std::fs::read_dir(modules) else { return Vec::new() };
    let mut found: Vec<EspKernel> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let version = entry.file_name().into_string().ok()?;
            let pkgbase = std::fs::read_to_string(entry.path().join("pkgbase")).ok()?.trim().to_owned();
            if !valid_pkgbase(&pkgbase) {
                return None;
            }
            let kernel = esp.join(format!("vmlinuz-{pkgbase}"));
            let initramfs = esp.join(format!("initramfs-{pkgbase}.img"));
            (initramfs.is_file() && read_version(&kernel)? == version).then_some(EspKernel {
                pkgbase,
                version,
                kernel,
                initramfs,
            })
        })
        .collect();
    let running_pkgbase = std::fs::read_to_string(modules.join(running).join("pkgbase"))
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    found.sort_by_key(|k| (k.pkgbase != running_pkgbase, k.pkgbase != "linux", k.pkgbase.clone()));
    found
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copy `source` into the store's .boot directory under its SHA-256, unless
/// a copy is there already, and return the hash. The copy is hashed as it is
/// written, and only renamed into place once complete and on disk.
pub fn put(store: &Store, source: &Path) -> Result<String> {
    let dir = store.directory.join(BOOT_DIR);
    let io = |path: &Path, e: std::io::Error| err!("{}: {e}", path.display());
    if !dir.is_dir() {
        std::fs::create_dir(&dir).map_err(|e| io(&dir, e))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| io(&dir, e))?;
    }
    let incoming = dir.join(format!(".incoming-{}", std::process::id()));
    let mut input = File::open(source).map_err(|e| io(source, e))?;
    let mut output = File::create(&incoming).map_err(|e| io(&incoming, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    let result = (|| -> Result<String> {
        loop {
            let n = input.read(&mut buffer).map_err(|e| io(source, e))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            output.write_all(&buffer[..n]).map_err(|e| io(&incoming, e))?;
        }
        output.sync_all().map_err(|e| io(&incoming, e))?;
        Ok(hex(&hasher.finalize()))
    })();
    let hash = match result {
        Ok(hash) => hash,
        Err(e) => {
            let _ = std::fs::remove_file(&incoming);
            return Err(e);
        }
    };
    let target = dir.join(&hash);
    if target.is_file() {
        std::fs::remove_file(&incoming).map_err(|e| io(&incoming, e))?;
    } else {
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io(&incoming, e))?;
        std::fs::rename(&incoming, &target).map_err(|e| io(&target, e))?;
    }
    Ok(hash)
}

/// Store the kernel matching the modules on disk now, for a root snapshot
/// being taken now.
pub fn store_current(store: &Store, esp: &Path, modules: &Path, running: &str) -> Result<BootFiles> {
    let Some(kernel) = esp_kernels(esp, modules, running).into_iter().next() else {
        bail!(
            "no kernel on {} matches a module directory in {}; the snapshot has no stored kernel",
            esp.display(),
            modules.display()
        );
    };
    Ok(BootFiles {
        kernel: put(store, &kernel.kernel)?,
        initramfs: put(store, &kernel.initramfs)?,
        pkgbase: kernel.pkgbase,
        version: kernel.version,
    })
}

/// Delete stored boot files no snapshot names any more, and returns how
/// many bytes that freed. Call it holding the lock.
///
/// Only when every snapshot's metadata could be read: a snapshot whose
/// info.json is unreadable might name a file, and deleting that would take
/// its kernel with it.
pub fn collect_garbage(store: &Store) -> Result<u64> {
    let dir = store.directory.join(BOOT_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(0) };
    let mut used = BTreeSet::new();
    for id in store.ids()? {
        let Ok(snapshot) = store.load(id) else { return Ok(0) };
        used.extend(referenced(&snapshot));
    }
    let mut freed = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        // .incoming-<pid> files are copies a crash interrupted; only ours are
        // safe to take, as another run may be copying right now - and it
        // holds the lock we hold, so there is none.
        if !used.contains(&name) {
            freed += entry.metadata().map(|m| m.len()).unwrap_or(0);
            std::fs::remove_file(entry.path()).map_err(|e| err!("{}: {e}", entry.path().display()))?;
        }
    }
    Ok(freed)
}

fn referenced(snapshot: &Snapshot) -> Vec<String> {
    snapshot.boot.iter().flat_map(|b| [b.kernel.clone(), b.initramfs.clone()]).collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::snapshot::tests::snapshot;

    /// Enough of a bzImage for its version to be read.
    pub fn fake_kernel(version: &str) -> Vec<u8> {
        let mut image = vec![0u8; 0x4000];
        image[0x202..0x206].copy_from_slice(b"HdrS");
        image[0x20E..0x210].copy_from_slice(&(0x3000u16 - 0x200).to_le_bytes());
        let text = format!("{version} (linux@archlinux) #1 SMP");
        image[0x3000..0x3000 + text.len()].copy_from_slice(text.as_bytes());
        image
    }

    /// An ESP and a modules directory with the given (pkgbase, version on
    /// the ESP, version of the modules).
    pub fn system(dir: &Path, kernels: &[(&str, &str, &str)]) -> (PathBuf, PathBuf) {
        let (esp, modules) = (dir.join("boot"), dir.join("modules"));
        std::fs::create_dir_all(&esp).unwrap();
        for (pkgbase, esp_version, module_version) in kernels {
            std::fs::write(esp.join(format!("vmlinuz-{pkgbase}")), fake_kernel(esp_version)).unwrap();
            std::fs::write(esp.join(format!("initramfs-{pkgbase}.img")), format!("initramfs {esp_version}"))
                .unwrap();
            let m = modules.join(module_version);
            std::fs::create_dir_all(&m).unwrap();
            std::fs::write(m.join("pkgbase"), format!("{pkgbase}\n")).unwrap();
        }
        (esp, modules)
    }

    #[test]
    fn versions_from_the_image_header() {
        assert_eq!(image_version(&fake_kernel("7.2.11-arch1-1")).as_deref(), Some("7.2.11-arch1-1"));
        assert_eq!(image_version(b"not a kernel"), None);
        let mut broken = fake_kernel("x");
        broken[0x20E] = 0xff;
        broken[0x20F] = 0xff;
        assert_eq!(image_version(&broken), None);
    }

    #[test]
    fn only_kernels_whose_modules_are_there() {
        let dir = tempfile::tempdir().unwrap();
        let (esp, modules) = system(
            dir.path(),
            &[("linux", "7.2.11-arch1-1", "7.2.11-arch1-1"), ("linux-lts", "6.12.50-1-lts", "6.12.49-1-lts")],
        );
        let found = esp_kernels(&esp, &modules, "7.2.11-arch1-1");
        assert_eq!(found.len(), 1, "linux-lts's ESP image is not its modules' version");
        assert_eq!((found[0].pkgbase.as_str(), found[0].version.as_str()), ("linux", "7.2.11-arch1-1"));
    }

    #[test]
    fn the_running_kernels_package_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let (esp, modules) = system(
            dir.path(),
            &[("linux", "7.2.11-arch1-1", "7.2.11-arch1-1"), ("linux-lts", "6.12.50-1-lts", "6.12.50-1-lts")],
        );
        assert_eq!(esp_kernels(&esp, &modules, "6.12.50-1-lts")[0].pkgbase, "linux-lts");
        assert_eq!(esp_kernels(&esp, &modules, "gone")[0].pkgbase, "linux");
    }

    #[test]
    fn hostile_pkgbase_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (esp, modules) = system(dir.path(), &[("linux", "7.2.11-arch1-1", "7.2.11-arch1-1")]);
        std::fs::write(modules.join("7.2.11-arch1-1/pkgbase"), "../../etc/x").unwrap();
        assert!(esp_kernels(&esp, &modules, "x").is_empty());
    }

    #[test]
    fn stored_once_and_collected_when_unused() {
        let dir = tempfile::tempdir().unwrap();
        let (esp, modules) = system(dir.path(), &[("linux", "7.2.11-arch1-1", "7.2.11-arch1-1")]);
        let store_dir = dir.path().join("store");
        std::fs::create_dir(&store_dir).unwrap();
        let store = Store::new(&store_dir);
        let first = store_current(&store, &esp, &modules, "7.2.11-arch1-1").unwrap();
        let second = store_current(&store, &esp, &modules, "7.2.11-arch1-1").unwrap();
        assert_eq!(first, second);
        assert_eq!((first.pkgbase.as_str(), first.version.as_str()), ("linux", "7.2.11-arch1-1"));
        let boot = store_dir.join(BOOT_DIR);
        assert_eq!(std::fs::read_dir(&boot).unwrap().count(), 2, "one kernel, one initramfs");
        assert_eq!(std::fs::read(boot.join(&first.kernel)).unwrap(), fake_kernel("7.2.11-arch1-1"));

        // Referenced by a snapshot: kept.
        let mut s = snapshot(1);
        s.boot = Some(first.clone());
        std::fs::create_dir(store.path(1)).unwrap();
        store.save(&s).unwrap();
        std::fs::write(boot.join(".incoming-99"), "half a copy").unwrap();
        assert_eq!(collect_garbage(&store).unwrap(), 11);
        assert_eq!(std::fs::read_dir(&boot).unwrap().count(), 2);

        // No longer referenced, but an unreadable snapshot might name them:
        // nothing is touched.
        s.boot = None;
        store.save(&s).unwrap();
        std::fs::create_dir(store.path(2)).unwrap();
        assert_eq!(collect_garbage(&store).unwrap(), 0);
        assert_eq!(std::fs::read_dir(&boot).unwrap().count(), 2);

        // Once every snapshot can be read: collected.
        std::fs::remove_dir(store.path(2)).unwrap();
        assert!(collect_garbage(&store).unwrap() > 0);
        assert_eq!(std::fs::read_dir(&boot).unwrap().count(), 0);
    }

    #[test]
    fn no_matching_kernel_is_an_error_not_a_guess() {
        let dir = tempfile::tempdir().unwrap();
        let (esp, modules) = system(dir.path(), &[("linux", "7.2.12-arch1-1", "7.2.11-arch1-1")]);
        let store = Store::new(dir.path());
        assert!(store_current(&store, &esp, &modules, "7.2.11-arch1-1").unwrap_err().0.contains("no kernel"));
    }
}
