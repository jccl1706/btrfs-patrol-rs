// SPDX-License-Identifier: GPL-3.0-or-later
//! What changed between two snapshots, asked of btrfs rather than worked out:
//!
//! ```text
//! btrfs send --no-data -p <older> <newer> | btrfs receive --dump
//! ```
//!
//! btrfs already knows which extents and metadata differ between two
//! snapshots that share a parent, so it says what changed without reading a
//! single file; `--no-data` keeps file contents out of the stream. Both
//! snapshots must be read-only (they are) and it needs root.
//!
//! THE STREAM IS NOT A DIFF. It is a recipe for turning the older snapshot
//! into the newer one, and three things in it have to be undone:
//!
//! - Paths are prefixed with the subvolume's directory: "./snapshot/etc/x".
//! - A NEW FILE IS CREATED UNDER A TEMPORARY NAME, o<inode>-<transid>-<n>,
//!   and only given its real name by a later rename.
//! - A RENAME COMES THROUGH AS link + unlink, and the link's dest= is the one
//!   path in the stream WITHOUT the "./snapshot/" prefix.
//!
//! tests/data keeps a stream taken across known changes.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::Result;
use crate::{bail, err};

/// Commands that bring something into existence, under a temporary name.
const CREATES: [&str; 6] = ["mkfile", "mkdir", "mknod", "mkfifo", "mksock", "symlink"];
/// Commands that change a file's contents (update_extent with --no-data).
const CONTENT: [&str; 4] = ["write", "update_extent", "truncate", "clone"];
/// Commands that change metadata in a way someone meant.
const METADATA: [&str; 4] = ["chmod", "chown", "set_xattr", "remove_xattr"];
// utimes is not a change by itself: running a program updates its atime, and
// a directory's mtime moves because a child changed - which is already listed.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Added,
    Removed,
    Modified,
    Renamed,
    Attrs,
}

impl Change {
    pub const ALL: [Change; 5] =
        [Change::Added, Change::Removed, Change::Modified, Change::Renamed, Change::Attrs];

