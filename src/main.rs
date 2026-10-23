// SPDX-License-Identifier: GPL-3.0-or-later
//! btrfs-patrol: a btrfs snapshot manager and rollback tool for Arch Linux.

mod btrfs;
mod check;
mod config;
mod diff;
mod error;
mod mounts;
mod selector;
mod snapshot;
mod table;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::config::{ColorChoice, Config};
use crate::error::Result;
use crate::snapshot::{ROOT, Snapshot, Store};
use crate::table::Style;

#[derive(Parser)]
#[command(name = "btrfs-patrol", version, about = "Btrfs snapshot manager and rollback tool.")]
struct Cli {
    /// Configuration file [default: /etc/btrfs-patrol/config.toml]
    #[arg(short, long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Override output.color from the configuration
    #[arg(long, global = true, value_enum)]
    color: Option<ColorChoice>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List snapshots
    #[command(after_help = selector::HELP)]
    List {
        /// Only list matching snapshots
        selector: Option<String>,
        /// Wrap long descriptions instead of truncating them
        #[arg(short, long)]
        verbose: bool,
    },
    /// Show what changed between two snapshots (needs root)
    Diff {
        #[arg(value_name = "OLD")]
        old: u64,
        #[arg(value_name = "NEW")]
        new: u64,
    },
    /// Browse snapshots in a full-screen interface
    Tui,
    /// Check the configuration, mounts and snapshots
    Check,
    /// Print the example configuration, with the defaults
    ExampleConfig,
}

pub struct App {
    pub config: Config,
    pub store: Store,
    pub color: ColorChoice,
}

impl App {
    pub fn show_subvolume(&self, snapshots: &[Snapshot]) -> bool {
        // Only a system that snapshots more than root needs to be told which one.
        !self.config.subvolumes.is_empty() || snapshots.iter().any(|s| s.subvolume != ROOT)
    }

    fn info(&self, message: &str) {
        println!("{} {message}", Style::for_stdout(self.color).blue("::"));
    }
}

fn warn(color: ColorChoice, message: &str) {
    eprintln!("{} {message}", Style::for_stderr(color).yellow("warning:"));
}

fn error(color: ColorChoice, message: &str) {
    eprintln!("{} {message}", Style::for_stderr(color).red("error:"));
}

fn cmd_list(app: &App, selector: Option<&str>, verbose: bool) -> Result<u8> {
    let all = app.store.load_all()?;
    let shown: Vec<&Snapshot> = match selector {
        Some(selector) => selector::select(selector, &all)?,
        None => all.iter().collect(),
    };
    // Off a terminal - a pipe, a file - there is no width to fit, and a
    // description cut short is lost to grep, so print it whole.
    let width = if std::io::stdout().is_terminal() {
        ratatui::crossterm::terminal::size().map_or(80, |(w, _)| w as usize)
    } else {
        usize::MAX / 2
    };
    let style = Style::for_stdout(app.color);
    println!("{}", table::format_table(&shown, style, width, verbose, app.show_subvolume(&all)));
    let kept = shown.iter().filter(|s| s.keep).count();
    println!("{} snapshot(s), {kept} kept", shown.len());
    Ok(0)
}

/// The one snapshot an ID names, for commands that take exactly one.
fn one(all: &[Snapshot], id: u64) -> Result<&Snapshot> {
    all.iter().find(|s| s.id == id).ok_or_else(|| err!("no snapshot with ID {id}"))
}

pub fn compare(app: &App, all: &[Snapshot], old: u64, new: u64) -> Result<diff::Comparison> {
    let (older, newer) = (one(all, old)?, one(all, new)?);
    if older.id == newer.id {
        bail!("that is the same snapshot twice");
    }
    if older.subvolume != newer.subvolume {
        bail!(
            "snapshot {} is of {:?} and {} is of {:?}; only snapshots of the same subvolume can be compared",
            older.id,
            older.subvolume,
            newer.id,
            newer.subvolume
        );
    }
    require_root("comparing snapshots")?;
    diff::compare(&app.store.subvolume(older.id), &app.store.subvolume(newer.id), older.id, newer.id)
}

fn cmd_diff(app: &App, old: u64, new: u64) -> Result<u8> {
    let all = app.store.load_all()?;
    let comparison = compare(app, &all, old, new)?;
    let subvolume = &one(&all, old)?.subvolume;
    app.info(&format!("snapshot {old} -> {new} ({subvolume}): {}", comparison.summary()));
    for line in comparison.lines() {
        println!("{}", table::printable(&line));
    }
    Ok(0)
}

fn cmd_check(app: &App) -> Result<u8> {
    let mounts = mounts::read_mounts()?;
    let fstab = std::fs::read_to_string("/etc/fstab").ok();
    let report = check::check(&app.config, &app.store, &mounts, fstab.as_deref(), &btrfs::is_read_only);
    for warning in &report.warnings {
        warn(app.color, warning);
    }
    for problem in &report.problems {
        error(app.color, problem);
    }
    if !report.problems.is_empty() {
        return Ok(1);
    }
    app.info("configuration and snapshots look good");
    Ok(0)
}

pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

pub fn require_root(what: &str) -> Result<()> {
    if !is_root() {
        bail!("{what} needs root; run it with sudo");
    }
    Ok(())
}

fn run(cli: Cli) -> Result<u8> {
    if let Command::ExampleConfig = cli.command {
        print!("{}", config::EXAMPLE);
        return Ok(0);
    }
    let config = config::load(cli.config.as_deref())?;
    let color = cli.color.unwrap_or(config.color);
    let app = App { store: Store::new(&config.snapshots_dir), config, color };
    match cli.command {
        Command::List { selector, verbose } => cmd_list(&app, selector.as_deref(), verbose),
        Command::Diff { old, new } => cmd_diff(&app, old, new),
        Command::Check => cmd_check(&app),
        Command::Tui => tui::run(&app),
        Command::ExampleConfig => unreachable!("handled above"),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let color = cli.color.unwrap_or(ColorChoice::Auto);
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            error(color, &e.0);
            ExitCode::from(1)
        }
    }
}
