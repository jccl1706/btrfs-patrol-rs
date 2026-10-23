# btrfs-patrol-rs

**A btrfs snapshot manager and rollback tool for Arch Linux, in Rust, with a
[ratatui](https://ratatui.rs) terminal interface.**

A rewrite of [btrfs-patrol](https://github.com/jccl1706/btrfs-patrol) (Python,
for Fedora) for Arch: snapshots before every pacman transaction and once a
day, a full-screen browser for them, and one checked command to put the
system back.

> **Status: all three phases done.** It takes snapshots - by hand, before
> every pacman transaction, once a day - with the kernel that goes with
> each, and rolls back to them, kernel included, from the running system or
> from a snapshot booted in the boot menu. Tested on a VM with this layout,
> across a kernel change in both directions. Snapshots are not backups.

## Install

```sh
git clone https://github.com/jccl1706/btrfs-patrol-rs
cd btrfs-patrol-rs/packaging/arch
makepkg -si                    # builds, runs the tests, installs btrfs-patrol-git
sudo btrfs-patrol setup        # once: shows its plan, then asks
```

`setup` only does what is missing: on a standard layout (with `@snapshots`
already mounted at `/.snapshots`) that is making the store readable only by
root and enabling the daily timer. It can also create and mount
`@snapshots`, and it removes `subvolid=` from fstab, which would silently
undo a rollback. `--dry-run` shows the plan and changes nothing.

No configuration is needed on this layout. To change something - boot
entries for snapshots, retention, other subvolumes - start from the
commented example:

```sh
sudo btrfs-patrol example-config | sudo tee /etc/btrfs-patrol/config.toml
sudo btrfs-patrol check
```

From then on a snapshot is taken **before every pacman transaction** -
described as `pacman -Syu: linux, mesa, firefox +31 more` - and **once a
day**, and the oldest are pruned beyond `max_snapshots` (kept ones never).

## Use

```sh
sudo btrfs-patrol tui                      # the dashboard: browse, take, keep, compare, delete
sudo btrfs-patrol snapshot -d "before kde" # take one now (-k to keep it, -s home for one subvolume)
sudo btrfs-patrol list                     # every snapshot
sudo btrfs-patrol list kind=pacman         # ... or those a selector matches
sudo btrfs-patrol diff 12 15               # what changed between two snapshots
sudo btrfs-patrol keep 12                  # never prune it (unkeep to undo)
sudo btrfs-patrol describe 12 "known good"
sudo btrfs-patrol delete 3-9               # asks first; -y to skip
sudo btrfs-patrol prune                    # apply max_snapshots now
btrfs-patrol check                         # does the layout match the configuration?
```

The store is readable only by root, so everything but `check` needs sudo.

## Rolling back

```sh
sudo btrfs-patrol rollback 24 --dry-run   # every check, the plan, nothing changed
sudo btrfs-patrol rollback 24             # the plan, then asks
sudo reboot
```

The current root is kept as a new snapshot (kind `rollback`, kept), and a
writable copy of snapshot 24 takes its place as `@`; subvolumes nested in it
(systemd's `var/lib/machines`) move across. If the snapshot was taken on
another kernel, **its stored kernel and initramfs go back on the ESP** -
and the kernel you leave is stored with the saved state, so `rollback` to
that one undoes it all. `/home`, `/var/log` and `/var/cache` are their own
subvolumes and are not touched. Every step is undone if a later one fails;
nothing reboots by itself. In the TUI: `R`, read the plan, `y`.

After rolling back across a kernel change, `sudo mkinitcpio -P` rebuilds the
fallback initramfs for the restored kernel (rollback reminds you).

### When the system won't start

Set `[boot] entries = 3` in `/etc/btrfs-patrol/config.toml`, and the three
newest root snapshots appear in the systemd-boot menu, below Arch's own
entries, each with its own kernel (copied to `/boot/btrfs-patrol/`). Boot
one - it runs read-only - and roll back from there:

```sh
sudo btrfs-patrol rollback 28    # the snapshot you booted, or any other
sudo reboot                      # into the normal entry
```

A snapshot booted this way reports itself as *degraded*: services that
write to `/` fail on a read-only root (pacman's gpg-agent sockets,
tmpfiles, lastlog). That is expected; it is there to roll back from.
`systemctl reboot --boot-loader-entry='~btrfs-patrol-28.conf'` boots one
without catching the menu.

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
- `n` new snapshot, `d` describe, `p` keep (pin), `x` delete (then `y`)
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
2. ✅ **Taking snapshots** - `snapshot`, `describe`, `keep`, `delete`,
   `prune`, kernels stored per snapshot, `setup`, the daily systemd timer,
   pacman (ALPM) hooks, the TUI's actions, a PKGBUILD.
3. ✅ **Rollback** - with the stored kernel put back on the ESP, undoable,
   from the running system or a snapshot's own boot entry; `R` in the TUI.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE) and [NOTICE](NOTICE) for credits.
