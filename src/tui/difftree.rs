// SPDX-License-Identifier: GPL-3.0-or-later
//! A comparison as a tree: changes grouped by folder, folders folded and
//! unfolded, each folder counting the changes beneath it.

use std::collections::{BTreeMap, BTreeSet};

use crate::diff::{Change, Comparison};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// One path component - or several joined with '/' when a folder has a
    /// single folder in it and nothing else, so /usr/lib/modules is one row.
    pub name: String,
    pub depth: usize,
    /// What happened to this path itself, if anything.
    pub change: Option<Change>,
    /// Where a renamed path came from.
    pub old_path: String,
    pub children: Vec<usize>,
    /// Changes at and below this node, by `Change::ALL`'s order.
    pub counts: [usize; 5],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffTree {
    pub title: String,
    pub summary: [usize; 5],
    pub nodes: Vec<Node>,
    pub roots: Vec<usize>,
    pub open: BTreeSet<usize>,
    pub cursor: usize,
    pub scroll: usize,
}

fn index(change: Change) -> usize {
    Change::ALL.iter().position(|&c| c == change).expect("in ALL")
}

#[derive(Default)]
struct Building {
    change: Option<(Change, String)>,
    children: BTreeMap<String, Building>,
}

/// More than this many changes and only the top level starts open.
const OPEN_ALL_UP_TO: usize = 300;

impl DiffTree {
    pub fn new(comparison: &Comparison, title: String) -> DiffTree {
        let mut top = Building::default();
        for entry in &comparison.entries {
            let mut node = &mut top;
            for part in entry.path.split('/').filter(|p| !p.is_empty()) {
                node = node.children.entry(part.to_owned()).or_default();
            }
            node.change = Some((entry.change, entry.old_path.clone()));
        }
        let mut tree = DiffTree {
            title,
            summary: [0; 5],
            nodes: Vec::new(),
            roots: Vec::new(),
            open: BTreeSet::new(),
            cursor: 0,
            scroll: 0,
        };
        for entry in &comparison.entries {
            tree.summary[index(entry.change)] += 1;
        }
        tree.roots = tree.add_children(top.children, 0);
        let all = comparison.entries.len() <= OPEN_ALL_UP_TO;
        for i in 0..tree.nodes.len() {
            if !tree.nodes[i].children.is_empty() && (all || tree.nodes[i].depth == 0) {
                tree.open.insert(i);
            }
        }
        tree
    }

    fn add_children(&mut self, children: BTreeMap<String, Building>, depth: usize) -> Vec<usize> {
        // Folders first, then files, each by name.
        // (The map is already by name, and the sort is stable.)
        let mut children: Vec<(String, Building)> = children.into_iter().collect();
        children.sort_by_key(|(_, b)| b.children.is_empty());
        children.into_iter().map(|(name, building)| self.add(name, building, depth)).collect()
    }

    fn add(&mut self, mut name: String, mut building: Building, depth: usize) -> usize {
        while building.change.is_none() && building.children.len() == 1 {
            let (child_name, child) = building.children.pop_first().expect("one child");
            if child.children.is_empty() {
                // Keep a file as its own row under its folder.
                building.children.insert(child_name, child);
                break;
            }
            name = format!("{name}/{child_name}");
            building = child;
        }
        let at = self.nodes.len();
        let (change, old_path) = building.change.map_or((None, String::new()), |(c, o)| (Some(c), o));
        self.nodes.push(Node { name, depth, change, old_path, children: Vec::new(), counts: [0; 5] });
        let children = self.add_children(building.children, depth + 1);
        let mut counts = [0; 5];
        if let Some(change) = change {
            counts[index(change)] += 1;
        }
        for &c in &children {
            for (total, n) in counts.iter_mut().zip(self.nodes[c].counts) {
                *total += n;
            }
        }
        self.nodes[at].children = children;
        self.nodes[at].counts = counts;
        at
    }

