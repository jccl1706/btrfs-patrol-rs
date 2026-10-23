// SPDX-License-Identifier: GPL-3.0-or-later
//! The TUI's state and what each key and click does to it - no terminal in
//! here, so all of it is tested without one. The event loop in mod.rs turns
//! crossterm events into [`Key`]s and clicks, and carries out the
//! [`Action`]s that need the world: reloading the store, running btrfs.

use std::collections::BTreeMap;

use chrono::{DateTime, Local};
use ratatui::layout::{Position, Rect};

use super::difftree::DiffTree;
use super::fuzzy;
use super::size::Size;
use crate::diff::Comparison;
use crate::selector;
use crate::snapshot::{ROOT, Snapshot, Store};
use crate::table::{printable, shown_description, wrap_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Tab,
    BackTab,
    Enter,
    Esc,
    Backspace,
    Delete,
    Char(char),
}

/// What the event loop has to do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Reload,
    /// Compare these two snapshots, older first.
    Compare(u64, u64),
    /// Take snapshots with this description: of one subvolume, or of every
    /// managed one when the "all" tab is open.
    Snapshot(String, Option<String>),
    Describe(u64, String),
    Keep(u64, bool),
    Delete(u64),
    /// Check a rollback to this snapshot and show its plan.
    PlanRollback(u64),
    /// The plan on screen was confirmed with y.
    Rollback,
    /// The plan on screen was dismissed.
    CancelRollback,
}

/// What the line at the bottom is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    NewSnapshot,
    Describe(u64),
    ConfirmDelete(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Browse,
    /// Typing in the search box; the list follows each key.
    Search,
    /// A popup or a full page over the dashboard.
    Overlay,
    /// Asking something at the bottom: a description, a confirmation.
    Prompt,
}

/// What a piece of text means, which the view turns into a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Dim,
    Accent,
    Heading,
    Error,
    Ok,
}

/// A popup of text: a snapshot's full details, or help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub lines: Vec<(Tone, String)>,
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Text(Page),
    Diff(DiffTree),
}

/// Where a click lands, recorded by the view as it draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A row of the snapshot list: its index among the visible rows.
    Row(usize),
    Tab(usize),
    /// A row of the diff tree.
    TreeRow(usize),
}

pub const ALL: &str = "all";

const NEEDS_ROOT: &str = "taking and changing snapshots needs root: run 'sudo btrfs-patrol tui'";

#[derive(Debug)]
pub struct Screen {
    pub snapshots: Vec<Snapshot>,
    /// The subvolume tabs; "all" first when there is more than one.
    pub tabs: Vec<String>,
    pub tab: usize,
    /// The search: a selector or fuzzy text.
    pub query: Option<String>,
    /// Why the query matches nothing, when it is a selector that's wrong.
    pub query_error: Option<String>,
    /// The query before this search began, for Esc to go back to.
    query_before: Option<String>,
    /// Indices into `snapshots` of the rows shown, in ID order.
    pub visible: Vec<usize>,
    pub cursor: usize,
    /// The first row on screen.
    pub offset: usize,
    /// The first snapshot picked for a comparison.
    pub mark: Option<u64>,
    pub mode: Mode,
    pub input: String,
    pub message: Option<(Tone, String)>,
    pub overlay: Option<Overlay>,
    pub show_subvolume: bool,
    pub snapshots_dir: String,
    pub sizes: BTreeMap<u64, Size>,
    /// Whether sizes can be measured at all: it needs root.
    pub can_measure: bool,
    /// Used and total bytes of the filesystem.
    pub disk: Option<(u64, u64)>,
    pub now: DateTime<Local>,
    pub hits: Vec<(Rect, Hit)>,
    pub prompt: Option<Prompt>,
    /// The snapshot whose rollback plan is on screen, waiting for y.
    pub rollback: Option<u64>,
    /// Whether snapshots can be taken and changed: it needs root.
    pub can_change: bool,
}

impl Screen {
    pub fn new(
        snapshots: Vec<Snapshot>,
        managed: Vec<String>,
        show_subvolume: bool,
        store: &Store,
    ) -> Screen {
        let mut screen = Screen {
            snapshots: Vec::new(),
            tabs: if managed.is_empty() { vec![ROOT.into()] } else { managed },
            tab: 0,
            query: None,
            query_error: None,
            query_before: None,
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            mark: None,
            mode: Mode::Browse,
            input: String::new(),
            message: None,
            overlay: None,
            show_subvolume,
            snapshots_dir: store.directory.display().to_string(),
            sizes: BTreeMap::new(),
            can_measure: false,
            disk: None,
            now: Local::now(),
            hits: Vec::new(),
            prompt: None,
            rollback: None,
            can_change: false,
        };
        screen.set_snapshots(snapshots);
        // Open on the first tab - "all" when there is more than one - at the newest.
        screen.tab = 0;
        screen.refilter(None);
        screen.cursor = screen.visible.len().saturating_sub(1);
        screen
    }

