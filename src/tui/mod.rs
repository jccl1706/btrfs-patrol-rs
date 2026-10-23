// SPDX-License-Identifier: GPL-3.0-or-later
//! `btrfs-patrol tui`: a dashboard of the snapshots.

mod difftree;
mod fuzzy;
mod screen;
mod size;
mod theme;
mod view;

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::execute;

use self::screen::{Action, Key, Screen, Tone};
use self::size::{Size, Sizer};
use self::theme::Look;
use crate::error::Result;
use crate::{App, bail, err, is_root, manage, rollback, table};

/// How often the store and the disk are looked at while nothing happens.
const WATCH: Duration = Duration::from_secs(1);
/// How long a key is waited for before looking for finished sizes.
const TICK: Duration = Duration::from_millis(200);

pub fn run(app: &App) -> Result<u8> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!("tui needs a terminal");
    }
    // Everything that can fail on the way in fails here, before the terminal
    // is taken over, so the error reaches an ordinary screen.
    let snapshots = app.store.load_all()?;
    let managed = app.config.managed().into_iter().map(|s| s.name).collect();
    let mut screen = Screen::new(snapshots, managed, false, &app.store);
    screen.show_subvolume = app.show_subvolume(&screen.snapshots);
    screen.can_measure = is_root();
    screen.can_change = is_root();
    screen.disk = size::disk_usage(&app.store.directory);
    let look = Look::detect(table::color_enabled(app.color, true));

    // init() restores the terminal if anything panics; mouse reporting is
    // ours to turn off, before that, or the shell after a panic would be
    // full of escape codes at every mouse move.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, app, &mut screen, look);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

enum Input {
    Key(Key),
    Quit,
    /// A key nothing here uses, such as F1.
    Ignored,
}

fn input(code: KeyCode, modifiers: KeyModifiers) -> Input {
    Input::Key(match code {
        // Raw mode turns Ctrl+C into an ordinary key; it must still quit.
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Input::Quit,
        KeyCode::Char(_) if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            return Input::Ignored;
        }
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Char(c) => Key::Char(c),
        _ => return Input::Ignored,
    })
}

fn reload(app: &App, screen: &mut Screen) {
    match app.store.load_all() {
        Ok(snapshots) => {
            screen.show_subvolume = app.show_subvolume(&snapshots);
            screen.set_snapshots(snapshots);
        }
        Err(e) => screen.message = Some((Tone::Error, e.0)),
    }
}

/// Say what is happening and draw it, before something slow.
fn busy(terminal: &mut DefaultTerminal, screen: &mut Screen, look: Look, text: &str) -> Result<()> {
    screen.message = Some((Tone::Accent, text.to_owned()));
    terminal
        .draw(|frame| {
            view::draw(frame, screen, look);
        })
        .map_err(|e| err!("terminal: {e}"))?;
    Ok(())
}

/// Reload after a change, and say how it went.
fn finish(app: &App, screen: &mut Screen, seen: &mut Option<std::time::SystemTime>, result: Result<String>) {
    reload(app, screen);
    *seen = app.store.modified();
    screen.message = Some(match result {
        Ok(text) => (Tone::Ok, text),
        Err(e) => (Tone::Error, e.0),
    });
}

