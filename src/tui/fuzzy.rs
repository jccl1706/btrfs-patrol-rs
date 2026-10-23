// SPDX-License-Identifier: GPL-3.0-or-later
//! The search box: a selector when it looks like one, fuzzy text otherwise.
//!
//! `kind=pacman`, `keep=yes`, `3`, `10-20` are selectors, exactly as on the
//! command line. Anything else is matched fuzzily - its characters in order,
//! not necessarily together - against what a row shows: "msa upg" finds
//! "upgrade mesa". Words are matched separately, in any order.

use crate::snapshot::Snapshot;

/// Whether the query is a selector rather than text to search for.
pub fn is_selector(query: &str) -> bool {
    let query = query.trim();
    query.contains('=')
        || (!query.is_empty()
            && query.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '-' || c == ' '))
}

/// Whether `needle`'s characters appear in `haystack` in order (both lowercase).
pub fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// What a row can be found by, one field each.
fn fields(s: &Snapshot) -> Vec<String> {
    let mut fields = vec![
        format!("#{}", s.id),
        s.kind.clone(),
        s.subvolume.clone(),
        s.kernel.clone(),
        s.created.format("%Y-%m-%d %H:%M %b").to_string(),
        s.description.clone(),
    ];
    if s.keep {
        fields.push("kept".into());
    }
    fields.into_iter().map(|f| f.to_lowercase()).collect()
}

/// One word of the query against one field: as a substring anywhere
/// ("2026-09", "upg"), or fuzzily within a single word of it ("msa" in
/// "mesa"). Fuzzy across the whole row would match nearly anything - the
/// letters of "asem" are somewhere in most rows.
fn word_matches(word: &str, field: &str) -> bool {
    field.contains(word) || field.split(|c: char| !c.is_alphanumeric()).any(|w| subsequence(word, w))
}

pub fn matches(query: &str, snapshot: &Snapshot) -> bool {
    let fields = fields(snapshot);
    query.to_lowercase().split_whitespace().all(|word| fields.iter().any(|f| word_matches(word, f)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::snapshot;

    #[test]
    fn selector_or_text() {
        for q in ["kind=pacman", "3", "1,10,20-23", " keep = yes "] {
            assert!(is_selector(q), "{q}");
        }
        for q in ["mesa", "", "upgrade 3", "#3"] {
            assert!(!is_selector(q), "{q}");
        }
    }

    #[test]
    fn fuzzy() {
        let mut s = snapshot(12);
        s.description = "upgrade linux, Mesa, firefox".into();
        s.kind = "pacman-pre".into();
        for q in ["mesa", "MSA", "msa upg", "firefox linux", "pacman", "#12", "sep", "2026-09"] {
            assert!(matches(q, &s), "{q}");
        }
        for q in ["chromium", "asem", "kept", "mesa chromium"] {
            assert!(!matches(q, &s), "{q}");
        }
        s.keep = true;
        assert!(matches("kept", &s));
    }
}
