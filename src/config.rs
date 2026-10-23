// SPDX-License-Identifier: GPL-3.0-or-later
//! Loading and validating the configuration file.
//!
//! The format is btrfs-patrol's, with Arch's layout as the defaults (`@`,
//! `@snapshots`) and `[pacman]` in place of `[dnf]`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::Result;
use crate::snapshot::ROOT;
use crate::{bail, err};

pub const DEFAULT_PATH: &str = "/etc/btrfs-patrol/config.toml";
pub const ENV_VAR: &str = "BTRFS_PATROL_CONFIG";
pub const EXAMPLE: &str = include_str!("../config.toml.example");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// A subvolume btrfs-patrol takes snapshots of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSubvolume {
    pub name: String,
    /// Its mount point, or its path inside the subvolume that contains it.
    pub path: PathBuf,
    pub max_snapshots: usize,
    /// Whether the daily timer snapshots it.
    pub timer: bool,
    /// Whether pacman transactions snapshot it.
    pub pacman: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Block device of the btrfs filesystem, or None for the one mounted at /.
    pub device: Option<String>,
    pub root_subvolume: String,
    pub snapshots_subvolume: String,
    pub snapshots_dir: PathBuf,
    pub max_snapshots: usize,
    /// How many snapshots get a boot menu entry; 0 writes none.
    pub boot_entries: usize,
    pub pacman_pre_snapshot: bool,
    pub pacman_post_snapshot: bool,
    pub color: ColorChoice,
    /// Subvolumes to snapshot besides root, from the [subvolumes.<name>] tables.
    pub subvolumes: Vec<ManagedSubvolume>,
}

impl Config {
    pub fn root(&self) -> ManagedSubvolume {
        ManagedSubvolume {
            name: ROOT.into(),
            path: "/".into(),
            max_snapshots: self.max_snapshots,
            timer: true,
            pacman: true,
        }
    }

    /// Every subvolume snapshots are taken of, root first.
    pub fn managed(&self) -> Vec<ManagedSubvolume> {
        std::iter::once(self.root()).chain(self.subvolumes.iter().cloned()).collect()
    }
}

impl Default for Config {
    fn default() -> Self {
        parse("").expect("the defaults are valid")
    }
}

