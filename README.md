# btrfs-patrol-rs

**A btrfs snapshot manager and rollback tool for Arch Linux, in Rust, with a
[ratatui](https://ratatui.rs) terminal interface.**

A rewrite of [btrfs-patrol](https://github.com/jccl1706/btrfs-patrol) (Python,
for Fedora) for Arch: snapshots before every pacman transaction and once a
day, a full-screen browser for them, and one checked command to put the
system back.

> **Status: early development - not usable yet.** Nothing here touches a
> system until the phase that needs it is finished and tested in a VM.
> Snapshots are not backups.

## Target layout

The layout [archydots](https://github.com/jccl1706/archydots)' installer
creates, and the common Arch setup it follows:

- btrfs (optionally on LUKS2) with subvolumes `@`, `@home`, `@snapshots`
  (mounted at `/.snapshots`), `@var_log`, `@var_cache`
- root found by name: `rootflags=subvol=@` on the kernel command line
- **systemd-boot, with the kernel and initramfs on the ESP** (`/boot` is vfat,
  outside btrfs)

That last point is the big difference from Fedora: the ESP only holds the
kernel that is installed *now*. A rollback to a snapshot from before a kernel
update would boot the new kernel with the old modules. So btrfs-patrol-rs
**stores the kernel and initramfs with each snapshot** (deduplicated) and puts
the matching pair back on the ESP when it rolls back - which also makes
"boot a snapshot from the menu" possible.

## Plan

1. **Read-only** - the snapshot store, selectors, `list`, `check`, `diff`,
   and the TUI (browse, detail, filter, compare).
2. **Taking snapshots** - `snapshot`, `describe`, `keep`, `delete`, `prune`,
   `setup`, the daily systemd timer, pacman (ALPM) hooks, a PKGBUILD.
3. **Rollback** - with kernels stored per snapshot, and systemd-boot entries
   to boot a snapshot. Tested in a VM with the same layout before anything
   else.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE](NOTICE) for credits.
