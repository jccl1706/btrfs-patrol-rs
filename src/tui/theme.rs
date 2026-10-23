// SPDX-License-Identifier: GPL-3.0-or-later
//! How the TUI looks: Tokyo Night colours, Nerd Font icons, and the small
//! formatting rules (times, sizes) the panels share.
//!
//! Each has a fallback. Without a true-colour terminal the palette is the
//! terminal's own ANSI colours; on the Linux console or a non-UTF-8 locale
//! the icons are letters; with `--color never` nothing is coloured and the
//! selection is reverse video - nothing is told apart by colour alone.

use chrono::{DateTime, Datelike, FixedOffset, Local};
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub fg: Color,
    pub muted: Color,
    pub subtle: Color,
    pub border: Color,
    pub selection: Color,
    pub blue: Color,
    pub cyan: Color,
    pub purple: Color,
    pub green: Color,
    pub yellow: Color,
    pub orange: Color,
    pub red: Color,
    /// Text on a coloured pill.
    pub on_pill: Color,
}

/// Tokyo Night (night), the palette of the rest of the desktop.
pub const TOKYO_NIGHT: Palette = Palette {
    fg: Color::Rgb(0xc0, 0xca, 0xf5),
    muted: Color::Rgb(0x56, 0x5f, 0x89),
    subtle: Color::Rgb(0xa9, 0xb1, 0xd6),
    border: Color::Rgb(0x3b, 0x42, 0x61),
    selection: Color::Rgb(0x28, 0x34, 0x57),
    blue: Color::Rgb(0x7a, 0xa2, 0xf7),
    cyan: Color::Rgb(0x7d, 0xcf, 0xff),
    purple: Color::Rgb(0xbb, 0x9a, 0xf7),
    green: Color::Rgb(0x9e, 0xce, 0x6a),
    yellow: Color::Rgb(0xe0, 0xaf, 0x68),
    orange: Color::Rgb(0xff, 0x9e, 0x64),
    red: Color::Rgb(0xf7, 0x76, 0x8e),
    on_pill: Color::Rgb(0x1a, 0x1b, 0x26),
};

/// The terminal's own colours, for terminals without true colour.
pub const ANSI: Palette = Palette {
    fg: Color::Reset,
    muted: Color::DarkGray,
    subtle: Color::Gray,
    border: Color::DarkGray,
    selection: Color::DarkGray,
    blue: Color::Blue,
    cyan: Color::Cyan,
    purple: Color::Magenta,
    green: Color::Green,
    yellow: Color::Yellow,
    orange: Color::LightRed,
    red: Color::Red,
    on_pill: Color::Black,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Icons {
    pub logo: &'static str,
    pub disk: &'static str,
    pub pacman: &'static str,
    pub timer: &'static str,
    pub manual: &'static str,
    pub rollback: &'static str,
    pub kept: &'static str,
    pub ok: &'static str,
    pub warn: &'static str,
    pub folder: &'static str,
    pub file: &'static str,
    pub search: &'static str,
    pub open: &'static str,
    pub closed: &'static str,
    pub bar: &'static str,
    pub arrow: &'static str,
    pub enter: &'static str,
    pub updown: &'static str,
    pub mark: &'static str,
    pub dot: &'static str,
    pub dash: &'static str,
}

pub const NERD: Icons = Icons {
    logo: "\u{F02DA}",     // nf-md-history
    disk: "\u{F02CA}",     // nf-md-harddisk
    pacman: "\u{F03D7}",   // nf-md-package_variant
    timer: "\u{F0150}",    // nf-md-clock_outline
    manual: "\u{F0100}",   // nf-md-camera
    rollback: "\u{F006F}", // nf-md-backup_restore
    kept: "\u{F033E}",     // nf-md-lock
    ok: "\u{F05E0}",       // nf-md-check_circle
    warn: "\u{F0028}",     // nf-md-alert_circle
    folder: "\u{F024B}",   // nf-md-folder
    file: "\u{F0214}",     // nf-md-file
    search: "\u{F0349}",   // nf-md-magnify
    open: "▾",
    closed: "▸",
    bar: "▌",
    arrow: "→",
    enter: "⏎",
    updown: "↑↓",
    mark: "◆",
    dot: "·",
    dash: "—",
};

/// Box-drawing and arrows still work; only the Nerd Font icons go.
pub const UNICODE: Icons = Icons {
    logo: "◷",
    disk: "◫",
    pacman: "P",
    timer: "T",
    manual: "M",
    rollback: "R",
    kept: "*",
    ok: "✓",
    warn: "!",
    folder: "/",
    file: " ",
    search: "/",
    ..NERD
};

pub const ASCII: Icons = Icons {
    logo: "#",
    disk: "",
    open: "v",
    closed: ">",
    bar: ">",
    arrow: "->",
    enter: "enter",
    updown: "up/dn",
    mark: "+",
    ok: "ok",
    dot: "-",
    dash: "-",
    ..UNICODE
};

#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub color: bool,
    pub palette: Palette,
    pub icons: Icons,
    /// Rounded box-drawing borders; plain ASCII ones without UTF-8.
    pub unicode: bool,
}

