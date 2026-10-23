// SPDX-License-Identifier: GPL-3.0-or-later
//! Drawing the dashboard:
//!
//! ```text
//! ╭─ btrfs-patrol ───────────────────────── all  root  home ─╮
//! │ disk  41 / 476 GiB ━━━━──────── 9%     9 snapshots · 1 kept │
//! ╰──────────────────────────────────────────────────────────╯
//! ╭ Snapshots ───────────────────────╮╭ #6 ───────────────────╮
//! │▌ icon  6  yesterday  install ru… ││ pills, times, kernel,  │
//! │  icon  5  Oct 22     before hy…  ││ size, description      │
//! ╰──────────────────────────────────╯╰───────────────────────╯
//!  keys that apply right now - or the search box, or a message
//! ```
//!
//! The detail panel sits beside the list on wide terminals and under it on
//! narrow ones. A comparison takes the whole screen; details and help are
//! popups. Every row and tab drawn is recorded in `screen.hits` for clicks.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::difftree::DiffTree;
use super::screen::{ALL, Hit, Mode, Overlay, Page, Prompt, Screen, Tone, size_text};
use super::size::Size;
use super::theme::{self, Look};
use crate::diff::Change;
use crate::snapshot::Snapshot;
use crate::table::{display_width, pad, printable, shown_description, truncate, wrap_text};

/// Below this width the detail panel goes under the list.
const SIDE_BY_SIDE: u16 = 96;

const ASCII_BORDER: border::Set = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

impl Look {
    fn tone(&self, tone: Tone) -> Style {
        match tone {
            Tone::Plain => self.text(),
            Tone::Dim => self.muted(),
            Tone::Accent => self.fg(self.palette.blue),
            Tone::Heading => self.bold(self.palette.fg),
            Tone::Error => self.bold(self.palette.red),
            Tone::Ok => self.fg(self.palette.green),
        }
    }