fn event_loop(terminal: &mut DefaultTerminal, app: &App, screen: &mut Screen, look: Look) -> Result<u8> {
    let io = |e: std::io::Error| err!("terminal: {e}");
    let sizer = Sizer::start();
    // A prepared rollback, holding the top-level subvolume mounted while
    // its plan is on screen.
    let mut planned: Option<rollback::Plan> = None;
    let mut seen = app.store.modified();
    let mut watched = Instant::now();
    loop {
        for id in screen.unmeasured() {
            sizer.measure(id, app.store.subvolume(id));
        }
        for (id, result) in sizer.ready() {
            let size = match result {
                Ok(bytes) => Size::Bytes(bytes),
                Err(e) => Size::Failed(e.0),
            };
            screen.sizes.insert(id, size);
        }
        // Snapshots that a pacman hook or the timer takes appear without r.
        if watched.elapsed() >= WATCH {
            watched = Instant::now();
            screen.now = chrono::Local::now();
            screen.disk = size::disk_usage(&app.store.directory);
            let now = app.store.modified();
            if now != seen {
                seen = now;
                reload(app, screen);
            }
        }

        let mut page_height = 1;
        terminal.draw(|frame| page_height = view::draw(frame, screen, look)).map_err(io)?;
        if !event::poll(TICK).map_err(io)? {
            continue;
        }
        let action = match event::read().map_err(io)? {
            Event::Key(event) if event.kind == KeyEventKind::Press => {
                match input(event.code, event.modifiers) {
                    Input::Key(key) => screen.key(key, page_height),
                    Input::Quit => Action::Quit,
                    Input::Ignored => Action::None,
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => screen.click(mouse.column, mouse.row),
                MouseEventKind::ScrollDown => screen.wheel(true, page_height),
                MouseEventKind::ScrollUp => screen.wheel(false, page_height),
                _ => Action::None,
            },
            _ => Action::None,
        };
        match action {
            Action::None => {}
            Action::Quit => return Ok(0),
            Action::Reload => {
                reload(app, screen);
                seen = app.store.modified();
                if screen.message.is_none() {
                    screen.message = Some((Tone::Ok, "reloaded".into()));
                }
            }
            Action::Snapshot(description, subvolume) => {
                busy(terminal, screen, look, "taking a snapshot…")?;
                let names: Vec<String> = subvolume.into_iter().collect();
                let result =
                    manage::selected_subvolumes(&app.config, &names, "manual").and_then(|subvolumes| {
                        manage::take_snapshots(app, &subvolumes, "manual", &description, false)
                    });
                reload(app, screen);
                seen = app.store.modified();
                match result {
                    Ok(taken) => {
                        let ids: Vec<String> = taken.created.iter().map(|s| format!("#{}", s.id)).collect();
                        if let Some(last) = taken.created.last() {
                            screen.select(last.id);
                        }
                        let mut text = format!("created {}", ids.join(", "));
                        if !taken.pruned.is_empty() {
                            text.push_str(&format!(", pruned {}", taken.pruned.len()));
                        }
                        screen.message = Some(match taken.warnings.first() {
                            Some(w) => (Tone::Error, format!("{text}; {w}")),
                            None => (Tone::Ok, text),
                        });
                    }
                    Err(e) => screen.message = Some((Tone::Error, e.0)),
                }
            }
            Action::Describe(id, text) => {
                let result = manage::describe(app, id, &text);
                finish(app, screen, &mut seen, result.map(|()| format!("#{id} described")));
            }
            Action::Keep(id, keep) => {
                let result = manage::set_keep(app, &[id], keep);
                let done = if keep { "kept: never pruned" } else { "can be pruned again" };
                finish(app, screen, &mut seen, result.map(|_| format!("#{id} {done}")));
            }
            Action::Delete(id) => {
                busy(terminal, screen, look, &format!("deleting #{id}…"))?;
                let result = manage::delete(app, &[id]).map(|_| {
                    let (_, warnings) = manage::sync_entries(app);
                    warnings.into_iter().fold(format!("#{id} deleted"), |m, w| format!("{m}; {w}"))
                });
                finish(app, screen, &mut seen, result);
            }
            Action::PlanRollback(id) => {
                busy(terminal, screen, look, &format!("checking a rollback to #{id}…"))?;
                let prepared = app.store.load(id).and_then(|target| rollback::prepare(app, &target));
                match prepared {
                    Ok(plan) => {
                        let mut lines: Vec<(Tone, String)> =
                            plan.describe().into_iter().map(|l| (Tone::Plain, l)).collect();
                        lines.extend(plan.warnings.iter().map(|w| (Tone::Error, format!("warning: {w}"))));
                        screen.message = None;
                        screen.show_rollback(id, lines);
                        planned = Some(plan);
                    }
                    Err(e) => screen.message = Some((Tone::Error, e.0)),
                }
            }
            Action::CancelRollback => planned = None,
            Action::Rollback => {
                if let Some(plan) = planned.take() {
                    let id = plan.target.id;
                    busy(terminal, screen, look, &format!("rolling back to #{id}…"))?;
                    let result = plan.execute(app);
                    drop(plan);
                    let result = result.map(|saved| {
                        let (_, warnings) = manage::sync_entries(app);
                        let text = format!(
                            "rolled back to #{id}: reboot to start it. The previous state is #{} (R on it undoes this)",
                            saved.id
                        );
                        warnings.into_iter().fold(text, |m, w| format!("{m}; {w}"))
                    });
                    finish(app, screen, &mut seen, result);
                }
            }
            Action::Compare(older, newer) => {
                screen.message = Some((Tone::Accent, format!("comparing #{older} → #{newer}…")));
                terminal
                    .draw(|frame| {
                        view::draw(frame, screen, look);
                    })
                    .map_err(io)?;
                let subvolume = screen
                    .snapshots
                    .iter()
                    .find(|s| s.id == older)
                    .map(|s| s.subvolume.clone())
                    .unwrap_or_default();
                match crate::compare(app, &screen.snapshots, older, newer) {
                    Ok(comparison) => {
                        screen.message = None;
                        screen.show_comparison(&comparison, &subvolume);
                    }
                    Err(e) => screen.message = Some((Tone::Error, e.0)),
                }
            }
        }
    }
}