    pub fn marker(self) -> char {
        match self {
            Change::Added => '+',
            Change::Removed => '-',
            Change::Modified => '~',
            Change::Renamed => '>',
            Change::Attrs => 'a',
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Removed => "removed",
            Change::Modified => "modified",
            Change::Renamed => "renamed",
            Change::Attrs => "attrs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub change: Change,
    pub path: String,
    /// Where it came from, for a rename.
    pub old_path: String,
}

impl Entry {
    pub fn describe(&self) -> String {
        match self.change {
            Change::Renamed => format!("{} {} -> {}", self.change.marker(), self.old_path, self.path),
            _ => format!("{} {}", self.change.marker(), self.path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub older_id: u64,
    pub newer_id: u64,
    pub entries: Vec<Entry>,
}

impl Comparison {
    pub fn of(&self, change: Change) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.change == change)
    }

    pub fn summary(&self) -> String {
        let parts: Vec<String> = Change::ALL
            .iter()
            .map(|&c| (c, self.of(c).count()))
            .filter(|(_, n)| *n > 0)
            .map(|(c, n)| format!("{n} {}", c.name()))
            .collect();
        if parts.is_empty() { "no differences".into() } else { parts.join(", ") }
    }

    /// Every change, grouped and in a stable order.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for change in Change::ALL {
            let mut found: Vec<&Entry> = self.of(change).collect();
            found.sort_by(|a, b| a.path.cmp(&b.path));
            out.extend(found.iter().map(|e| e.describe()));
        }
        out
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offsets of each `key=` in `text`: a run of word characters
/// immediately followed by '='.
fn key_starts(text: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let chars = text.char_indices();
    let mut word_start: Option<usize> = None;
    for (i, c) in chars {
        if is_word(c) {
            word_start.get_or_insert(i);
            continue;
        }
        if c == '='
            && let Some(start) = word_start
        {
            found.push((start, i + 1));
        }
        word_start = None;
    }
    found
}

/// The key=value tail of a dump line. Values can hold spaces - an SELinux
/// xattr does - so each value runs to the NEXT key, not to whitespace.
pub fn parse_fields(rest: &str) -> BTreeMap<String, String> {
    let keys = key_starts(rest);
    keys.iter()
        .enumerate()
        .map(|(n, &(start, value_start))| {
            let end = keys.get(n + 1).map_or(rest.len(), |k| k.0);
            (rest[start..value_start - 1].to_owned(), rest[value_start..end].trim().to_owned())
        })
        .collect()
}

/// "./snapshot/difftest/added.txt" -> "difftest/added.txt"; the subvolume's
/// own line is "" once stripped.
pub fn strip_subvolume(path: &str) -> &str {
    let cleaned = path.strip_prefix("./").unwrap_or(path);
    cleaned.split_once('/').map_or("", |(_, tail)| tail)
}

fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('o') else { return false };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Where the fields start: the first whitespace run followed by `key=`.
fn fields_start(rest: &str) -> Option<usize> {
    let keys = key_starts(rest);
    keys.iter().find_map(|&(start, _)| {
        let before = &rest[..start];
        let trimmed = before.trim_end();
        (trimmed.len() < before.len()).then_some(trimmed.len())
    })
}

/// Dump text to (command, path, fields), with paths still as written.
pub fn parse(dump: &str) -> Vec<(String, String, BTreeMap<String, String>)> {
    dump.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let line = line.trim_start();
            let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim_start();
            match fields_start(rest) {
                Some(at) => (command.to_owned(), rest[..at].trim().to_owned(), parse_fields(&rest[at..])),
                None => (command.to_owned(), rest.trim().to_owned(), BTreeMap::new()),
            }
        })
        .collect()
}

/// Turn a receive --dump stream into what actually changed.
pub fn interpret(dump: &str) -> Vec<Entry> {
    let commands = parse(dump);
    let dest = |fields: &BTreeMap<String, String>| fields.get("dest").cloned().unwrap_or_default();

    // Pass one: what each temporary name ends up being called.
    let mut final_name: BTreeMap<String, String> = BTreeMap::new();
    for (command, path, fields) in &commands {
        let source = strip_subvolume(path);
        if command == "rename" && is_temp_name(source) {
            final_name.insert(source.to_owned(), strip_subvolume(&dest(fields)).to_owned());
        }
    }

    let mut created = BTreeSet::new();
    let mut removed = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut attrs = BTreeSet::new();
    let mut renamed: BTreeMap<String, String> = BTreeMap::new();

    for (command, raw_path, fields) in &commands {
        let stripped = strip_subvolume(raw_path);
        let path = final_name.get(stripped).cloned().unwrap_or_else(|| stripped.to_owned());
        let command = command.as_str();
        if command == "snapshot" || path.is_empty() {
            continue;
        }
        if CREATES.contains(&command) {
            created.insert(path);
        } else if command == "link" {
            // The rename half that names the new path. Its dest is the OLD
            // path, and it is the one path in the stream with no prefix.
            let old = dest(fields);
            renamed.insert(old.strip_prefix("./").unwrap_or(&old).to_owned(), path);
        } else if command == "unlink" || command == "rmdir" {
            removed.insert(path);
        } else if CONTENT.contains(&command) {
            modified.insert(path);
        } else if METADATA.contains(&command) {
            attrs.insert(path);
        } else if command == "rename" && !is_temp_name(stripped) {
            renamed.insert(stripped.to_owned(), strip_subvolume(&dest(fields)).to_owned());
        }
    }

    // A renamed file is a link to the new name and an unlink of the old one;
    // neither half is an addition or a deletion on its own.
    for (old, new) in &renamed {
        removed.remove(old);
        created.remove(new);
        modified.remove(new);
        attrs.remove(new);
    }
    // Something created in this window was written, not modified.
    let modified: BTreeSet<String> = modified.difference(&created).cloned().collect();
    attrs.retain(|p| !created.contains(p) && !modified.contains(p) && !removed.contains(p));
    let removed: BTreeSet<String> = removed.difference(&created).cloned().collect();

    let entry = |change, path: String| Entry { change, path, old_path: String::new() };
    let mut entries: Vec<Entry> = created.into_iter().map(|p| entry(Change::Added, p)).collect();
    entries.extend(removed.into_iter().map(|p| entry(Change::Removed, p)));
    entries.extend(modified.into_iter().map(|p| entry(Change::Modified, p)));
    entries.extend(renamed.into_iter().map(|(old, new)| Entry {
        change: Change::Renamed,
        path: new,
        old_path: old,
    }));
    entries.extend(attrs.into_iter().map(|p| entry(Change::Attrs, p)));
    entries
}

/// Run btrfs and interpret what it says about two snapshot subvolumes.
pub fn compare(older: &Path, newer: &Path, older_id: u64, newer_id: u64) -> Result<Comparison> {
    for path in [older, newer] {
        if !path.is_dir() {
            bail!("{} is not there; the snapshot has no subvolume", path.display());
        }
    }
    let missing = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => err!("btrfs not found; install btrfs-progs"),
        _ => err!("btrfs: {e}"),
    };
    let mut send = Command::new("btrfs")
        .args(["send", "--no-data", "-p"])
        .args([older, newer])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(missing)?;
    // send's stderr is drained on its own thread: if it filled its pipe while
    // receive was being waited on, the two would wait on each other for ever.
    let mut send_stderr = send.stderr.take().expect("piped");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = send_stderr.read_to_string(&mut text);
        text
    });
    let receive =
        Command::new("btrfs").args(["receive", "--dump"]).stdin(send.stdout.take().expect("piped")).output();
    let send_status = send.wait().map_err(|e| err!("btrfs send: {e}"))?;
    let send_error = errors.join().unwrap_or_default();
    let receive = receive.map_err(missing)?;
    if !send_status.success() {
        let detail = send_error.trim();
        bail!("btrfs send: {}", if detail.is_empty() { send_status.to_string() } else { detail.to_owned() });
    }
    if !receive.status.success() {
        let stderr = String::from_utf8_lossy(&receive.stderr);
        let detail = stderr.trim();
        bail!(
            "btrfs receive: {}",
            if detail.is_empty() { receive.status.to_string() } else { detail.to_owned() }
        );
    }
    let dump = String::from_utf8_lossy(&receive.stdout);
    Ok(Comparison { older_id, newer_id, entries: interpret(&dump) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream taken across known changes in difftest/: unchanged.txt chmod
    /// 600, modified.txt rewritten, deleted.txt and deleted-dir removed,
    /// oldname.txt renamed to newname.txt, added.txt, added-dir and link.txt
    /// created - and rmdir run, which touched /usr/bin/rmdir's atime.
    const DUMP: &str = include_str!("../tests/data/receive-dump-known-changes.txt");

    fn paths(entries: &[Entry], change: Change) -> Vec<String> {
        let mut found: Vec<String> =
            entries.iter().filter(|e| e.change == change).map(|e| e.path.clone()).collect();
        found.sort();
        found
    }

    #[test]
    fn the_known_changes() {
        let entries = interpret(DUMP);
        assert_eq!(
            paths(&entries, Change::Added),
            ["difftest/added-dir", "difftest/added.txt", "difftest/link.txt"]
        );
        assert_eq!(paths(&entries, Change::Removed), ["difftest/deleted-dir", "difftest/deleted.txt"]);
        assert_eq!(paths(&entries, Change::Modified), ["difftest/modified.txt"]);
        assert_eq!(paths(&entries, Change::Attrs), ["difftest/unchanged.txt"]);
        let renamed: Vec<&Entry> = entries.iter().filter(|e| e.change == Change::Renamed).collect();
        assert_eq!(renamed.len(), 1);
        assert_eq!(
            (renamed[0].old_path.as_str(), renamed[0].path.as_str()),
            ("difftest/oldname.txt", "difftest/newname.txt")
        );
    }

    #[test]
    fn no_noise() {
        let entries = interpret(DUMP);
        for entry in &entries {
            assert!(!entry.path.starts_with("./") && !entry.path.starts_with("snapshot/"), "{}", entry.path);
            assert!(!is_temp_name(entry.path.rsplit('/').next().unwrap()), "{}", entry.path);
        }
        let all: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(!all.contains(&"usr/bin/rmdir"), "an atime is not a change");
        assert!(!all.contains(&"difftest"), "nor is a parent directory's mtime");
    }

    #[test]
    fn parsing_lines() {
        let parsed = parse("unlink          ./snapshot/difftest/deleted.txt\n\n");
        assert_eq!(parsed, [("unlink".into(), "./snapshot/difftest/deleted.txt".into(), BTreeMap::new())]);
        let (_, path, fields) = &parse("chmod ./snapshot/x mode=600")[0];
        assert_eq!((path.as_str(), fields["mode"].as_str()), ("./snapshot/x", "600"));
        let fields = parse_fields("name=security.selinux value=a b c len=35");
        assert_eq!(fields["value"], "a b c");
        assert_eq!(fields["len"], "35");
        assert!(parse("\n\n").is_empty());
        assert!(parse_fields("").is_empty());
    }

    #[test]
    fn stripping() {
        assert_eq!(strip_subvolume("./snapshot/difftest/x"), "difftest/x");
        assert_eq!(strip_subvolume("./snapshot"), "");
        assert_eq!(strip_subvolume("./x/y"), "y");
        assert!(is_temp_name("o50578-55-0"));
        assert!(!is_temp_name("o50578-55"));
        assert!(!is_temp_name("notes-1-2-3"));
    }

    #[test]
    fn summary_and_lines() {
        let comparison = Comparison { older_id: 1, newer_id: 2, entries: interpret(DUMP) };
        let summary = comparison.summary();
        assert_eq!(summary, "3 added, 2 removed, 1 modified, 1 renamed, 1 attrs");
        let lines = comparison.lines();
        assert!(lines[0].starts_with('+') && lines.last().unwrap().starts_with('a'));
        assert!(lines.contains(&"> difftest/oldname.txt -> difftest/newname.txt".to_owned()));
        let empty = Comparison { older_id: 1, newer_id: 2, entries: vec![] };
        assert_eq!(empty.summary(), "no differences");
        assert!(empty.lines().is_empty());
    }
}
