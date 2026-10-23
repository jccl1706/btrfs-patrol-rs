// SPDX-License-Identifier: GPL-3.0-or-later
//! Terminal output: colours and the snapshot table, shared by `list` and the TUI.

use std::io::IsTerminal;

use unicode_width::UnicodeWidthChar;

use crate::config::ColorChoice;
use crate::snapshot::Snapshot;

pub const GAP: &str = "  ";

/// Extra space before DESCRIPTION, on top of GAP. The fixed columns are short
/// and of a known shape; DESCRIPTION is free text, and at two spaces it
/// crowded KIND ("rollback" fills that column exactly). At exactly 80
/// columns it costs the KERNEL column, which is the first to go anyway.
pub const DESCRIPTION_GAP: &str = "  ";

pub const MIN_DESCRIPTION_WIDTH: usize = 20;

/// Columns the table gives up when the width is tight, in the order they go:
/// a date and a description identify a snapshot, a kernel version rarely does.
pub const DROPPABLE: [&str; 2] = ["KERNEL", "TIME"];

pub fn color_enabled(setting: ColorChoice, is_terminal: bool) -> bool {
    match setting {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            is_terminal
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
                && std::env::var_os("TERM").is_none_or(|v| v != "dumb")
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub enabled: bool,
}

impl Style {
    pub fn for_stdout(setting: ColorChoice) -> Style {
        Style { enabled: color_enabled(setting, std::io::stdout().is_terminal()) }
    }

    pub fn for_stderr(setting: ColorChoice) -> Style {
        Style { enabled: color_enabled(setting, std::io::stderr().is_terminal()) }
    }

    fn sgr(&self, code: &str, text: &str) -> String {
        if self.enabled { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }

    pub fn bold(&self, text: &str) -> String {
        self.sgr("1", text)
    }
    pub fn red(&self, text: &str) -> String {
        self.sgr("1;31", text)
    }
    pub fn green(&self, text: &str) -> String {
        self.sgr("1;32", text)
    }
    pub fn yellow(&self, text: &str) -> String {
        self.sgr("1;33", text)
    }
    pub fn blue(&self, text: &str) -> String {
        self.sgr("1;34", text)
    }
}

/// Unicode format characters (Cf) that matter here: zero-width characters
/// and the bidirectional overrides that reorder what is displayed.
fn is_format(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}'
        | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

fn is_private_use(c: char) -> bool {
    matches!(c, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
}

/// Text safe to put in a table.
///
/// Descriptions are written by people and by pacman's package lists, and are
/// printed straight to a terminal. A newline splits one snapshot across two
/// lines; an escape sequence is obeyed, not shown, so a description could
/// clear the screen; a bidi override reorders what is displayed, which matters
/// when the text says what a rollback is about to restore. Stored text is left
/// alone - this is only how it is shown.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter_map(|c| {
            if c.is_control() {
                Some(' ') // newline, tab, escape: keep the words apart
            } else if is_format(c) || is_private_use(c) {
                None
            } else {
                Some(c)
            }
        })
        .collect()
}

/// Terminal columns one character occupies: 0 combining, 2 wide, else 1.
pub fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Terminal columns text occupies, which is not its length for CJK or emoji.
pub fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Pad to `width` display columns.
pub fn pad(text: &str, width: usize, right: bool) -> String {
    let filler = " ".repeat(width.saturating_sub(display_width(text)));
    if right { filler + text } else { format!("{text}{filler}") }
}

/// Cut to `limit` display columns, ending with '…', without splitting a character.
pub fn truncate(text: &str, limit: usize) -> String {
    if display_width(text) <= limit {
        return text.to_owned();
    }
    let mut kept = String::new();
    let mut used = 0;
    for c in text.chars() {
        let width = char_width(c);
        if used + width + 1 > limit {
            break;
        }
        kept.push(c);
        used += width;
    }
    kept + "…"
}