    /// The rows on screen, in order: open folders show their children.
    pub fn rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            rows.push(i);
            if self.open.contains(&i) {
                stack.extend(self.nodes[i].children.iter().rev());
            }
        }
        rows
    }

    pub fn current(&self) -> Option<usize> {
        self.rows().get(self.cursor).copied()
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.rows().len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last.max(0)) as usize;
    }

    pub fn toggle(&mut self) {
        if let Some(i) = self.current().filter(|&i| !self.nodes[i].children.is_empty())
            && !self.open.remove(&i)
        {
            self.open.insert(i);
        }
    }

    /// Open the folder under the cursor, or step into it if it already is.
    pub fn right(&mut self) {
        let Some(i) = self.current() else { return };
        if self.nodes[i].children.is_empty() {
            return;
        }
        if self.open.insert(i) {
            return;
        }
        self.move_by(1);
    }

    /// Close the folder under the cursor, or go to the folder it is in.
    pub fn left(&mut self) {
        let Some(i) = self.current() else { return };
        if self.open.remove(&i) {
            return;
        }
        let rows = self.rows();
        let depth = self.nodes[i].depth;
        if let Some(parent) = rows[..self.cursor].iter().rposition(|&r| self.nodes[r].depth < depth) {
            self.cursor = parent;
        }
    }

    pub fn open_all(&mut self, open: bool) {
        let current = self.current();
        self.open = if open {
            (0..self.nodes.len()).filter(|&i| !self.nodes[i].children.is_empty()).collect()
        } else {
            BTreeSet::new()
        };
        // Keep the cursor on the same row when it is still shown, else on its top-level folder.
        let rows = self.rows();
        self.cursor = current
            .and_then(|c| rows.iter().position(|&r| r == c))
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
    }

    pub fn scroll_to(&mut self, height: usize) {
        let height = height.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Entry, interpret};

    fn tree() -> DiffTree {
        let comparison = Comparison {
            older_id: 1,
            newer_id: 2,
            entries: interpret(include_str!("../../tests/data/receive-dump-known-changes.txt")),
        };
        DiffTree::new(&comparison, "t".into())
    }

    fn names(tree: &DiffTree) -> Vec<String> {
        tree.rows()
            .iter()
            .map(|&i| format!("{}{}", "  ".repeat(tree.nodes[i].depth), tree.nodes[i].name))
            .collect()
    }

    #[test]
    fn grouped_by_folder_with_counts() {
        let t = tree();
        assert_eq!(t.summary, [3, 2, 1, 1, 1]);
        let root = t.roots[0];
        assert_eq!(t.nodes[root].name, "difftest");
        assert_eq!(t.nodes[root].counts, [3, 2, 1, 1, 1]);
        assert_eq!(
            names(&t),
            [
                "difftest",
                "  added-dir",
                "  added.txt",
                "  deleted-dir",
                "  deleted.txt",
                "  link.txt",
                "  modified.txt",
                "  newname.txt",
                "  unchanged.txt",
            ]
        );
        let renamed = t.nodes.iter().find(|n| n.name == "newname.txt").unwrap();
        assert_eq!(
            (renamed.change, renamed.old_path.as_str()),
            (Some(Change::Renamed), "difftest/oldname.txt")
        );
    }

    #[test]
    fn single_folder_chains_are_one_row() {
        let entry = |p: &str| Entry { change: Change::Modified, path: p.into(), old_path: String::new() };
        let comparison = Comparison {
            older_id: 1,
            newer_id: 2,
            entries: vec![entry("usr/lib/modules/a"), entry("usr/lib/modules/b"), entry("etc/x")],
        };
        let t = DiffTree::new(&comparison, "t".into());
        assert_eq!(names(&t), ["etc", "  x", "usr/lib/modules", "  a", "  b"]);
    }

    #[test]
    fn folding_and_moving() {
        let mut t = tree();
        assert_eq!(t.rows().len(), 9);
        t.left(); // on difftest: close it
        assert_eq!(t.rows().len(), 1);
        t.right(); // open it again
        t.right(); // and step in
        assert_eq!(t.cursor, 1);
        t.move_by(100);
        assert_eq!(t.cursor, 8);
        t.left(); // a file: go to its folder
        assert_eq!(t.cursor, 0);
        t.open_all(false);
        assert_eq!(t.rows().len(), 1);
        t.open_all(true);
        assert_eq!(t.rows().len(), 9);
        t.toggle();
        assert_eq!(t.rows().len(), 1);
    }

    #[test]
    fn empty() {
        let mut t = DiffTree::new(&Comparison { older_id: 1, newer_id: 2, entries: vec![] }, "t".into());
        t.move_by(1);
        t.left();
        t.right();
        assert!(t.rows().is_empty() && t.current().is_none());
    }
}
