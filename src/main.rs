// SPDX-License-Identifier: GPL-3.0-or-later
//! btrfs-patrol: a btrfs snapshot manager and rollback tool for Arch Linux.

mod btrfs;
mod check;
mod config;
mod diff;
mod entries;
mod error;
mod hook;
mod kernel;
mod manage;
mod mounts;
mod rollback;
mod selector;
mod setup;
mod snapshot;
mod status;
mod system;
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
    /// Take snapshots of the managed subvolumes
    Snapshot {
        #[arg(short, long, default_value = "")]
        description: String,
        /// Never prune these snapshots automatically
        #[arg(short, long)]
        keep: bool,
        /// Only this subvolume, root or a [subvolumes.NAME] table; repeat for more
        #[arg(short, long = "subvolume", value_name = "NAME")]
        subvolumes: Vec<String>,
        #[arg(long, default_value = "manual", hide = true, value_parser = ["manual", "timer"])]
        kind: String,
    },
    /// Change a snapshot's description
    Describe { id: u64, description: String },
    /// Protect snapshots from pruning
    #[command(after_help = selector::HELP)]
    Keep { selector: String },
    /// Let snapshots be pruned again
    #[command(after_help = selector::HELP)]
    Unkeep { selector: String },
    /// Delete snapshots
    #[command(after_help = selector::HELP)]
    Delete {
        selector: String,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete each subvolume's oldest snapshots beyond its max_snapshots
    Prune,
    /// Roll a subvolume back to one of its snapshots, then reboot to use it
    Rollback {
        id: u64,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
        /// Run every check and show the plan, without changing anything
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Set the system up: the snapshots subvolume, its mount, the timer
    Setup {
        /// Show what would be done, change nothing
        #[arg(short = 'n', long)]
        dry_run: bool,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Write the status file desktop widgets read (/run/btrfs-patrol/status.json)
    #[command(hide = true)]
    PublishStatus,
    /// Run by the pacman hooks around each transaction
    #[command(hide = true)]
    PacmanHook {
        #[arg(value_parser = ["pre", "post"])]
        when: String,
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

    /// The snapshots a selector matches; matching none is an error.
    pub fn matching(&self, selector: &str) -> Result<Vec<Snapshot>> {
        let all = self.store.load_all()?;
        let found: Vec<Snapshot> = selector::select(selector, &all)?.into_iter().cloned().collect();
        if found.is_empty() {
            bail!("no snapshots match {selector:?}");
        }
        Ok(found)
    }

    pub fn print_table(&self, snapshots: &[Snapshot]) {
        let rows: Vec<&Snapshot> = snapshots.iter().collect();
        let width = if std::io::stdout().is_terminal() {
            ratatui::crossterm::terminal::size().map_or(80, |(w, _)| w as usize)
        } else {
            usize::MAX / 2
        };
        let style = Style::for_stdout(self.color);
        println!("{}", table::format_table(&rows, style, width, false, self.show_subvolume(snapshots)));
    }

    pub fn info(&self, message: &str) {
        println!("{} {message}", Style::for_stdout(self.color).blue("::"));
    }
}

pub fn warn(color: ColorChoice, message: &str) {
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

/// Set the system up, then check it. Unlike the other commands, it needs no
/// configuration - writing one is part of what it may do.
fn cmd_setup(
    path: Option<&std::path::Path>,
    color: Option<ColorChoice>,
    dry_run: bool,
    yes: bool,
) -> Result<u8> {
    require_root("setup")?;
    system::require_confirmation_possible(yes || dry_run)?;
    let plan = setup::prepare(path)?;
    let color = color.unwrap_or(plan.config.color);
    let app = App { store: Store::new(&plan.config.snapshots_dir), config: plan.config.clone(), color };
    let actions = plan.actions();
    if actions.is_empty() {
        app.info("already set up; nothing to do");
    } else {
        println!("setup will:");
        for action in &actions {
            println!("  - {action}");
        }
        if dry_run {
            app.info("dry run: nothing changed");
            return Ok(0);
        }
        if !system::confirm("Continue?", yes)? {
            app.info("nothing changed");
            return Ok(1);
        }
        plan.execute()?;
        app.info("setup done");
    }
    drop(plan);
    cmd_check(&app)
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
    if let Command::Setup { dry_run, yes } = cli.command {
        return cmd_setup(cli.config.as_deref(), cli.color, dry_run, yes);
    }
    let config = config::load(cli.config.as_deref())?;
    let color = cli.color.unwrap_or(config.color);
    let app = App { store: Store::new(&config.snapshots_dir), config, color };
    let changes = matches!(
        cli.command,
        Command::Snapshot { .. }
            | Command::Describe { .. }
            | Command::Keep { .. }
            | Command::Unkeep { .. }
            | Command::Delete { .. }
            | Command::Prune
            | Command::PacmanHook { .. }
            | Command::Rollback { .. }
            | Command::PublishStatus
    );
    if changes {
        require_root("changing snapshots")?;
        system::defer_interrupts();
    }
    match cli.command {
        Command::Snapshot { description, keep, subvolumes, kind } => {
            manage::cmd_snapshot(&app, &description, keep, &subvolumes, &kind)
        }
        Command::Describe { id, description } => manage::cmd_describe(&app, id, &description),
        Command::Keep { selector } => manage::cmd_keep(&app, &selector, true),
        Command::Unkeep { selector } => manage::cmd_keep(&app, &selector, false),
        Command::Delete { selector, yes } => manage::cmd_delete(&app, &selector, yes),
        Command::Prune => manage::cmd_prune(&app),
        Command::PacmanHook { when } => Ok(hook::run(&app, &when)),
        Command::PublishStatus => status::cmd_publish(&app),
        Command::Rollback { id, yes, dry_run } => rollback::cmd_rollback(&app, id, yes, dry_run),
        Command::List { selector, verbose } => cmd_list(&app, selector.as_deref(), verbose),
        Command::Diff { old, new } => cmd_diff(&app, old, new),
        Command::Check => cmd_check(&app),
        Command::Tui => tui::run(&app),
        Command::ExampleConfig | Command::Setup { .. } => unreachable!("handled above"),
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