// The file as written. Every section and option has a default, and an
// unknown one is an error: a misspelt option silently ignored is a setting
// that never applies.

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct File {
    filesystem: Filesystem,
    retention: Retention,
    boot: Boot,
    pacman: Pacman,
    output: Output,
    subvolumes: BTreeMap<String, SubvolumeTable>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Filesystem {
    device: String,
    root_subvolume: String,
    snapshots_subvolume: String,
    snapshots_dir: String,
}

impl Default for Filesystem {
    fn default() -> Self {
        Self {
            device: "auto".into(),
            root_subvolume: "@".into(),
            snapshots_subvolume: "@snapshots".into(),
            snapshots_dir: "/.snapshots".into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Retention {
    max_snapshots: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { max_snapshots: 50 }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Boot {
    entries: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Pacman {
    pre_snapshot: bool,
    post_snapshot: bool,
}

impl Default for Pacman {
    fn default() -> Self {
        Self { pre_snapshot: true, post_snapshot: false }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Output {
    color: ColorChoice,
}

impl Default for Output {
    fn default() -> Self {
        Self { color: ColorChoice::Auto }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubvolumeTable {
    path: String,
    max_snapshots: Option<i64>,
    #[serde(default = "yes")]
    timer: bool,
    #[serde(default)]
    pacman: bool,
}

fn yes() -> bool {
    true
}

/// The configuration file to use, and whether it was asked for: the path
/// given, else $BTRFS_PATROL_CONFIG, else the default.
pub fn resolve_path(path: Option<&Path>) -> (PathBuf, bool) {
    if let Some(path) = path {
        return (path.to_path_buf(), true);
    }
    match std::env::var_os(ENV_VAR) {
        Some(value) if !value.is_empty() => (value.into(), true),
        _ => (DEFAULT_PATH.into(), false),
    }
}

/// Load the configuration.
///
/// A missing DEFAULT file means the defaults, which are Arch's layout: a
/// standard install works with no configuration at all. A file someone named,
/// with -c or the environment, has to exist - a typo there must not quietly
/// manage a different layout.
pub fn load(path: Option<&Path>) -> Result<Config> {
    let (path, explicit) = resolve_path(path);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map_err(|e| err!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(Config::default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("configuration file not found: {}", path.display())
        }
        Err(e) => bail!("{}: {e}", path.display()),
    }
}

pub fn parse(text: &str) -> Result<Config> {
    let file: File = toml::from_str(text).map_err(|e| err!("{}", e.message()))?;
    let max_snapshots = count(file.retention.max_snapshots, "retention.max_snapshots", 1)?;
    let mut subvolumes = Vec::new();
    for (name, table) in file.subvolumes {
        let where_ = format!("subvolumes.{name}");
        subvolumes.push(ManagedSubvolume {
            max_snapshots: match table.max_snapshots {
                Some(n) => count(n, &format!("{where_}.max_snapshots"), 1)?,
                None => max_snapshots,
            },
            name,
            path: table.path.into(),
            timer: table.timer,
            pacman: table.pacman,
        });
    }
    let config = Config {
        device: (file.filesystem.device != "auto").then_some(file.filesystem.device),
        root_subvolume: file.filesystem.root_subvolume,
        snapshots_subvolume: file.filesystem.snapshots_subvolume,
        snapshots_dir: file.filesystem.snapshots_dir.into(),
        max_snapshots,
        boot_entries: count(file.boot.entries, "boot.entries", 0)?,
        pacman_pre_snapshot: file.pacman.pre_snapshot,
        pacman_post_snapshot: file.pacman.post_snapshot,
        color: file.output.color,
        subvolumes,
    };
    validate(&config)?;
    Ok(config)
}

fn count(value: i64, option: &str, minimum: i64) -> Result<usize> {
    if value < minimum {
        bail!("{option} must be at least {minimum}");
    }
    Ok(value as usize)
}

fn has_parent_dir(path: &Path) -> bool {
    path.components().any(|c| c == Component::ParentDir)
}

fn validate(config: &Config) -> Result<()> {
    for (option, name) in
        [("root_subvolume", &config.root_subvolume), ("snapshots_subvolume", &config.snapshots_subvolume)]
    {
        if name.is_empty() || name.starts_with('/') || has_parent_dir(Path::new(name)) {
            bail!("filesystem.{option} must be a path relative to the top-level subvolume, got {name:?}");
        }
    }
    if Path::new(&config.snapshots_subvolume).starts_with(&config.root_subvolume) {
        // A rollback moves the root subvolume away, which would take the snapshots with it.
        bail!("filesystem.snapshots_subvolume must not be inside the root subvolume");
    }
    if !config.snapshots_dir.is_absolute() {
        bail!("filesystem.snapshots_dir must be an absolute path");
    }
    if config.snapshots_dir.to_string_lossy().chars().any(char::is_whitespace) {
        // /etc/fstab separates its fields with whitespace, and setup writes the
        // mount point there unescaped.
        bail!("filesystem.snapshots_dir must not contain whitespace");
    }

    let mut paths: BTreeMap<&Path, &str> = BTreeMap::new();
    let mut names: BTreeMap<String, &str> = BTreeMap::new();
    for subvolume in &config.subvolumes {
        let where_ = format!("subvolumes.{}", subvolume.name);
        // Case-insensitively, because 'subvolume=' matching is: [subvolumes.ROOT]
        // would otherwise be selected along with the real root.
        let folded = subvolume.name.to_lowercase();
        if folded == ROOT {
            bail!("[{where_}]: the name {ROOT:?} is the root subvolume's");
        }
        if let Some(other) = names.insert(folded, &subvolume.name) {
            bail!("[{where_}]: {other:?} differs only in case, and a selector could not tell them apart");
        }
        let mut chars = subvolume.name.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
            && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
        if !valid {
            bail!(
                "[{where_}]: a subvolume's name is letters, digits, '_', '.' and '-', starting with a letter or digit"
            );
        }
        let path = subvolume.path.as_path();
        if !path.is_absolute() || has_parent_dir(path) {
            bail!(
                "{where_}.path must be an absolute path without '..', got {:?}",
                path.display().to_string()
            );
        }
        if path == Path::new("/") {
            bail!("{where_}.path is /, the root subvolume, which is always snapshotted");
        }
        if path.starts_with(&config.snapshots_dir) {
            bail!("{where_}.path is inside the snapshots directory {}", config.snapshots_dir.display());
        }
        if let Some(other) = paths.insert(path, &subvolume.name) {
            bail!("{where_}.path {} is already [subvolumes.{other}]", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_archs_layout() {
        let config = Config::default();
        assert_eq!(config.root_subvolume, "@");
        assert_eq!(config.snapshots_subvolume, "@snapshots");
        assert_eq!(config.snapshots_dir, PathBuf::from("/.snapshots"));
        assert_eq!(config.max_snapshots, 50);
        assert_eq!(config.device, None);
        assert!(config.pacman_pre_snapshot && !config.pacman_post_snapshot);
        assert_eq!(config.managed().len(), 1);
    }

    #[test]
    fn the_shipped_example_is_the_defaults() {
        assert_eq!(parse(EXAMPLE).unwrap(), Config::default());
    }

    #[test]
    fn subvolumes() {
        let config = parse(
            "[retention]\nmax_snapshots = 9\n[subvolumes.home]\npath = \"/home\"\n\
             [subvolumes.log]\npath = \"/var/log\"\nmax_snapshots = 3\ntimer = false\npacman = true\n",
        )
        .unwrap();
        let home = &config.subvolumes[0];
        assert_eq!(
            (home.name.as_str(), home.max_snapshots, home.timer, home.pacman),
            ("home", 9, true, false)
        );
        let log = &config.subvolumes[1];
        assert_eq!((log.max_snapshots, log.timer, log.pacman), (3, false, true));
    }

    #[test]
    fn unknown_options_and_sections_are_errors() {
        assert!(parse("[retention]\nmax_snapshot = 3\n").is_err());
        assert!(parse("[dnf]\npre_snapshot = true\n").is_err());
        assert!(parse("[subvolumes.home]\npath = \"/home\"\ndnf = true\n").is_err());
    }

    #[test]
    fn invalid_values() {
        for (text, message) in [
            ("[retention]\nmax_snapshots = 0", "at least 1"),
            ("[boot]\nentries = -1", "at least 0"),
            ("[filesystem]\nroot_subvolume = \"/@\"", "relative"),
            ("[filesystem]\nsnapshots_subvolume = \"../x\"", "relative"),
            ("[filesystem]\nsnapshots_subvolume = \"@/snapshots\"", "inside the root"),
            ("[filesystem]\nsnapshots_dir = \"snapshots\"", "absolute"),
            ("[filesystem]\nsnapshots_dir = \"/my snapshots\"", "whitespace"),
            ("[output]\ncolor = \"sometimes\"", "unknown variant"),
            ("[subvolumes.Root]\npath = \"/srv\"", "root subvolume's"),
            ("[subvolumes.a]\npath = \"/a\"\n[subvolumes.A]\npath = \"/b\"", "only in case"),
            ("[subvolumes.-x]\npath = \"/a\"", "letters, digits"),
            ("[subvolumes.a]\npath = \"a\"", "absolute"),
            ("[subvolumes.a]\npath = \"/\"", "always snapshotted"),
            ("[subvolumes.a]\npath = \"/.snapshots/x\"", "inside the snapshots"),
            ("[subvolumes.a]\npath = \"/x\"\n[subvolumes.b]\npath = \"/x\"", "already"),
            ("[subvolumes.a]\nmax_snapshots = 2", "missing field `path`"),
        ] {
            let error = parse(text).unwrap_err().0;
            assert!(error.contains(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn missing_default_file_means_defaults_but_a_named_one_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(load(Some(&missing)).unwrap_err().0.contains("not found"));
    }
}