/// Wrap on whitespace to `limit` display columns, breaking a long word if it must.
pub fn wrap_text(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut lines = Vec::new();
    let (mut current, mut used) = (String::new(), 0);
    for word in text.split_whitespace() {
        let mut word = word.to_owned();
        let mut width = display_width(&word);
        if !current.is_empty() && used + 1 + width > limit {
            lines.push(std::mem::take(&mut current));
            used = 0;
        }
        while width > limit {
            // A single word wider than the column: as much as fits, at least one character.
            let mut head = String::new();
            let mut head_width = 0;
            for c in word.chars() {
                if head_width + char_width(c) > limit && !head.is_empty() {
                    break;
                }
                head.push(c);
                head_width += char_width(c);
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            lines.push(head.clone());
            word = word[head.len()..].to_owned();
            width = display_width(&word);
        }
        if word.is_empty() {
            continue;
        }
        if current.is_empty() {
            current = word;
            used = width;
        } else {
            current.push(' ');
            current.push_str(&word);
            used += 1 + width;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn value(snapshot: &Snapshot, header: &str) -> String {
    match header {
        "ID" => format!("{}{}", if snapshot.keep { "*" } else { "" }, snapshot.id),
        "DATE" => snapshot.created.format("%Y-%m-%d").to_string(),
        "TIME" => snapshot.created.format("%H:%M:%S").to_string(),
        "SUBVOLUME" => printable(&snapshot.subvolume),
        "KERNEL" => printable(&snapshot.kernel),
        _ => printable(&snapshot.kind),
    }
}

/// The column widths for a set of snapshots, and how to render one row. The
/// TUI draws one row at a time from this too, so the two never disagree about
/// what a snapshot looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableLayout {
    pub headers: Vec<&'static str>,
    pub widths: Vec<usize>,
    pub description_width: usize,
}

impl TableLayout {
    /// Where the DESCRIPTION column starts.
    pub fn indent(&self) -> usize {
        self.widths.iter().sum::<usize>() + GAP.len() * self.widths.len() + DESCRIPTION_GAP.len()
    }

    pub fn header(&self) -> String {
        let headers: Vec<String> = self.headers.iter().map(|h| h.to_string()).collect();
        self.cell_parts(&headers).concat() + "DESCRIPTION"
    }

    /// The cells, one string each with its own trailing gap, so a caller can
    /// colour them apart without a second idea of where the columns are.
    pub fn cell_parts(&self, values: &[String]) -> Vec<String> {
        let mut parts: Vec<String> = values
            .iter()
            .zip(&self.widths)
            .enumerate()
            // IDs are right-aligned so the '*' marker sits next to the number.
            .map(|(i, (value, &width))| pad(value, width, i == 0) + GAP)
            .collect();
        if let Some(last) = parts.last_mut() {
            last.push_str(DESCRIPTION_GAP);
        }
        parts
    }

    pub fn columns(&self, snapshot: &Snapshot) -> Vec<String> {
        self.headers.iter().map(|h| value(snapshot, h)).collect()
    }

    /// The row's fixed columns, without the description.
    pub fn row(&self, snapshot: &Snapshot) -> String {
        self.cell_parts(&self.columns(snapshot)).concat()
    }
}

pub fn shown_description(snapshot: &Snapshot) -> String {
    let text = printable(&snapshot.description);
    if text.trim().is_empty() { "-".into() } else { text }
}

/// Measure the columns needed for `snapshots` within `width`, dropping
/// columns rather than letting a row run past the width it was given.
pub fn layout(snapshots: &[&Snapshot], width: usize, show_subvolume: bool) -> TableLayout {
    let mut headers = vec!["ID", "DATE", "TIME"];
    if show_subvolume {
        headers.push("SUBVOLUME");
    }
    headers.extend(["KERNEL", "KIND"]);
    let mut drops = DROPPABLE.iter();
    loop {
        let widths: Vec<usize> = headers
            .iter()
            .map(|h| snapshots.iter().map(|s| display_width(&value(s, h))).max().unwrap_or(0).max(h.len()))
            .collect();
        let table = TableLayout { headers: headers.clone(), widths, description_width: 0 };
        let indent = table.indent();
        let room = width.saturating_sub(indent);
        if room >= MIN_DESCRIPTION_WIDTH || drops.len() == 0 {
            return TableLayout { description_width: room.max(MIN_DESCRIPTION_WIDTH), ..table };
        }
        let drop = drops.next().expect("checked above");
        headers.retain(|h| h != drop);
    }
}

/// Render snapshots as a table that fits in `width` columns. Long
/// descriptions are truncated with '…', or wrapped onto indented lines when
/// `wrap` is set. Kept snapshots are marked with '*' before their ID.
pub fn format_table(
    snapshots: &[&Snapshot],
    style: Style,
    width: usize,
    wrap: bool,
    show_subvolume: bool,
) -> String {
    let table = layout(snapshots, width, show_subvolume);
    let mut lines = vec![style.bold(&table.header())];
    for snapshot in snapshots {
        let mut prefix = table.row(snapshot);
        if snapshot.keep {
            prefix = prefix.replacen('*', &style.green("*"), 1);
        }
        let description = shown_description(snapshot);
        let parts = if wrap {
            let parts = wrap_text(&description, table.description_width);
            if parts.is_empty() { vec![description] } else { parts }
        } else {
            vec![truncate(&description, table.description_width)]
        };
        lines.push(prefix + &parts[0]);
        lines.extend(parts[1..].iter().map(|part| " ".repeat(table.indent()) + part));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::snapshot;

    const PLAIN: Style = Style { enabled: false };

    #[test]
    fn printable_neutralises_terminal_tricks() {
        assert_eq!(printable("a\nb\tc\x1b[2J"), "a b c [2J");
        assert_eq!(printable("safe\u{202E}txt.exe"), "safetxt.exe");
        assert_eq!(printable("zero\u{200B}width"), "zerowidth");
        assert_eq!(printable("café 日本"), "café 日本");
    }

    #[test]
    fn widths() {
        assert_eq!(display_width("日本"), 4);
        assert_eq!(display_width("e\u{0301}"), 1);
        assert_eq!(pad("日", 4, false), "日  ");
        assert_eq!(pad("7", 3, true), "  7");
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert_eq!(truncate("short", 5), "short");
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap_text("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap_text("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap_text("x abcdefghij y", 4), ["x", "abcd", "efgh", "ij y"]);
        assert_eq!(wrap_text("   ", 4), Vec::<String>::new());
    }

    #[test]
    fn eighty_columns_drops_kernel_first() {
        let s = snapshot(1);
        let wide = layout(&[&s], 120, false);
        assert_eq!(wide.headers, ["ID", "DATE", "TIME", "KERNEL", "KIND"]);
        let narrow = layout(&[&s], 60, false);
        assert_eq!(narrow.headers, ["ID", "DATE", "TIME", "KIND"]);
        let tiny = layout(&[&s], 20, true);
        assert_eq!(tiny.headers, ["ID", "DATE", "SUBVOLUME", "KIND"]);
        assert_eq!(tiny.description_width, MIN_DESCRIPTION_WIDTH);
    }

    #[test]
    fn table_rows() {
        let mut kept = snapshot(12);
        kept.keep = true;
        kept.description = "before the mesa upgrade".into();
        let plain = snapshot(3);
        let text = format_table(&[&plain, &kept], PLAIN, 200, false, false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], " ID  DATE        TIME      KERNEL          KIND      DESCRIPTION");
        assert_eq!(lines[1], "  3  2026-09-21  22:25:56  7.2.11-arch1-1  manual    -");
        assert_eq!(lines[2], "*12  2026-09-21  22:25:56  7.2.11-arch1-1  manual    before the mesa upgrade");
    }

    #[test]
    fn wrapped_descriptions_line_up() {
        let mut s = snapshot(1);
        s.description = "word ".repeat(12);
        let text = format_table(&[&s], PLAIN, 80, true, false);
        let lines: Vec<&str> = text.lines().collect();
        let indent = lines[1].find("word").unwrap();
        assert!(lines.len() > 2);
        assert!(lines[2..].iter().all(|l| l.find("word") == Some(indent)));
        assert!(lines.iter().all(|l| display_width(l) <= 80));
    }

    #[test]
    fn colour_marks_kept() {
        let mut s = snapshot(1);
        s.keep = true;
        let text = format_table(&[&s], Style { enabled: true }, 120, false, false);
        assert!(text.contains("\x1b[1;32m*\x1b[0m1"));
    }
}