    /// Replace the snapshots - after a reload - keeping the cursor on the
    /// same snapshot when it is still there.
    pub fn set_snapshots(&mut self, snapshots: Vec<Snapshot>) {
        let current = self.current().map(|s| s.id);
        self.snapshots = snapshots;
        // A store can hold snapshots of subvolumes no longer configured;
        // they get a tab too, or they could not be seen at all.
        let mut tabs: Vec<String> = self.tabs.iter().filter(|t| *t != ALL).cloned().collect();
        for s in &self.snapshots {
            if !tabs.contains(&s.subvolume) {
                tabs.push(s.subvolume.clone());
            }
        }
        let selected = self.tabs.get(self.tab).cloned();
        if tabs.len() > 1 {
            tabs.insert(0, ALL.into());
        }
        self.tabs = tabs;
        self.tab = selected.and_then(|t| self.tabs.iter().position(|x| *x == t)).unwrap_or(0);
        if self.mark.is_some_and(|m| !self.snapshots.iter().any(|s| s.id == m)) {
            self.mark = None;
        }
        let ids: Vec<u64> = self.snapshots.iter().map(|s| s.id).collect();
        self.sizes.retain(|id, _| ids.contains(id));
        self.refilter(current);
    }

    /// Work out the visible rows again, keeping the cursor on `keep` if it's
    /// still visible, or as close as possible.
    fn refilter(&mut self, keep: Option<u64>) {
        let tab = &self.tabs[self.tab];
        let in_tab: Vec<&Snapshot> =
            self.snapshots.iter().filter(|s| tab == ALL || &s.subvolume == tab).collect();
        self.query_error = None;
        let ids: Vec<u64> = match self.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            None => in_tab.iter().map(|s| s.id).collect(),
            Some(q) if fuzzy::is_selector(q) => {
                let owned: Vec<Snapshot> = in_tab.iter().map(|s| (*s).clone()).collect();
                match selector::select(q, &owned) {
                    Ok(found) => found.iter().map(|s| s.id).collect(),
                    Err(e) => {
                        // Half-typed ("kind=") or wrong: say why rather than
                        // show an empty list that looks like an empty store.
                        self.query_error = Some(e.0);
                        Vec::new()
                    }
                }
            }
            Some(q) => in_tab.iter().filter(|s| fuzzy::matches(q, s)).map(|s| s.id).collect(),
        };
        self.visible =
            self.snapshots.iter().enumerate().filter(|(_, s)| ids.contains(&s.id)).map(|(i, _)| i).collect();
        let found = keep.and_then(|id| self.visible.iter().position(|&i| self.snapshots[i].id == id));
        self.cursor = found.unwrap_or(self.cursor).min(self.visible.len().saturating_sub(1));
    }

    pub fn current(&self) -> Option<&Snapshot> {
        self.visible.get(self.cursor).map(|&i| &self.snapshots[i])
    }

    pub fn rows(&self) -> impl Iterator<Item = &Snapshot> {
        self.visible.iter().map(|&i| &self.snapshots[i])
    }

    /// Snapshots whose size hasn't been asked for yet, newest first: the
    /// ones most likely to be looked at. Marks them as being measured.
    pub fn unmeasured(&mut self) -> Vec<u64> {
        if !self.can_measure {
            return Vec::new();
        }
        let ids: Vec<u64> =
            self.snapshots.iter().rev().map(|s| s.id).filter(|id| !self.sizes.contains_key(id)).collect();
        for &id in &ids {
            self.sizes.insert(id, Size::Measuring);
        }
        ids
    }

    /// Scroll so the cursor is on screen, for a list `height` rows tall.
    pub fn scroll(&mut self, height: usize) {
        let height = height.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + height {
            self.offset = self.cursor + 1 - height;
        }
        self.offset = self.offset.min(self.visible.len().saturating_sub(height));
    }

    pub fn key(&mut self, key: Key, page_height: usize) -> Action {
        self.message = None;
        match self.mode {
            Mode::Browse => self.browse(key, page_height),
            Mode::Search => self.searching(key),
            Mode::Overlay => self.overlaid(key, page_height),
            Mode::Prompt => self.prompting(key),
        }
    }

    /// Put the cursor on a snapshot - one just taken, say - if it is shown.
    pub fn select(&mut self, id: u64) {
        if let Some(n) = self.visible.iter().position(|&i| self.snapshots[i].id == id) {
            self.cursor = n;
        }
    }

    fn ask(&mut self, prompt: Prompt, input: String) {
        if !self.can_change {
            self.message = Some((Tone::Error, NEEDS_ROOT.into()));
            return;
        }
        self.prompt = Some(prompt);
        self.input = input;
        self.mode = Mode::Prompt;
    }

    fn prompting(&mut self, key: Key) -> Action {
        let Some(prompt) = self.prompt else {
            self.mode = Mode::Browse;
            return Action::None;
        };
        let done = |screen: &mut Screen| {
            screen.prompt = None;
            screen.mode = Mode::Browse;
        };
        if let Prompt::ConfirmDelete(id) = prompt {
            // Only y deletes; any other key - Enter included - keeps it.
            done(self);
            return if matches!(key, Key::Char('y') | Key::Char('Y')) {
                Action::Delete(id)
            } else {
                self.message = Some((Tone::Dim, "nothing deleted".into()));
                Action::None
            };
        }
        match key {
            Key::Char(c) => self.input.push(c),
            Key::Backspace => {
                self.input.pop();
            }
            Key::Esc => done(self),
            Key::Enter => {
                done(self);
                let text = self.input.trim().to_owned();
                return match prompt {
                    Prompt::NewSnapshot => {
                        let tab = self.tabs[self.tab].clone();
                        Action::Snapshot(text, (tab != ALL).then_some(tab))
                    }
                    Prompt::Describe(id) => Action::Describe(id, text),
                    Prompt::ConfirmDelete(_) => unreachable!("handled above"),
                };
            }
            _ => {}
        }
        Action::None
    }

    pub fn click(&mut self, column: u16, row: u16) -> Action {
        let at = Position { x: column, y: row };
        let Some(&(_, hit)) = self.hits.iter().find(|(area, _)| area.contains(at)) else {
            return Action::None;
        };
        self.message = None;
        match (self.mode, hit) {
            (Mode::Browse | Mode::Search, Hit::Row(n)) => {
                if n == self.cursor && self.mode == Mode::Browse {
                    // A click on the row already selected opens it, like Enter.
                    return self.browse(Key::Enter, 1);
                }
                self.cursor = n.min(self.visible.len().saturating_sub(1));
            }
            (Mode::Browse | Mode::Search, Hit::Tab(n)) => {
                self.tab = n.min(self.tabs.len() - 1);
                let current = self.current().map(|s| s.id);
                self.refilter(current);
            }
            (Mode::Overlay, Hit::TreeRow(n)) => {
                if let Some(Overlay::Diff(tree)) = &mut self.overlay {
                    if tree.cursor == n {
                        tree.toggle();
                    } else {
                        tree.cursor = n;
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    pub fn wheel(&mut self, down: bool, page_height: usize) -> Action {
        match self.mode {
            Mode::Search => Action::None,
            _ => self.key(if down { Key::Down } else { Key::Up }, page_height),
        }
    }

    fn browse(&mut self, key: Key, page_height: usize) -> Action {
        let last = self.visible.len().saturating_sub(1);
        let step = page_height.max(1);
        match key {
            Key::Up | Key::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => self.cursor = (self.cursor + 1).min(last),
            Key::PageUp => self.cursor = self.cursor.saturating_sub(step),
            Key::PageDown => self.cursor = (self.cursor + step).min(last),
            Key::Home | Key::Char('g') => self.cursor = 0,
            Key::End | Key::Char('G') => self.cursor = last,
            Key::Tab | Key::BackTab | Key::Right | Key::Left | Key::Char('l') | Key::Char('h') => {
                let n = self.tabs.len();
                let forward = matches!(key, Key::Tab | Key::Right | Key::Char('l'));
                self.tab = if forward { (self.tab + 1) % n } else { (self.tab + n - 1) % n };
                let current = self.current().map(|s| s.id);
                self.refilter(current);
            }
            Key::Enter => {
                if let Some(snapshot) = self.current() {
                    let size = self.sizes.get(&snapshot.id).cloned();
                    let page = detail_page(snapshot, &self.snapshots_dir, size, self.can_measure, &self.now);
                    self.overlay = Some(Overlay::Text(page));
                    self.mode = Mode::Overlay;
                }
            }
            Key::Char('/') => {
                self.query_before = self.query.clone();
                self.input = self.query.clone().unwrap_or_default();
                self.mode = Mode::Search;
            }
            Key::Char('c') => return self.compare(),
            Key::Char('n') => self.ask(Prompt::NewSnapshot, String::new()),
            Key::Char('d') => {
                if let Some((id, text)) = self.current().map(|s| (s.id, s.description.clone())) {
                    self.ask(Prompt::Describe(id), text);
                }
            }
            Key::Char('p') => {
                if let Some((id, keep)) = self.current().map(|s| (s.id, s.keep)) {
                    if self.can_change {
                        return Action::Keep(id, !keep);
                    }
                    self.message = Some((Tone::Error, NEEDS_ROOT.into()));
                }
            }
            Key::Char('R') => {
                if let Some(id) = self.current().map(|s| s.id) {
                    if self.can_change {
                        return Action::PlanRollback(id);
                    }
                    self.message = Some((Tone::Error, NEEDS_ROOT.into()));
                }
            }
            Key::Char('x') | Key::Delete => {
                if let Some(id) = self.current().map(|s| s.id) {
                    self.ask(Prompt::ConfirmDelete(id), String::new());
                }
            }
            Key::Char('r') => return Action::Reload,
            Key::Char('?') => {
                self.overlay = Some(Overlay::Text(help_page()));
                self.mode = Mode::Overlay;
            }
            Key::Esc if self.mark.is_some() => self.mark = None,
            Key::Esc if self.query.is_some() => {
                self.query = None;
                let current = self.current().map(|s| s.id);
                self.refilter(current);
            }
            Key::Char('q') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    fn compare(&mut self) -> Action {
        let Some(current) = self.current() else { return Action::None };
        let (id, subvolume) = (current.id, current.subvolume.clone());
        match self.mark {
            None => {
                self.mark = Some(id);
                self.message =
                    Some((Tone::Accent, format!("#{id} marked: move to another snapshot and press c")));
                Action::None
            }
            Some(mark) if mark == id => {
                self.mark = None;
                Action::None
            }
            Some(mark) => {
                let marked =
                    self.snapshots.iter().find(|s| s.id == mark).expect("marks are pruned on reload");
                if marked.subvolume != subvolume {
                    self.message = Some((
                        Tone::Error,
                        format!(
                            "#{mark} is of {} and #{id} of {subvolume}: only snapshots of one subvolume compare",
                            marked.subvolume
                        ),
                    ));
                    return Action::None;
                }
                self.mark = None;
                Action::Compare(mark.min(id), mark.max(id))
            }
        }
    }

    fn searching(&mut self, key: Key) -> Action {
        let current = self.current().map(|s| s.id);
        match key {
            Key::Char(c) => self.input.push(c),
            Key::Backspace => {
                self.input.pop();
            }
            Key::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                return Action::None;
            }
            Key::Down => {
                self.cursor = (self.cursor + 1).min(self.visible.len().saturating_sub(1));
                return Action::None;
            }
            Key::Esc => {
                self.query = self.query_before.take();
                self.mode = Mode::Browse;
                self.refilter(current);
                return Action::None;
            }
            Key::Enter => {
                self.mode = Mode::Browse;
                if self.query_error.is_some() {
                    self.query = None;
                    self.refilter(current);
                }
                return Action::None;
            }
            _ => return Action::None,
        }
        let text = self.input.trim();
        self.query = (!text.is_empty()).then(|| text.to_owned());
        self.refilter(current);
        Action::None
    }

    /// Show a rollback plan: the popup waits for y.
    pub fn show_rollback(&mut self, id: u64, mut lines: Vec<(Tone, String)>) {
        lines.push((Tone::Plain, String::new()));
        lines.push((Tone::Error, "y rolls back now; any other key cancels".into()));
        self.overlay =
            Some(Overlay::Text(Page { title: format!("roll back to snapshot #{id}?"), lines, scroll: 0 }));
        self.rollback = Some(id);
        self.mode = Mode::Overlay;
    }

    fn overlaid(&mut self, key: Key, page_height: usize) -> Action {
        if self.rollback.take().is_some() {
            // Like the delete prompt: only y, never Enter.
            self.overlay = None;
            self.mode = Mode::Browse;
            return if matches!(key, Key::Char('y') | Key::Char('Y')) {
                Action::Rollback
            } else {
                self.message = Some((Tone::Dim, "no rollback".into()));
                Action::CancelRollback
            };
        }
        let mut close = false;
        match &mut self.overlay {
            None => close = true,
            Some(Overlay::Text(page)) => {
                let height = page_height.max(1);
                let last = page.lines.len().saturating_sub(height);
                match key {
                    Key::Up | Key::Char('k') => page.scroll = page.scroll.saturating_sub(1),
                    Key::Down | Key::Char('j') => page.scroll = (page.scroll + 1).min(last),
                    Key::PageUp => page.scroll = page.scroll.saturating_sub(height),
                    Key::PageDown | Key::Char(' ') => page.scroll = (page.scroll + height).min(last),
                    Key::Home | Key::Char('g') => page.scroll = 0,
                    Key::End | Key::Char('G') => page.scroll = last,
                    Key::Esc | Key::Enter | Key::Backspace | Key::Char('?') => close = true,
                    Key::Char('q') => return Action::Quit,
                    _ => {}
                }
            }
            Some(Overlay::Diff(tree)) => {
                let height = page_height.max(1) as isize;
                match key {
                    Key::Up | Key::Char('k') => tree.move_by(-1),
                    Key::Down | Key::Char('j') => tree.move_by(1),
                    Key::PageUp => tree.move_by(-height),
                    Key::PageDown => tree.move_by(height),
                    Key::Home | Key::Char('g') => tree.move_by(isize::MIN / 2),
                    Key::End | Key::Char('G') => tree.move_by(isize::MAX / 2),
                    Key::Right | Key::Char('l') => tree.right(),
                    Key::Left | Key::Char('h') => tree.left(),
                    Key::Enter | Key::Char(' ') => tree.toggle(),
                    Key::Char('+') | Key::Char('=') => tree.open_all(true),
                    Key::Char('-') => tree.open_all(false),
                    Key::Esc | Key::Backspace => close = true,
                    Key::Char('q') => return Action::Quit,
                    _ => {}
                }
            }
        }
        if close {
            self.overlay = None;
            self.mode = Mode::Browse;
        }
        Action::None
    }

    pub fn show_comparison(&mut self, comparison: &Comparison, subvolume: &str) {
        let title = format!("#{} → #{} · {}", comparison.older_id, comparison.newer_id, printable(subvolume));
        self.overlay = Some(Overlay::Diff(DiffTree::new(comparison, title)));
        self.mode = Mode::Overlay;
    }

    /// The keys that apply right now, for the key bar.
    pub fn keys(&self) -> Vec<(&'static str, String)> {
        match (self.mode, &self.overlay) {
            (Mode::Search, _) => vec![("⏎", "keep".into()), ("esc", "cancel".into()), ("↑↓", "move".into())],
            (Mode::Prompt, _) => match self.prompt {
                Some(Prompt::ConfirmDelete(_)) => {
                    vec![("y", "delete".into()), ("any other key", "keep it".into())]
                }
                _ => vec![("⏎", "ok".into()), ("esc", "cancel".into())],
            },
            (Mode::Overlay, Some(Overlay::Diff(_))) => vec![
                ("↑↓", "move".into()),
                ("←→", "fold".into()),
                ("+ -", "all".into()),
                ("esc", "back".into()),
                ("q", "quit".into()),
            ],
            (Mode::Overlay, _) if self.rollback.is_some() => {
                vec![("y", "roll back".into()), ("any other key", "cancel".into())]
            }
            (Mode::Overlay, _) => {
                vec![("↑↓", "scroll".into()), ("esc", "close".into()), ("q", "quit".into())]
            }
            (Mode::Browse, _) => {
                let mut keys = vec![("↑↓", "move".into())];
                if self.tabs.len() > 1 {
                    keys.push(("tab", "subvolume".into()));
                }
                if self.current().is_some() {
                    keys.push(("⏎", "details".into()));
                    let compare = match self.mark {
                        Some(m) => format!("compare with #{m}"),
                        None => "compare".into(),
                    };
                    keys.push(("c", compare));
                }
                if self.can_change {
                    keys.push(("n", "new".into()));
                    if self.current().is_some() {
                        keys.push(("d", "describe".into()));
                        keys.push(("p", "keep".into()));
                        keys.push(("x", "delete".into()));
                        keys.push(("R", "rollback".into()));
                    }
                }
                keys.push(("/", "search".into()));
                if self.mark.is_some() {
                    keys.push(("esc", "unmark".into()));
                } else if self.query.is_some() {
                    keys.push(("esc", "clear search".into()));
                }
                keys.extend([("?", "help".into()), ("q", "quit".into())]);
                keys
            }
        }
    }
}

/// The size's text, or why there is none.
pub fn size_text(size: Option<&Size>, can_measure: bool) -> String {
    match size {
        Some(Size::Bytes(bytes)) => super::theme::size(*bytes),
        Some(Size::Measuring) => "measuring…".into(),
        Some(Size::Failed(e)) => format!("unknown ({e})"),
        None if can_measure => "measuring…".into(),
        None => "run as root to measure".into(),
    }
}

fn detail_page(
    s: &Snapshot,
    snapshots_dir: &str,
    size: Option<Size>,
    can_measure: bool,
    now: &DateTime<Local>,
) -> Page {
    let field = |name: &str, value: String| (Tone::Plain, format!("{name:<13}{value}"));
    let created = format!(
        "{}  ·  {}",
        s.created.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S %:z"),
        super::theme::ago(&s.created, now)
    );
    let boot = match &s.boot {
        Some(b) => {
            format!("{} {} stored ({}…, {}…)", b.pkgbase, b.version, &b.kernel[..8], &b.initramfs[..8])
        }
        None => "not stored".into(),
    };
    let mut lines = vec![
        field("subvolume", printable(&s.subvolume)),
        field("created", created),
        field("kind", printable(&s.kind)),
        field("kept", if s.keep { "yes - never pruned".into() } else { "no".into() }),
        field("kernel", printable(&s.kernel)),
        field("boot files", boot),
        field("size", size_text(size.as_ref(), can_measure)),
        (Tone::Dim, format!("{:<13}exclusive: what deleting it frees", "")),
        field("path", format!("{snapshots_dir}/{}/snapshot", s.id)),
        (Tone::Plain, String::new()),
        (Tone::Heading, "description".into()),
    ];
    lines.extend(wrap_text(&shown_description(s), 70).into_iter().map(|l| (Tone::Plain, l)));
    Page { title: format!("snapshot #{}", s.id), lines, scroll: 0 }
}

fn help_page() -> Page {
    let keys = |list: &[(&str, &str)]| -> Vec<(Tone, String)> {
        list.iter().map(|(key, what)| (Tone::Plain, format!("  {key:<16}{what}"))).collect()
    };
    let mut lines = vec![(Tone::Heading, "snapshots".into())];
    lines.extend(keys(&[
        ("↑ ↓  j k", "move (or the mouse wheel)"),
        ("PgUp PgDn", "a page at a time"),
        ("Home End  g G", "first, last (newest)"),
        ("Tab ← →", "next, previous subvolume (or click a tab)"),
        ("Enter", "a snapshot's full details (or click it twice)"),
        ("c", "mark a snapshot, then c on another: what changed"),
        ("/", "search: text, or a selector such as kind=pacman"),
        ("n", "new snapshot (of this tab's subvolume, or all)"),
        ("d", "describe: change the description"),
        ("p", "keep (pin) or unkeep: kept ones are never pruned"),
        ("x  Del", "delete, after y to confirm"),
        ("R", "roll back to it: shows every check and the plan, then y"),
        ("Esc", "unmark, or clear the search"),
        ("r", "reload (it also reloads when snapshots come and go)"),
        ("q", "quit"),
    ]));
    lines.push((Tone::Plain, String::new()));
    lines.push((Tone::Heading, "comparison".into()));
    lines.extend(keys(&[
        ("→ ←  l h", "open, close a folder"),
        ("Enter Space", "open or close"),
        ("+ -", "open all, close all"),
        ("Esc", "back to the snapshots"),
    ]));
    lines.push((Tone::Plain, String::new()));
    for line in selector::HELP.lines() {
        let tone = if line.ends_with(':') { Tone::Heading } else { Tone::Plain };
        lines.push((tone, line.to_owned()));
    }
    Page { title: "help".into(), lines, scroll: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::interpret;
    use crate::snapshot::tests::snapshot;

    fn screen(managed: &[&str]) -> Screen {
        let mut all: Vec<Snapshot> = (1..=5).map(snapshot).collect();
        all[1].subvolume = "home".into();
        all[3].keep = true;
        all[4].kind = "pacman-pre".into();
        all[4].description = "upgrade mesa".into();
        let managed = managed.iter().map(|s| s.to_string()).collect();
        Screen::new(all, managed, true, &Store::new("/.snapshots"))
    }

    fn press(screen: &mut Screen, keys: &[Key]) -> Action {
        keys.iter().fold(Action::None, |_, &k| screen.key(k, 10))
    }

    fn typing(screen: &mut Screen, text: &str) {
        for c in text.chars() {
            screen.key(Key::Char(c), 10);
        }
    }

    fn ids(screen: &Screen) -> Vec<u64> {
        screen.rows().map(|s| s.id).collect()
    }

    #[test]
    fn starts_on_the_newest_of_all() {
        let s = screen(&["root", "home"]);
        assert_eq!(s.tabs, ["all", "root", "home"]);
        assert_eq!(ids(&s), [1, 2, 3, 4, 5]);
        assert_eq!(s.current().unwrap().id, 5);
    }

    #[test]
    fn unconfigured_subvolumes_still_get_a_tab() {
        assert_eq!(screen(&["root"]).tabs, ["all", "root", "home"]);
    }

    #[test]
    fn tabs_filter_by_subvolume_and_keep_the_cursor() {
        let mut s = screen(&["root", "home"]);
        press(&mut s, &[Key::Up, Key::Up]); // on 3
        press(&mut s, &[Key::Tab]);
        assert_eq!(ids(&s), [1, 3, 4, 5]);
        assert_eq!(s.current().unwrap().id, 3);
        press(&mut s, &[Key::Right]);
        assert_eq!(ids(&s), [2]);
        press(&mut s, &[Key::Left, Key::BackTab]);
        assert_eq!(s.tabs[s.tab], "all");
    }

    #[test]
    fn moving_stays_in_bounds() {
        let mut s = screen(&["root"]);
        press(&mut s, &[Key::Down, Key::Down]);
        assert_eq!(s.cursor, 4);
        press(&mut s, &[Key::Home, Key::Up]);
        assert_eq!(s.cursor, 0);
        press(&mut s, &[Key::PageDown]);
        assert_eq!(s.cursor, 4);
    }

    #[test]
    fn scrolling_follows_the_cursor() {
        let mut s = screen(&["root"]);
        s.scroll(2);
        assert_eq!(s.offset, 3);
        press(&mut s, &[Key::Home]);
        s.scroll(2);
        assert_eq!(s.offset, 0);
    }

    #[test]
    fn search_follows_each_key() {
        let mut s = screen(&["root"]);
        press(&mut s, &[Key::Char('/')]);
        typing(&mut s, "msa");
        assert_eq!((s.mode, ids(&s)), (Mode::Search, vec![5]));
        press(&mut s, &[Key::Backspace, Key::Backspace, Key::Backspace]);
        assert_eq!(ids(&s).len(), 5);
        typing(&mut s, "keep=yes");
        assert_eq!(ids(&s), [4]);
        press(&mut s, &[Key::Enter]);
        assert_eq!((s.mode, s.query.as_deref()), (Mode::Browse, Some("keep=yes")));
        press(&mut s, &[Key::Esc]);
        assert_eq!(ids(&s).len(), 5);
    }

    #[test]
    fn a_half_typed_selector_says_why() {
        let mut s = screen(&["root"]);
        press(&mut s, &[Key::Char('/')]);
        typing(&mut s, "kind=");
        assert!(ids(&s).is_empty());
        assert!(s.query_error.as_ref().unwrap().contains("some text"));
        typing(&mut s, "pac");
        assert_eq!((ids(&s), s.query_error.as_ref()), (vec![5], None));
    }

    #[test]
    fn esc_puts_the_previous_search_back() {
        let mut s = screen(&["root"]);
        press(&mut s, &[Key::Char('/')]);
        typing(&mut s, "mesa");
        press(&mut s, &[Key::Enter, Key::Char('/')]);
        typing(&mut s, "x");
        press(&mut s, &[Key::Esc]);
        assert_eq!((s.query.as_deref(), ids(&s)), (Some("mesa"), vec![5]));
    }

    #[test]
    fn enter_on_a_broken_selector_drops_it() {
        let mut s = screen(&["root"]);
        press(&mut s, &[Key::Char('/')]);
        typing(&mut s, "colour=red");
        press(&mut s, &[Key::Enter]);
        assert_eq!((s.query.as_ref(), ids(&s).len()), (None, 5));
    }

    #[test]
    fn typing_q_in_the_search_does_not_quit() {
        let mut s = screen(&["root"]);
        let action = press(&mut s, &[Key::Char('/'), Key::Char('q')]);
        assert_eq!((action, s.input.as_str()), (Action::None, "q"));
    }

    #[test]
    fn comparing_two_snapshots() {
        let mut s = screen(&["root", "home"]);
        assert_eq!(press(&mut s, &[Key::Char('c')]), Action::None);
        assert_eq!(s.mark, Some(5));
        assert_eq!(press(&mut s, &[Key::Home, Key::Char('c')]), Action::Compare(1, 5));
        assert_eq!(s.mark, None);
    }

    #[test]
    fn comparing_across_subvolumes_is_refused() {
        let mut s = screen(&["root", "home"]);
        press(&mut s, &[Key::Char('c'), Key::Home, Key::Down]); // mark 5, move to 2 (home)
        assert_eq!(press(&mut s, &[Key::Char('c')]), Action::None);
        assert_eq!(s.message.as_ref().unwrap().0, Tone::Error);
        assert_eq!(s.mark, Some(5));
        press(&mut s, &[Key::Esc]);
        assert_eq!(s.mark, None);
    }

    #[test]
    fn the_comparison_tree() {
        let mut s = screen(&["root"]);
        let comparison = Comparison {
            older_id: 1,
            newer_id: 3,
            entries: interpret(include_str!("../../tests/data/receive-dump-known-changes.txt")),
        };
        s.show_comparison(&comparison, "root");
        let rows = |s: &Screen| match &s.overlay {
            Some(Overlay::Diff(t)) => t.rows().len(),
            _ => panic!("no tree"),
        };
        assert_eq!(rows(&s), 9);
        press(&mut s, &[Key::Char('-')]);
        assert_eq!(rows(&s), 1);
        press(&mut s, &[Key::Char('+'), Key::Down, Key::Left]);
        assert_eq!(rows(&s), 9);
        press(&mut s, &[Key::Esc]);
        assert_eq!((s.mode, s.overlay.is_none()), (Mode::Browse, true));
    }

    #[test]
    fn popups() {
        let mut s = screen(&["root"]);
        press(&mut s, &[Key::Enter]);
        let Some(Overlay::Text(page)) = &s.overlay else { panic!() };
        assert_eq!(page.title, "snapshot #5");
        assert!(page.lines.iter().any(|(_, l)| l.contains("pacman-pre")));
        assert!(page.lines.iter().any(|(_, l)| l.contains("run as root to measure")));
        press(&mut s, &[Key::Esc, Key::Char('?')]);
        let Some(Overlay::Text(page)) = &s.overlay else { panic!() };
        assert_eq!(page.title, "help");
        assert_eq!(press(&mut s, &[Key::Char('q')]), Action::Quit);
    }

    #[test]
    fn clicks() {
        let mut s = screen(&["root", "home"]);
        s.hits = vec![
            (Rect::new(0, 5, 40, 1), Hit::Row(1)),
            (Rect::new(0, 6, 40, 1), Hit::Row(2)),
            (Rect::new(20, 0, 6, 1), Hit::Tab(2)),
        ];
        s.click(3, 5);
        assert_eq!(s.current().unwrap().id, 2);
        s.click(3, 5); // again: open it
        assert_eq!(s.mode, Mode::Overlay);
        press(&mut s, &[Key::Esc]);
        s.click(22, 0);
        assert_eq!(s.tabs[s.tab], "home");
        assert_eq!(s.click(60, 20), Action::None);
    }

    #[test]
    fn sizes_are_asked_for_once_newest_first_and_only_as_root() {
        let mut s = screen(&["root"]);
        assert!(s.unmeasured().is_empty());
        s.can_measure = true;
        assert_eq!(s.unmeasured(), [5, 4, 3, 2, 1]);
        assert!(s.unmeasured().is_empty());
        let fewer: Vec<Snapshot> = s.snapshots[1..].to_vec();
        s.set_snapshots(fewer);
        assert!(!s.sizes.contains_key(&1));
    }

    #[test]
    fn a_reload_keeps_the_cursor_and_drops_a_vanished_mark() {
        let mut s = screen(&["root"]);
        press(&mut s, &[Key::Up, Key::Char('c')]); // mark 4
        let fewer: Vec<Snapshot> = s.snapshots.iter().filter(|x| x.id != 1).cloned().collect();
        s.set_snapshots(fewer);
        assert_eq!(s.current().unwrap().id, 4);
        let without_4: Vec<Snapshot> = s.snapshots.iter().filter(|x| x.id != 4).cloned().collect();
        s.set_snapshots(without_4);
        assert_eq!(s.mark, None);
    }

    #[test]
    fn changes_need_root() {
        let mut s = screen(&["root"]);
        for key in ['n', 'd', 'x'] {
            assert_eq!(press(&mut s, &[Key::Char(key)]), Action::None);
            assert_eq!(s.mode, Mode::Browse);
            assert!(s.message.as_ref().unwrap().1.contains("needs root"));
        }
        assert_eq!(press(&mut s, &[Key::Char('p')]), Action::None);
        assert!(s.message.as_ref().unwrap().1.contains("needs root"));
    }

    #[test]
    fn a_new_snapshot_of_this_tab() {
        let mut s = screen(&["root", "home"]);
        s.can_change = true;
        press(&mut s, &[Key::Char('n')]);
        typing(&mut s, "before kde ");
        assert_eq!(press(&mut s, &[Key::Enter]), Action::Snapshot("before kde".into(), None));
        press(&mut s, &[Key::Tab, Key::Tab, Key::Char('n')]);
        assert_eq!(press(&mut s, &[Key::Enter]), Action::Snapshot(String::new(), Some("home".into())));
        press(&mut s, &[Key::Char('n'), Key::Char('q'), Key::Esc]);
        assert_eq!(s.mode, Mode::Browse, "q is typed, esc cancels");
    }

    #[test]
    fn describe_keep_delete() {
        let mut s = screen(&["root"]);
        s.can_change = true;
        press(&mut s, &[Key::Char('d')]);
        assert_eq!(s.input, "upgrade mesa", "starts from the old description");
        press(&mut s, &[Key::Backspace, Key::Backspace, Key::Backspace, Key::Backspace]);
        assert_eq!(press(&mut s, &[Key::Enter]), Action::Describe(5, "upgrade".into()));
        assert_eq!(press(&mut s, &[Key::Char('p')]), Action::Keep(5, true));
        assert_eq!(press(&mut s, &[Key::Char('x'), Key::Enter]), Action::None, "Enter is not yes");
        assert_eq!(press(&mut s, &[Key::Char('x'), Key::Char('y')]), Action::Delete(5));
        assert_eq!(press(&mut s, &[Key::Up, Key::Delete, Key::Char('y')]), Action::Delete(4));
    }

    #[test]
    fn a_rollback_needs_its_plan_and_a_y() {
        let mut s = screen(&["root"]);
        assert_eq!(press(&mut s, &[Key::Char('R')]), Action::None, "not as a user");
        s.can_change = true;
        assert_eq!(press(&mut s, &[Key::Char('R')]), Action::PlanRollback(5));
        s.show_rollback(5, vec![(Tone::Plain, "root subvolume: @".into())]);
        assert_eq!(press(&mut s, &[Key::Enter]), Action::CancelRollback, "Enter is not yes");
        assert_eq!(s.mode, Mode::Browse);
        s.show_rollback(5, vec![]);
        assert_eq!(press(&mut s, &[Key::Char('y')]), Action::Rollback);
        assert_eq!((s.mode, s.rollback), (Mode::Browse, None));
    }

    #[test]
    fn an_empty_store() {
        let mut s = Screen::new(vec![], vec!["root".into()], false, &Store::new("/x"));
        assert_eq!(press(&mut s, &[Key::Down, Key::Enter, Key::Char('c')]), Action::None);
        assert_eq!(s.mode, Mode::Browse);
        assert!(s.keys().iter().all(|(_, what)| what != "details"));
    }
}
