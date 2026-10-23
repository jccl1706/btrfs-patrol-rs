// SPDX-License-Identifier: GPL-3.0-or-later
//! Snapshot selectors.
//!
//! A selector is either a list of IDs and ID ranges, such as `3` or
//! `1,10,20-23`, or a single field match:
//!
//! ```text
//! date=2026-09        date (YYYY-MM-DD) contains the text
//! time=16:            time (HH:MM:SS) contains the text
//! kernel=7.2          kernel version contains the text
//! kind=pacman-pre     kind contains the text
//! description=mesa    description contains the text
//! subvolume=home      snapshots of that subvolume (the whole name)
//! keep=yes            kept snapshots (keep=no for the others)
//! ```
//!
//! Text matches ignore case. Field matches can't be combined with commas, so
//! the text may contain commas.

use std::collections::{BTreeMap, BTreeSet};

use crate::bail;
use crate::error::Result;
use crate::snapshot::Snapshot;

pub const HELP: &str = "\
selectors:
  3, 1,10,20-23       snapshot IDs and ID ranges
  date=2026-09        date (YYYY-MM-DD) contains the text
  time=16:            time (HH:MM:SS) contains the text
  kernel=7.2          kernel version contains the text
  kind=pacman-pre     kind contains the text
  description=mesa    description contains the text
  subvolume=home      snapshots of that subvolume
  keep=yes, keep=no   kept snapshots, or the others
text matches ignore case";

const FIELDS: [&str; 5] = ["date", "time", "kernel", "kind", "description"];

fn field(snapshot: &Snapshot, name: &str) -> String {
    match name {
        "date" => snapshot.created.format("%Y-%m-%d").to_string(),
        "time" => snapshot.created.format("%H:%M:%S").to_string(),
        "kernel" => snapshot.kernel.clone(),
        "kind" => snapshot.kind.clone(),
        _ => snapshot.description.clone(),
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The snapshots matching `selector`, ordered by ID.
///
/// An explicit ID that doesn't exist is an error; ranges and field matches
/// select whatever exists.
pub fn select<'a>(selector: &str, snapshots: &'a [Snapshot]) -> Result<Vec<&'a Snapshot>> {
    if selector.contains('=') {
        return select_by_field(selector, snapshots);
    }
    let by_id: BTreeMap<u64, &Snapshot> = snapshots.iter().map(|s| (s.id, s)).collect();
    let mut chosen = BTreeSet::new();
    for term in selector.split(',').map(str::trim) {
        if is_number(term) {
            let Ok(id) = term.parse() else { bail!("no snapshot with ID {term}") };
            if !by_id.contains_key(&id) {
                bail!("no snapshot with ID {id}");
            }
            chosen.insert(id);
        } else if let Some((low, high)) = term.split_once('-').filter(|(l, h)| is_number(l) && is_number(h)) {
            let (Ok(low), Ok(high)) = (low.parse::<u64>(), high.parse::<u64>()) else {
                bail!("invalid selector {selector:?}")
            };
            let (low, high) = (low.min(high), low.max(high));
            chosen.extend(by_id.range(low..=high).map(|(id, _)| *id));
        } else {
            bail!("invalid selector {selector:?}");
        }
    }
    Ok(chosen.into_iter().map(|id| by_id[&id]).collect())
}

fn select_by_field<'a>(selector: &str, snapshots: &'a [Snapshot]) -> Result<Vec<&'a Snapshot>> {
    let (name, value) = selector.split_once('=').expect("has '='");
    // Both trimmed: "kind = timer" is the natural way to type it.
    let (name, value) = (name.trim(), value.trim());
    let mut matches: Vec<&Snapshot> = match name {
        "keep" => {
            let keep = match value {
                "yes" => true,
                "no" => false,
                _ => bail!("keep= must be followed by 'yes' or 'no'"),
            };
            snapshots.iter().filter(|s| s.keep == keep).collect()
        }
        "subvolume" => {
            // The whole name: "subvolume=o" matching both root and home would only surprise.
            if value.is_empty() {
                bail!("subvolume= must be followed by a subvolume's name");
            }
            let value = value.to_lowercase();
            snapshots.iter().filter(|s| s.subvolume.to_lowercase() == value).collect()
        }
        _ if FIELDS.contains(&name) => {
            if value.is_empty() {
                bail!("{name}= must be followed by some text");
            }
            let needle = value.to_lowercase();
            snapshots.iter().filter(|s| field(s, name).to_lowercase().contains(&needle)).collect()
        }
        _ => {
            bail!("unknown selector field {name:?} (expected one of: {}, subvolume, keep)", FIELDS.join(", "))
        }
    };
    matches.sort_by_key(|s| s.id);
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::snapshot;

    fn ids(selector: &str, snapshots: &[Snapshot]) -> Vec<u64> {
        select(selector, snapshots).unwrap().iter().map(|s| s.id).collect()
    }

    fn sample() -> Vec<Snapshot> {
        let mut all: Vec<Snapshot> = [1, 2, 3, 10, 20, 21, 22, 23].into_iter().map(snapshot).collect();
        all[0].kind = "pacman-pre".into();
        all[0].description = "upgrade Mesa, linux".into();
        all[1].keep = true;
        all[2].subvolume = "home".into();
        all
    }

    #[test]
    fn ids_and_ranges() {
        let all = sample();
        assert_eq!(ids("3", &all), [3]);
        assert_eq!(ids("1,10,20-22", &all), [1, 10, 20, 21, 22]);
        assert_eq!(ids("23-20", &all), [20, 21, 22, 23]);
        assert_eq!(ids(" 2 , 2 ", &all), [2]);
        assert_eq!(ids("4-9", &all), [] as [u64; 0]);
    }

    #[test]
    fn missing_or_malformed() {
        let all = sample();
        assert_eq!(select("4", &all).unwrap_err().0, "no snapshot with ID 4");
        assert!(select("a", &all).unwrap_err().0.contains("invalid selector"));
        assert!(select("1-", &all).is_err());
        assert!(select("", &all).is_err());
    }

    #[test]
    fn fields() {
        let all = sample();
        assert_eq!(ids("kind=PACMAN", &all), [1]);
        assert_eq!(ids("description = mesa, linux", &all), [1]);
        assert_eq!(ids("keep=yes", &all), [2]);
        assert_eq!(ids("keep=no", &all).len(), 7);
        assert_eq!(ids("subvolume=Home", &all), [3]);
        assert_eq!(ids("subvolume=hom", &all), [] as [u64; 0]);
        assert_eq!(ids("date=2026-09-21", &all).len(), 8);
        assert_eq!(ids("time=22:25", &all).len(), 8);
        assert_eq!(ids("kernel=arch1", &all).len(), 8);
    }

    #[test]
    fn field_errors() {
        let all = sample();
        assert!(select("keep=maybe", &all).unwrap_err().0.contains("'yes' or 'no'"));
        assert!(select("kind=", &all).unwrap_err().0.contains("some text"));
        assert!(select("subvolume=", &all).is_err());
        assert!(select("colour=red", &all).unwrap_err().0.contains("unknown selector field"));
    }
}
