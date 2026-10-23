# btrfs-patrol-rs

**A btrfs snapshot manager and rollback tool for Arch Linux, in Rust, with a
[ratatui](https://ratatui.rs) terminal interface.**

A rewrite of [btrfs-patrol](https://github.com/jccl1706/btrfs-patrol) (Python,
for Fedora) for Arch: snapshots before every pacman transaction and once a
day, a full-screen browser for them, and one checked command to put the
system back.

> **Status: phase 1 of 3 - read-only.** It lists, filters, compares and
> browses snapshots, and checks the setup; it does not take or change any
> yet. Nothing here touches a system until the phase that needs it is
> finished and tested in a VM. Snapshots are not backups.

## Use

```sh
cargo build --release          # target/release/btrfs-patrol

btrfs-patrol check             # does the layout match the configuration?
btrfs-patrol list              # every snapshot
btrfs-patrol list kind=pacman  # ... or those a selector matches
sudo btrfs-patrol diff 12 15   # what changed between two snapshots
btrfs-patrol tui               # browse them full-screen
```

No configuration is needed on the standard layout below; `btrfs-patrol
example-config` prints the file with its defaults, to copy to
`/etc/btrfs-patrol/config.toml` and adjust.

Selectors: `3`, `1,10,20-23`, or one of `date=2026-10`, `time=16:`,
`kernel=7.2`, `kind=pacman-pre`, `description=mesa`, `subvolume=home`,
`keep=yes`.

### The TUI

A dashboard in Tokyo Night: the snapshot list with an icon for each kind
(pacman, timer, manual, rollback) and a lock on kept ones, a detail panel
beside it (under it on narrow terminals), and the disk's use in the header.

- `↑↓` move, `Tab` / `←→` switch subvolume, `Enter` full details, `?` help
- `/` search as you type: plain text is fuzzy (`msa upg` finds "upgrade
  mesa"), and a selector (`kind=pacman`, `keep=yes`, `10-20`) works as on
  the command line
- `c` on one snapshot, then `c` on another: what changed between them, as
  a foldable tree grouped by folder (`←→` fold, `+`/`-` all)
- the mouse: click a row (twice to open it), click a tab, scroll the wheel
- run as root, it measures how much space each snapshot holds on its own -
  what deleting it would free - in the background
- it reloads by itself when snapshots come and go

Nerd Font icons need a Nerd Font in the terminal; the Linux console gets
letters, and a non-UTF-8 locale gets plain ASCII.

### Compatible with btrfs-patrol

The store is the same - `/.snapshots/<id>/info.json` and
`/.snapshots/<id>/snapshot` - so either tool reads the other's snapshots.
One field is new: `boot`, naming the kernel and initramfs stored with a
snapshot in `/.snapshots/.boot/`, which btrfs-patrol ignores.

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

1. ✅ **Read-only** - the snapshot store, selectors, `list`, `check`,
   `diff`, and the TUI (browse, detail, filter, compare).
2. **Taking snapshots** - `snapshot`, `describe`, `keep`, `delete`, `prune`,
   `setup`, the daily systemd timer, pacman (ALPM) hooks, a PKGBUILD.
3. **Rollback** - with kernels stored per snapshot, and systemd-boot entries
   to boot a snapshot. Tested in a VM with the same layout before anything
   else.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE](NOTICE) for credits.