impl Look {
    /// The look for this terminal: true colour if it says so, Nerd Font
    /// icons unless it is the Linux console, ASCII without a UTF-8 locale.
    pub fn detect(color: bool) -> Look {
        let truecolor = std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit");
        let console = std::env::var("TERM").is_ok_and(|t| t == "linux");
        let unicode = locale_is_utf8();
        let icons = match (unicode, console) {
            (false, _) => ASCII,
            (true, true) => UNICODE,
            (true, false) => NERD,
        };
        Look { color, palette: if truecolor { TOKYO_NIGHT } else { ANSI }, icons, unicode }
    }

    pub fn fg(&self, color: Color) -> Style {
        if self.color { Style::new().fg(color) } else { Style::new() }
    }

    pub fn text(&self) -> Style {
        self.fg(self.palette.fg)
    }

    pub fn muted(&self) -> Style {
        self.fg(self.palette.muted)
    }

    pub fn bold(&self, color: Color) -> Style {
        self.fg(color).add_modifier(Modifier::BOLD)
    }

    pub fn border(&self, focused: bool) -> Style {
        self.fg(if focused { self.palette.blue } else { self.palette.border })
    }

    /// A coloured badge: dark text on `color`, or bold brackets without colour.
    pub fn pill(&self, color: Color) -> Style {
        if self.color {
            Style::new().fg(self.palette.on_pill).bg(color).add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        }
    }

    pub fn selected(&self) -> Style {
        if self.color {
            Style::new().bg(self.palette.selection)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn kind_icon(&self, kind: &str) -> &'static str {
        match kind_class(kind) {
            KindClass::Package => self.icons.pacman,
            KindClass::Timer => self.icons.timer,
            KindClass::Rollback => self.icons.rollback,
            KindClass::Manual => self.icons.manual,
        }
    }

    pub fn kind_color(&self, kind: &str) -> Color {
        match kind_class(kind) {
            KindClass::Package => self.palette.blue,
            KindClass::Timer => self.palette.muted,
            KindClass::Rollback => self.palette.orange,
            KindClass::Manual => self.palette.purple,
        }
    }
}

enum KindClass {
    Package,
    Timer,
    Rollback,
    Manual,
}

fn kind_class(kind: &str) -> KindClass {
    match kind {
        k if k.starts_with("pacman") || k.starts_with("dnf") => KindClass::Package,
        "timer" => KindClass::Timer,
        "rollback" => KindClass::Rollback,
        _ => KindClass::Manual,
    }
}

pub fn locale_is_utf8() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .is_some_and(|v| {
            let v = v.to_lowercase();
            v.contains("utf-8") || v.contains("utf8")
        })
}

/// When, in the list: the time today, "yesterday", the date this year,
/// the full date before that.
pub fn when(created: &DateTime<FixedOffset>, now: &DateTime<Local>) -> String {
    let local = created.with_timezone(&Local);
    let days = (now.date_naive() - local.date_naive()).num_days();
    match days {
        0 => local.format("%H:%M").to_string(),
        1 => "yesterday".into(),
        _ if local.year() == now.year() => local.format("%b %d").to_string(),
        _ => local.format("%Y-%m-%d").to_string(),
    }
}

/// How long ago, roughly: "just now", "12 min ago", "5 h ago", "3 days ago".
pub fn ago(created: &DateTime<FixedOffset>, now: &DateTime<Local>) -> String {
    let seconds = (now.fixed_offset() - *created).num_seconds();
    match seconds {
        s if s < 0 => "in the future (clock?)".into(),
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s if s < 86_400 * 2 => "1 day ago".into(),
        s if s < 86_400 * 60 => format!("{} days ago", s / 86_400),
        s => format!("{} months ago", s / (86_400 * 30)),
    }
}

/// Bytes in binary units: "0 B", "512 KiB", "312.4 MiB", "1.2 GiB".
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let number = if unit == 0 || value >= 100.0 { format!("{value:.0}") } else { format!("{value:.1}") };
    format!("{} {}", number.strip_suffix(".0").unwrap_or(&number), UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).unwrap()
    }

    #[test]
    fn whens() {
        let now = Local.from_local_datetime(&at("2026-10-23T15:00:00+00:00").naive_utc()).unwrap();
        let local = |t: &str| Local.from_local_datetime(&at(t).naive_utc()).unwrap().fixed_offset();
        assert_eq!(when(&local("2026-10-23T09:05:00+00:00"), &now), "09:05");
        assert_eq!(when(&local("2026-10-22T23:59:00+00:00"), &now), "yesterday");
        assert_eq!(when(&local("2026-10-02T10:00:00+00:00"), &now), "Oct 02");
        assert_eq!(when(&local("2025-12-31T10:00:00+00:00"), &now), "2025-12-31");
        assert_eq!(ago(&local("2026-10-23T14:59:30+00:00"), &now), "just now");
        assert_eq!(ago(&local("2026-10-23T14:48:00+00:00"), &now), "12 min ago");
        assert_eq!(ago(&local("2026-10-23T10:00:00+00:00"), &now), "5 h ago");
        assert_eq!(ago(&local("2026-10-20T10:00:00+00:00"), &now), "3 days ago");
    }

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(50 << 30), "50 GiB");
        assert_eq!(size(512 * 1024), "512 KiB");
        assert_eq!(size(327_576_371), "312 MiB");
        assert_eq!(size(1_288_490_189), "1.2 GiB");
    }
}