    fn block(&self, title: Line<'static>, focused: bool) -> Block<'static> {
        let block = Block::bordered().border_style(self.border(focused)).title(title);
        if self.unicode { block.border_type(BorderType::Rounded) } else { block.border_set(ASCII_BORDER) }
    }

    /// The arrows and symbols in key names, or their ASCII stand-ins.
    fn keyname(&self, key: &str) -> String {
        if self.unicode {
            return key.to_owned();
        }
        key.replace("↑↓", "up/dn").replace("←→", "lt/rt").replace('⏎', "enter")
    }

    fn change_color(&self, change: Change) -> Color {
        match change {
            Change::Added => self.palette.green,
            Change::Removed => self.palette.red,
            Change::Modified => self.palette.yellow,
            Change::Renamed => self.palette.blue,
            Change::Attrs => self.palette.muted,
        }
    }
}

/// Draw the screen, and return how many rows the list, page or tree shows,
/// which is what PgUp and PgDn move by.
pub fn draw(frame: &mut Frame, screen: &mut Screen, look: Look) -> usize {
    screen.hits.clear();
    let [main, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let height = if let Some(Overlay::Diff(_)) = &screen.overlay {
        draw_diff(frame, main, screen, look)
    } else {
        let [header, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(main);
        draw_header(frame, header, screen, look);
        let (list, detail) = if body.width >= SIDE_BY_SIDE {
            let side = (body.width / 3).clamp(36, 48);
            let [list, detail] =
                Layout::horizontal([Constraint::Min(1), Constraint::Length(side)]).areas(body);
            (list, detail)
        } else {
            let [list, detail] = Layout::vertical([Constraint::Min(3), Constraint::Length(9)]).areas(body);
            (list, detail)
        };
        let rows = draw_list(frame, list, screen, look);
        draw_detail(frame, detail, screen, look);
        match &mut screen.overlay {
            Some(Overlay::Text(page)) => draw_popup(frame, main, page, look),
            _ => rows,
        }
    };
    draw_footer(frame, footer, screen, look);
    height
}

fn draw_header(frame: &mut Frame, area: Rect, screen: &mut Screen, look: Look) {
    let p = look.palette;
    let title = Line::from(vec![
        Span::styled(format!(" {} ", look.icons.logo), look.bold(p.blue)),
        Span::styled("btrfs-patrol ", look.bold(p.fg)),
    ]);
    let block = look.block(title, false);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The subvolume tabs, on the top border at the right.
    if screen.tabs.len() > 1 {
        let widths: Vec<u16> = screen.tabs.iter().map(|t| display_width(&printable(t)) as u16 + 2).collect();
        let total: u16 = widths.iter().sum::<u16>() + widths.len() as u16 - 1;
        if total + 20 < area.width {
            let mut x = area.right() - 2 - total;
            for (i, (tab, width)) in screen.tabs.iter().zip(widths).enumerate() {
                let style = if i == screen.tab { look.pill(p.blue) } else { look.muted() };
                let cell = Rect { x, y: area.y, width, height: 1 };
                frame.render_widget(
                    Paragraph::new(Span::styled(format!(" {} ", printable(tab)), style)),
                    cell,
                );
                screen.hits.push((cell, Hit::Tab(i)));
                x += width + 1;
            }
        }
    }

    let mut spans = vec![Span::raw(" ")];
    if let Some((used, total)) = screen.disk {
        let share = if total == 0 { 0.0 } else { used as f64 / total as f64 };
        let cells = 12;
        let filled = ((share * cells as f64).round() as usize).min(cells);
        let color = if share >= 0.9 {
            p.red
        } else if share >= 0.75 {
            p.yellow
        } else {
            p.green
        };
        let (on, off) = if look.unicode { ("━", "─") } else { ("#", "-") };
        if !look.icons.disk.is_empty() {
            spans.push(Span::styled(format!("{} ", look.icons.disk), look.fg(p.subtle)));
        }
        spans.push(Span::styled(format!("{} / {}  ", theme::size(used), theme::size(total)), look.text()));
        spans.push(Span::styled(on.repeat(filled), look.fg(color)));
        spans.push(Span::styled(off.repeat(cells - filled), look.fg(p.border)));
        spans.push(Span::styled(format!(" {:.0}%", share * 100.0), look.fg(color)));
        spans.push(Span::raw("     "));
    }
    let shown: Vec<&Snapshot> = screen.rows().collect();
    let kept = shown.iter().filter(|s| s.keep).count();
    spans.push(Span::styled(
        format!("{} snapshot{}", shown.len(), if shown.len() == 1 { "" } else { "s" }),
        look.text(),
    ));
    spans.push(Span::styled(format!(" {} ", look.icons.dot), look.muted()));
    spans.push(Span::styled(format!("{kept} kept"), look.fg(if kept > 0 { p.green } else { p.muted })));
    if let Some(total) = screen.snapshots.len().checked_sub(shown.len()).filter(|n| *n > 0) {
        spans.push(Span::styled(format!(" {} {total} hidden", look.icons.dot), look.muted()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

/// What a snapshot with no description says in the list.
fn placeholder(s: &Snapshot, look: &Look) -> &'static str {
    match s.kind.as_str() {
        "timer" => "daily snapshot",
        "rollback" => "the system before a rollback",
        k if k.starts_with("pacman") => "pacman transaction",
        _ => look.icons.dash,
    }
}

fn draw_list(frame: &mut Frame, area: Rect, screen: &mut Screen, look: Look) -> usize {
    let p = look.palette;
    let mut title = vec![Span::styled(" Snapshots ", look.bold(p.fg))];
    if let Some(query) = &screen.query {
        title.push(Span::styled(
            format!("{} {} {} ", look.icons.dot, look.icons.search, printable(query)),
            look.fg(p.yellow),
        ));
    }
    let block = look.block(Line::from(title), screen.mode != Mode::Overlay);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = inner.height as usize;
    screen.scroll(height);

    if screen.visible.is_empty() {
        let mut lines = vec![Line::raw("")];
        match (&screen.query, &screen.query_error) {
            (Some(_), Some(error)) => {
                lines.push(Line::styled(format!("  {}", printable(error)), look.fg(p.red)))
            }
            (Some(query), None) => {
                lines.push(Line::styled(format!("  nothing matches \"{}\"", printable(query)), look.muted()))
            }
            (None, _) => {
                lines.push(Line::styled("  no snapshots yet", look.bold(p.fg)));
                lines.push(Line::styled(
                    "  they arrive with every pacman transaction and once a day",
                    look.muted(),
                ));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
        return height;
    }

    let rows: Vec<&Snapshot> = screen.rows().collect();
    let all_tab = screen.tabs.get(screen.tab).is_some_and(|t| t == ALL);
    let show_subvolume = screen.show_subvolume && all_tab && inner.width >= 60;
    let show_size = screen.can_measure && inner.width >= 60;
    let id_width = rows.iter().map(|s| s.id.to_string().len()).max().unwrap_or(1);
    let whens: Vec<String> = rows.iter().map(|s| theme::when(&s.created, &screen.now)).collect();
    let when_width = whens.iter().map(|w| w.len()).max().unwrap_or(0);
    let sub_width = rows.iter().map(|s| display_width(&printable(&s.subvolume))).max().unwrap_or(0);
    // bar, space, icon, space, id, space, lock, space, when, gap
    let mut fixed = 1 + 1 + 1 + 1 + id_width + 1 + 1 + 2 + when_width + 2;
    if show_subvolume {
        fixed += sub_width + 2;
    }
    let size_width = 10;
    if show_size {
        fixed += size_width + 1;
    }
    let description_width = (inner.width as usize).saturating_sub(fixed).max(1);

    let mut lines = Vec::new();
    let mut hits = Vec::new();
    for (n, s) in rows.iter().enumerate().skip(screen.offset).take(height) {
        let selected = n == screen.cursor;
        let marked = screen.mark == Some(s.id);
        let kind = look.kind_color(&s.kind);
        let mut spans = vec![
            if marked {
                Span::styled(look.icons.mark, look.fg(p.yellow))
            } else if selected {
                Span::styled(look.icons.bar, look.fg(p.blue))
            } else {
                Span::raw(" ")
            },
            Span::raw(" "),
            Span::styled(look.kind_icon(&s.kind), look.fg(kind)),
            Span::raw(" "),
            Span::styled(
                pad(&s.id.to_string(), id_width, true),
                if selected { look.bold(p.fg) } else { look.text() },
            ),
            Span::raw(" "),
            Span::styled(if s.keep { look.icons.kept } else { " " }, look.fg(p.green)),
            Span::raw("  "),
            Span::styled(pad(&whens[n], when_width, false), look.fg(p.subtle)),
            Span::raw("  "),
        ];
        if show_subvolume {
            spans.push(Span::styled(pad(&printable(&s.subvolume), sub_width, false), look.fg(p.cyan)));
            spans.push(Span::raw("  "));
        }
        let text = printable(&s.description);
        let (text, style) = if text.trim().is_empty() {
            (placeholder(s, &look).to_owned(), look.muted())
        } else {
            (text, if selected { look.bold(p.fg) } else { look.text() })
        };
        let text = truncate(&text, description_width);
        let filler = description_width.saturating_sub(display_width(&text));
        spans.push(Span::styled(text, style));
        spans.push(Span::raw(" ".repeat(filler)));
        if show_size {
            let size = match screen.sizes.get(&s.id) {
                Some(Size::Bytes(b)) => theme::size(*b),
                Some(Size::Measuring) => "…".into(),
                _ => String::new(),
            };
            spans.push(Span::styled(format!(" {}", pad(&size, size_width, true)), look.muted()));
        }
        let line = Line::from(spans);
        lines.push(if selected { line.style(look.selected()) } else { line });
        let y = inner.y + (n - screen.offset) as u16;
        hits.push((Rect { x: inner.x, y, width: inner.width, height: 1 }, Hit::Row(n)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
    screen.hits.extend(hits);
    height
}

fn draw_detail(frame: &mut Frame, area: Rect, screen: &Screen, look: Look) {
    let p = look.palette;
    let Some(s) = screen.current() else {
        frame.render_widget(look.block(Line::raw(""), false), area);
        return;
    };
    let block = look.block(Line::styled(format!(" #{} ", s.id), look.bold(p.fg)), false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;

    let pill = |text: String, color: Color| Span::styled(format!(" {text} "), look.pill(color));
    let mut badges = vec![
        Span::raw(" "),
        pill(format!("{} {}", look.kind_icon(&s.kind), printable(&s.kind)), look.kind_color(&s.kind)),
        Span::raw(" "),
        pill(printable(&s.subvolume), p.cyan),
    ];
    if s.keep {
        badges.extend([Span::raw(" "), pill(format!("{} kept", look.icons.kept), p.green)]);
    }
    let field = |name: &str, value: Span<'static>| {
        Line::from(vec![Span::styled(format!(" {name:<9}"), look.muted()), value])
    };
    let mut lines = vec![Line::from(badges), Line::raw("")];
    lines.push(field(
        "created",
        Span::styled(s.created.with_timezone(&chrono::Local).format("%b %d %H:%M").to_string(), look.text()),
    ));
    lines.push(field("", Span::styled(theme::ago(&s.created, &screen.now), look.fg(p.subtle))));
    lines.push(field(
        "kernel",
        Span::styled(truncate(&printable(&s.kernel), width.saturating_sub(10)), look.text()),
    ));
    lines.push(field(
        "boot",
        match s.boot {
            Some(_) => Span::styled(format!("{} kernel stored", look.icons.ok), look.fg(p.green)),
            None => Span::styled("kernel not stored", look.muted()),
        },
    ));
    let size = size_text(screen.sizes.get(&s.id), screen.can_measure);
    let size_style =
        if matches!(screen.sizes.get(&s.id), Some(Size::Bytes(_))) { look.text() } else { look.muted() };
    lines.push(field("size", Span::styled(size, size_style)));
    if let Some(mark) = screen.mark {
        lines.push(Line::raw(""));
        let text = if mark == s.id {
            format!(" {} marked: c on another to compare", look.icons.mark)
        } else {
            format!(" {} c: compare with #{mark}", look.icons.mark)
        };
        lines.push(Line::styled(truncate(&text, width), look.fg(p.yellow)));
    }
    lines.push(Line::raw(""));
    let description = shown_description(s);
    for line in wrap_text(&description, width.saturating_sub(2)) {
        lines.push(Line::styled(format!(" {line}"), look.text()));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A centred popup of text, scrolled; returns its height in lines.
fn draw_popup(frame: &mut Frame, area: Rect, page: &mut Page, look: Look) -> usize {
    let p = look.palette;
    let width = area.width.saturating_sub(4).min(84);
    let height = area.height.saturating_sub(2).min(page.lines.len() as u16 + 4);
    let popup =
        Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height };
    frame.render_widget(Clear, popup);
    let shown = height.saturating_sub(4) as usize;
    page.scroll = page.scroll.min(page.lines.len().saturating_sub(shown.max(1)));
    let mut block = look
        .block(Line::styled(format!(" {} ", page.title), look.bold(p.fg)), true)
        .padding(ratatui::widgets::Padding::new(2, 2, 1, 1));
    if page.lines.len() > shown {
        let end = (page.scroll + shown).min(page.lines.len());
        block = block.title_bottom(
            Line::styled(format!(" {}-{} of {} ", page.scroll + 1, end, page.lines.len()), look.muted())
                .right_aligned(),
        );
    }
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines: Vec<Line> = page
        .lines
        .iter()
        .skip(page.scroll)
        .take(shown)
        .map(|(tone, text)| Line::styled(truncate(text, inner.width as usize), look.tone(*tone)))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    shown
}

fn draw_diff(frame: &mut Frame, area: Rect, screen: &mut Screen, look: Look) -> usize {
    let p = look.palette;
    let Some(Overlay::Diff(tree)) = &mut screen.overlay else { return 1 };
    let block = look.block(Line::styled(format!(" {} ", tree.title), look.bold(p.fg)), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut summary = vec![Span::raw(" ")];
    for (change, count) in Change::ALL.iter().zip(tree.summary) {
        if count > 0 {
            summary.push(Span::styled(
                format!(" {}{count} {} ", change.marker(), change.name()),
                look.pill(look.change_color(*change)),
            ));
            summary.push(Span::raw(" "));
        }
    }
    if tree.nodes.is_empty() {
        summary.push(Span::styled("no differences", look.muted()));
    }
    let [top, _, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    frame.render_widget(Paragraph::new(Line::from(summary)), top);

    let height = body.height as usize;
    tree.scroll_to(height);
    let rows = tree.rows();
    let mut lines = Vec::new();
    let mut hits = Vec::new();
    for (n, &i) in rows.iter().enumerate().skip(tree.scroll).take(height) {
        lines.push(tree_line(tree, i, n == tree.cursor, body.width as usize, look));
        let y = body.y + (n - tree.scroll) as u16;
        hits.push((Rect { x: body.x, y, width: body.width, height: 1 }, Hit::TreeRow(n)));
    }
    frame.render_widget(Paragraph::new(lines), body);
    screen.hits.extend(hits);
    height
}

fn tree_line(tree: &DiffTree, i: usize, selected: bool, width: usize, look: Look) -> Line<'static> {
    let p = look.palette;
    let node = &tree.nodes[i];
    let folder = !node.children.is_empty();
    let mut spans = vec![Span::raw(" "), Span::raw("  ".repeat(node.depth))];
    let mut used = 1 + 2 * node.depth;
    if folder {
        let arrow = if tree.open.contains(&i) { look.icons.open } else { look.icons.closed };
        spans.push(Span::styled(format!("{arrow} {} ", look.icons.folder), look.fg(p.blue)));
        spans.push(Span::styled(format!("{}/", printable(&node.name)), look.bold(p.fg)));
        used += 4 + display_width(&printable(&node.name)) + 1;
    } else {
        let (marker, color) = match node.change {
            Some(c) => (c.marker().to_string(), look.change_color(c)),
            None => (" ".into(), p.muted),
        };
        spans.push(Span::styled(format!("{marker} "), look.bold(color)));
        let name = printable(&node.name);
        used += 2 + display_width(&name);
        spans.push(Span::styled(
            name,
            look.fg(if node.change == Some(Change::Removed) { p.muted } else { p.fg }),
        ));
        if node.change == Some(Change::Renamed) {
            let from = format!("  {} {}", if look.unicode { "←" } else { "<-" }, printable(&node.old_path));
            used += display_width(&from);
            spans.push(Span::styled(from, look.muted()));
        }
    }
    if folder {
        // What is inside, by kind of change, at the right.
        let counts: Vec<Span> = Change::ALL
            .iter()
            .zip(node.counts)
            .filter(|(_, n)| *n > 0)
            .map(|(c, n)| Span::styled(format!(" {}{n}", c.marker()), look.fg(look.change_color(*c))))
            .collect();
        let counts_width: usize = counts.iter().map(|s| s.width()).sum();
        if used + counts_width + 2 <= width {
            spans.push(Span::raw(" ".repeat(width - used - counts_width - 1)));
            spans.extend(counts);
        }
    }
    let line = Line::from(spans);
    if selected { line.style(look.selected()) } else { line }
}

fn draw_footer(frame: &mut Frame, area: Rect, screen: &Screen, look: Look) {
    let p = look.palette;
    let line = if let (Mode::Prompt, Some(prompt)) = (screen.mode, screen.prompt) {
        match prompt {
            Prompt::ConfirmDelete(id) => {
                let what =
                    screen.snapshots.iter().find(|s| s.id == id).map(shown_description).unwrap_or_default();
                Line::from(vec![
                    Span::styled(format!(" delete #{id} "), look.pill(p.red)),
                    Span::styled(format!("  {}  ", truncate(&what, 40)), look.text()),
                    Span::styled("y", look.bold(p.red)),
                    Span::styled(" delete   any other key keeps it", look.muted()),
                ])
            }
            Prompt::NewSnapshot | Prompt::Describe(_) => {
                let label = match prompt {
                    Prompt::NewSnapshot => {
                        let tab = &screen.tabs[screen.tab];
                        if tab == ALL {
                            " new snapshot ".to_owned()
                        } else {
                            format!(" new snapshot of {tab} ")
                        }
                    }
                    _ => " describe ".to_owned(),
                };
                Line::from(vec![
                    Span::styled(label, look.pill(p.green)),
                    Span::styled("  ", look.text()),
                    Span::styled(screen.input.clone(), look.bold(p.fg)),
                    Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
                    Span::styled("   ⏎ ok   esc cancel", look.muted()),
                ])
            }
        }
    } else if screen.mode == Mode::Search {
        let mut spans = vec![
            Span::styled(format!(" {} search ", look.icons.search), look.pill(p.blue)),
            Span::raw(" "),
            Span::styled(screen.input.clone(), look.bold(p.fg)),
            Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
        ];
        let hint = match &screen.query_error {
            Some(error) => Span::styled(format!("   {}", printable(error)), look.fg(p.red)),
            None => Span::styled("   text, or kind=pacman  keep=yes  10-20", look.muted()),
        };
        spans.push(hint);
        Line::from(spans)
    } else if let Some((tone, text)) = &screen.message {
        Line::styled(format!(" {}", printable(text)), look.tone(*tone))
    } else {
        let mut spans = vec![Span::raw(" ")];
        for (key, what) in screen.keys() {
            spans.push(Span::styled(look.keyname(key), look.bold(p.blue)));
            spans.push(Span::styled(format!(" {what}   "), look.muted()));
        }
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::diff::{Comparison, interpret};
    use crate::snapshot::Store;
    use crate::snapshot::tests::snapshot;
    use crate::tui::screen::Key;
    use crate::tui::theme::{ANSI, ASCII, NERD};

    fn render(screen: &mut Screen, width: u16, height: u16, look: Look) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                draw(f, screen, look);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn screen() -> Screen {
        let mut all: Vec<Snapshot> = (1..=3).map(snapshot).collect();
        all[2].description = "before the mesa upgrade".into();
        all[2].kind = "pacman-pre".into();
        all[1].keep = true;
        let mut s = Screen::new(all, vec!["root".into()], false, &Store::new("/.snapshots"));
        s.now = s.snapshots[0].created.with_timezone(&chrono::Local) + chrono::Duration::hours(3);
        s.disk = Some((50 << 30, 200 << 30));
        s
    }

    const PLAIN: Look = Look { color: false, palette: ANSI, icons: NERD, unicode: true };

    #[test]
    fn the_dashboard() {
        let mut s = screen();
        let lines = render(&mut s, 120, 16, PLAIN);
        assert!(lines[0].starts_with("╭ \u{F02DA} btrfs-patrol "), "{:?}", lines[0]);
        assert!(lines[1].contains("50 GiB / 200 GiB  ━━━─────────"), "{:?}", lines[1]);
        assert!(lines[1].contains("25%") && lines[1].contains("3 snapshots · 1 kept"), "{:?}", lines[1]);
        assert!(lines[3].starts_with("╭ Snapshots ─"), "{:?}", lines[3]);
        assert!(lines[3].contains("╭ #3 ─"), "{:?}", lines[3]);
        let row = &lines[6];
        assert!(row.starts_with("│▌ \u{F03D7} 3    "), "{row:?}");
        assert!(row.contains("before the mesa upgrade"), "{row:?}");
        assert!(lines[5].contains("2 \u{F033E}"), "kept: {:?}", lines[5]);
        assert!(lines[4].contains("daily snapshot") || lines[4].contains("—"), "{:?}", lines[4]);
        assert!(lines[4].contains("│"));
        assert!(lines[15].contains("⏎ details") && lines[15].contains("q quit"), "{:?}", lines[15]);
        assert!(lines.iter().any(|l| l.contains("pacman-pre") && l.contains("root")), "badges");
        assert!(lines.iter().any(|l| l.contains("run as root to measure")));
    }

    #[test]
    fn narrow_terminals_stack_the_panels() {
        let mut s = screen();
        let lines = render(&mut s, 80, 24, PLAIN);
        assert!(!lines[3].contains("#3"), "{:?}", lines[3]);
        assert!(lines.iter().skip(4).any(|l| l.starts_with("╭ #3 ")), "{lines:#?}");
    }

    #[test]
    fn ascii_when_the_locale_is_not_utf8() {
        let mut s = screen();
        let look = Look { color: false, palette: ANSI, icons: ASCII, unicode: false };
        let lines = render(&mut s, 120, 16, look);
        assert!(lines[0].starts_with("+ # btrfs-patrol "), "{:?}", lines[0]);
        assert!(lines[6].starts_with("|> P 3"), "{:?}", lines[6]);
        assert!(lines.iter().all(|l| l.is_ascii()), "{lines:#?}");
    }

    #[test]
    fn rows_and_tabs_are_clickable() {
        let mut all: Vec<Snapshot> = (1..=3).map(snapshot).collect();
        all[0].subvolume = "home".into();
        let mut s = Screen::new(all, vec!["root".into(), "home".into()], true, &Store::new("/x"));
        render(&mut s, 120, 16, PLAIN);
        assert!(s.hits.iter().any(|(_, h)| *h == Hit::Tab(2)));
        let (area, _) = *s.hits.iter().find(|(_, h)| *h == Hit::Row(0)).unwrap();
        s.click(area.x + 3, area.y);
        assert_eq!(s.current().unwrap().id, 1);
        let (tab, _) = *s.hits.iter().find(|(_, h)| *h == Hit::Tab(2)).unwrap();
        s.click(tab.x, tab.y);
        assert_eq!(s.tabs[s.tab], "home");
    }

    #[test]
    fn the_comparison_takes_the_screen() {
        let mut s = screen();
        let comparison = Comparison {
            older_id: 1,
            newer_id: 3,
            entries: interpret(include_str!("../../tests/data/receive-dump-known-changes.txt")),
        };
        s.show_comparison(&comparison, "root");
        let lines = render(&mut s, 100, 20, PLAIN);
        assert!(lines[0].starts_with("╭ #1 → #3 · root "), "{:?}", lines[0]);
        assert!(lines[1].contains("+3 added") && lines[1].contains("-2 removed"), "{:?}", lines[1]);
        assert!(lines[3].contains("▾ \u{F024B} difftest/"), "{:?}", lines[3]);
        assert!(lines[3].ends_with("+3 -2 ~1 >1 a1 │"), "{:?}", lines[3]);
        assert!(lines.iter().any(|l| l.contains("> newname.txt  ← difftest/oldname.txt")));
        assert!(lines[19].contains("fold"));
        s.key(Key::Char('-'), 10);
        let lines = render(&mut s, 100, 20, PLAIN);
        assert!(lines[3].contains("▸ \u{F024B} difftest/"));
        assert!(!lines[4].contains("added"));
    }

    #[test]
    fn the_search_box() {
        let mut s = screen();
        s.key(Key::Char('/'), 5);
        for c in "kind=".chars() {
            s.key(Key::Char(c), 5);
        }
        let lines = render(&mut s, 120, 16, PLAIN);
        assert!(lines[15].contains("search  kind="), "{:?}", lines[15]);
        assert!(lines[15].contains("must be followed by some text"));
        assert!(lines[5].contains("must be followed by some text"), "{:?}", lines[5]);
    }

    #[test]
    fn popups_sit_over_the_dashboard() {
        let mut s = screen();
        s.key(Key::Char('?'), 5);
        let lines = render(&mut s, 120, 30, PLAIN);
        assert!(lines.iter().any(|l| l.contains("╭ help ")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("of ")), "scroll position shown when it overflows");
    }

    #[test]
    fn odd_sizes_do_not_panic() {
        let mut s = screen();
        for (w, h) in [(10, 3), (1, 1), (40, 5), (95, 12), (97, 12), (300, 80)] {
            render(&mut s, w, h, PLAIN);
            s.key(Key::Char('?'), 3);
            render(&mut s, w, h, PLAIN);
            s.key(Key::Esc, 3);
        }
    }
}
