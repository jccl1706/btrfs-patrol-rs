// SPDX-License-Identifier: GPL-3.0-or-later
//! Snapshots around pacman transactions.
//!
//! Two ALPM hooks (data/alpm/) run `btrfs-patrol pacman-hook pre` before
//! each transaction and `... post` after it, with NeedsTargets: the package
//! names arrive on stdin, one per line. pacman says nothing about what it
//! is doing to them, so the command line is read from the pacman process
//! itself, and a snapshot says "pacman -Syu: linux, mesa, firefox +31 more".
//!
//! A snapshot problem must never fail a transaction: the hooks have no
//! AbortOnFail, and this always exits 0, saying what went wrong instead.

use std::io::Read;

use crate::App;
use crate::manage::{selected_subvolumes, take_snapshots};

const NAMES: usize = 3;
const COMMAND_WIDTH: usize = 40;

/// "pacman -Syu: linux, mesa, firefox +31 more"
pub fn describe(command: Option<&str>, targets: &[String]) -> String {
    let what = command.unwrap_or("pacman transaction");
    if targets.is_empty() {
        return what.to_owned();
    }
    let mut names = targets.iter().take(NAMES).cloned().collect::<Vec<_>>().join(", ");
    if targets.len() > NAMES {
        names.push_str(&format!(" +{} more", targets.len() - NAMES));
    }
    format!("{what}: {names}")
}

/// The pacman command line this hook runs under, found by walking up the
/// process tree: "pacman -Syu", or "pacman -S --needed git" from an AUR helper.
pub fn pacman_command() -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..6 {
        pid = parent(pid)?;
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        if comm.trim() == "pacman" {
            let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            return Some(shorten(&raw));
        }
    }
    None
}

fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ...", and comm may hold spaces and parentheses.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// argv as one line: the program's base name, then the arguments, cut short.
fn shorten(cmdline: &[u8]) -> String {
    let mut args =
        cmdline.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned());
    let program = args.next().map(|p| p.rsplit('/').next().unwrap_or(&p).to_owned()).unwrap_or_default();
    let line = std::iter::once(program).chain(args).collect::<Vec<_>>().join(" ");
    crate::table::truncate(&crate::table::printable(&line), COMMAND_WIDTH)
}

pub fn run(app: &App, when: &str) -> u8 {
    let wanted = match when {
        "pre" => app.config.pacman_pre_snapshot,
        _ => app.config.pacman_post_snapshot,
    };
    if !wanted {
        return 0;
    }
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let targets: Vec<String> =
        input.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect();
    let description = describe(pacman_command().as_deref(), &targets);
    let kind = format!("pacman-{when}");
    let result = selected_subvolumes(&app.config, &[], &kind)
        .and_then(|subvolumes| take_snapshots(app, &subvolumes, &kind, &description, false));
    match result {
        Ok(taken) => {
            for name in &taken.skipped {
                println!("btrfs-patrol: a rollback of {name} is waiting for a reboot; no snapshot taken");
            }
            for s in &taken.created {
                let kernel =
                    s.boot.as_ref().map(|b| format!(", kernel {} stored", b.version)).unwrap_or_default();
                println!("btrfs-patrol: snapshot {} of {}{kernel}", s.id, s.subvolume);
            }
            for s in &taken.pruned {
                println!("btrfs-patrol: pruned snapshot {}", s.id);
            }
            for warning in &taken.warnings {
                eprintln!("btrfs-patrol: warning: {warning}");
            }
        }
        Err(e) => eprintln!("btrfs-patrol: no snapshot taken: {e}"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(describe(Some("pacman -Syu"), &names(&["linux", "mesa"])), "pacman -Syu: linux, mesa");
        assert_eq!(
            describe(Some("pacman -Syu"), &names(&["a", "b", "c", "d", "e"])),
            "pacman -Syu: a, b, c +2 more"
        );
        assert_eq!(describe(None, &names(&["git"])), "pacman transaction: git");
        assert_eq!(describe(Some("pacman -Sc"), &[]), "pacman -Sc");
    }

    #[test]
    fn command_lines() {
        assert_eq!(shorten(b"/usr/bin/pacman\0-Syu\0"), "pacman -Syu");
        assert_eq!(
            shorten(b"pacman\0-S\0--needed\0--noconfirm\0a-very-long-package-name\0another\0"),
            "pacman -S --needed --noconfirm a-very-l…"
        );
        assert_eq!(shorten(b"pacman\0-S\0evil\x1b[2J\0"), "pacman -S evil [2J");
    }

    #[test]
    fn our_own_parent_is_found() {
        assert!(parent(std::process::id()).is_some_and(|p| p > 0));
    }
}
