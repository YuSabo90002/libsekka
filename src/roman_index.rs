// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
// SPDX-FileCopyrightText: 2010-2026 Kiyoka Nishiyama <kiyoka@sumibi.org>
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Parts of this file are ported from Sekka (https://github.com/kiyoka/sekka),
// master @ 0f73ee9 (retrieved 2026-09-23): emacs/sekka-jarowinkler.el, emacs/sekka-jisyo.el.

//! Romaji index lookup rules (pure functions) plus the in-memory index for the
//! user dictionary
//!
//! Holds the adaptive prefix length and the length-difference cutoff shared by
//! build-time index construction (`dictionary::dict_format::write_dict`) and
//! runtime bucket lookup
//! (`dictionary::immutable_dict::ImmutableFileDict::roman_bucket`).
//! It never builds or owns the buckets themselves (D-59: at runtime we only read
//! buckets).
//!
//! Ported from `sekka-jarowinkler--pick-prefix-length` and
//! `sekka-jarowinkler--length-ok-p` in `emacs/sekka-jarowinkler.el` of upstream
//! Sekka.
//!
//! `MemoryRomanIndex` (D-73) is the single exception that does own buckets: it
//! uses these rules to build an in-memory index for the user dictionary. Unlike
//! the master dictionary (immutable, indexed at build time), the user dictionary
//! needs incremental additions at runtime, so it gets its own memory structure.

use std::collections::BTreeMap;

use crate::dictionary::RomanKey;
use crate::kana_romaji::kana_to_hepburn;

/// Keys whose canonical romaji is shorter than this are not indexed (as upstream).
pub const MIN_INDEX_ROMAN_LEN: usize = 2;

/// Decides whether the kana key `kana_key` is an okuri-ari conventional key
/// (the okuri-ari convention in SKK-JISYO.L, e.g. `かんj` or `おこなu`) by
/// checking whether its last character is an ASCII letter.
///
/// Ported from `sekka-jisyo--okuri-key-p` (lines 511-515) in
/// `emacs/sekka-jisyo.el` of upstream Sekka.
///
/// This predicate is used asymmetrically in two places (the asymmetry is
/// intended, D-94):
/// (a) The romaji index (`MemoryRomanIndex::insert` and `is_roman_index_eligible`
/// in `dict_format::write_dict`) uses it to **exclude keys at the entrance**.
/// The romaji index requires canonicalization in romaji space, so indexing
/// okuri-ari conventional keys would produce bogus romaji prefix hits.
/// (b) SymSpell (tier 1 of `context::lookup_dictionary`) does **not** use it for
/// exclusion (D-94: every key is indexed). It measures edit distance in kana
/// space, where exclusion is unnecessary; instead the predicate only **sorts
/// such keys last** within the same distance (equivalent to upstream
/// `sekka-jisyo--okuri-key-score`).
pub fn ends_with_ascii_alpha(kana_key: &str) -> bool {
    kana_key
        .chars()
        .next_back()
        .map(|c| c.is_ascii_alphabetic())
        .unwrap_or(false)
}

/// Returns the adaptive prefix length for a query of length `query_len`.
///
/// Ported from `sekka-jarowinkler--pick-prefix-length`: >= 11 -> 4, >= 7 -> 3,
/// otherwise 2. Longer queries demand a stricter prefix match, which keeps
/// bucket sizes down.
pub fn pick_prefix_length(query_len: usize) -> usize {
    if query_len >= 11 {
        4
    } else if query_len >= 7 {
        3
    } else {
        2
    }
}

/// Returns the first `pick_prefix_length(query.chars().count())` characters of
/// the query.
///
/// Returns `None` when the character count is below `MIN_INDEX_ROMAN_LEN`, in
/// which case the index is not consulted at all.
pub fn query_prefix(query: &str) -> Option<String> {
    let char_count = query.chars().count();
    if char_count < MIN_INDEX_ROMAN_LEN {
        return None;
    }
    let plen = pick_prefix_length(char_count);
    Some(query.chars().take(plen).collect())
}

/// Length-difference cutoff: `100 * min(a_len, b_len) >= 70 * max(a_len, b_len)`.
///
/// Ported from `sekka-jarowinkler--length-ok-p`. The upstream docstring says
/// "within a 28% length difference (min/max >= roughly 0.72)", but the
/// implementation is the definition and we follow the code
/// (`(>= (* 100 mn) (* 70 mx))` = min/max >= 0.70).
pub fn length_ok(a_len: usize, b_len: usize) -> bool {
    let mn = a_len.min(b_len);
    let mx = a_len.max(b_len);
    mn > 0 && 100 * mn >= 70 * mx
}

/// In-memory romaji index for the user dictionary (D-73)
///
/// Key = canonical Hepburn romaji, value = the kana keys that romanize to it
/// (D-75: every collision between homophonous romaji is kept; we do not port
/// upstream's lossy one-to-one romaji->kana hash).
#[derive(Debug, Default)]
pub struct MemoryRomanIndex {
    buckets: BTreeMap<String, Vec<String>>,
}

impl MemoryRomanIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the kana key `reading` to the index.
    ///
    /// Computes `kana_to_hepburn(reading)` and does nothing when (a) the
    /// character count is below `MIN_INDEX_ROMAN_LEN`, or (b) the last character
    /// of `reading` is an ASCII letter (an okuri-ari conventional key in
    /// SKK-JISYO.L). These exclusion rules are kept identical to
    /// `dict_format::write_dict` (02.1-01) and `MockDictionary::roman_bucket`
    /// (Task 2). If the same `reading` is already present, nothing happens
    /// (equivalent to the first-wins guard in upstream
    /// `sekka-jisyo--register-jarowinkler-key`; D-75: when several kana keys map
    /// to the same romaji, all of them are kept).
    pub fn insert(&mut self, reading: &str) {
        let roman = kana_to_hepburn(reading);
        let roman_len_ok = roman.chars().count() >= MIN_INDEX_ROMAN_LEN;
        let last_char_is_ascii_alpha = reading
            .chars()
            .next_back()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false);
        if !roman_len_ok || last_char_is_ascii_alpha {
            return;
        }

        let readings = self.buckets.entry(roman).or_default();
        if !readings.iter().any(|r| r == reading) {
            readings.push(reading.to_string());
        }
    }

    /// Returns the `RomanKey`s matching the romaji prefix `roman_prefix`.
    ///
    /// Walks `BTreeMap::range(roman_prefix..)` only while `starts_with` holds
    /// (ascending key iteration makes the prefix-matching range contiguous).
    pub fn bucket(&self, roman_prefix: &str) -> Vec<RomanKey> {
        let mut result = Vec::new();
        for (roman, readings) in self.buckets.range(roman_prefix.to_string()..) {
            if !roman.starts_with(roman_prefix) {
                break;
            }
            for reading in readings {
                result.push(RomanKey {
                    reading: reading.clone(),
                    roman: roman.clone(),
                });
            }
        }
        result
    }

    /// Total number of (romaji, kana key) pairs in the index.
    pub fn len(&self) -> usize {
        self.buckets.values().map(Vec::len).sum()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ends_with_ascii_alpha_detects_okuri_ari_keys() {
        assert!(ends_with_ascii_alpha("かんj"));
        assert!(ends_with_ascii_alpha("おこなu"));
        assert!(!ends_with_ascii_alpha("かん"));
        assert!(!ends_with_ascii_alpha(""));
    }

    #[test]
    fn pick_prefix_length_boundaries() {
        assert_eq!(pick_prefix_length(2), 2);
        assert_eq!(pick_prefix_length(6), 2);
        assert_eq!(pick_prefix_length(7), 3);
        assert_eq!(pick_prefix_length(10), 3);
        assert_eq!(pick_prefix_length(11), 4);
    }

    #[test]
    fn query_prefix_cuts_the_head_at_the_adaptive_length() {
        assert_eq!(query_prefix("nihogno").as_deref(), Some("nih"));
        assert_eq!(query_prefix("ka").as_deref(), Some("ka"));
    }

    #[test]
    fn query_prefix_returns_none_below_the_minimum_length() {
        assert_eq!(query_prefix("k"), None);
        assert_eq!(query_prefix(""), None);
    }

    #[test]
    fn length_ok_passes_at_exactly_0_70() {
        assert!(length_ok(7, 10));
        assert!(length_ok(10, 7));
    }

    #[test]
    fn length_ok_fails_below_0_70() {
        assert!(!length_ok(6, 10));
    }

    // === MemoryRomanIndex (D-73, RED target) ===

    /// RED target: the stub always returns empty, so the inserted key is not
    /// found and this fails.
    #[test]
    fn inserted_reading_is_found_through_bucket() {
        let mut index = MemoryRomanIndex::new();
        index.insert("にほんご");

        let results = index.bucket("nih");
        assert!(
            results.contains(&RomanKey {
                reading: "にほんご".to_string(),
                roman: "nihongo".to_string(),
            }),
            "the inserted にほんご should appear in the bucket: {:?}",
            results
        );
    }

    #[test]
    fn two_readings_with_the_same_romaji_are_both_returned() {
        // D-75: かんい and かに both canonicalize to the romaji kani (see kana_romaji.rs).
        let mut index = MemoryRomanIndex::new();
        index.insert("かんい");
        index.insert("かに");

        let results = index.bucket("ka");
        let readings: Vec<&str> = results.iter().map(|rk| rk.reading.as_str()).collect();
        assert!(readings.contains(&"かんい"), "{:?}", readings);
        assert!(readings.contains(&"かに"), "{:?}", readings);
    }

    #[test]
    fn inserting_the_same_reading_twice_does_not_duplicate_it() {
        let mut index = MemoryRomanIndex::new();
        index.insert("にほんご");
        index.insert("にほんご");

        assert_eq!(
            index.len(),
            1,
            "inserting twice should still leave a total of 1 entry"
        );
    }

    #[test]
    fn short_readings_and_okuri_ari_conventional_keys_are_not_indexed() {
        let mut index = MemoryRomanIndex::new();
        index.insert("あ"); // canonical romaji "a" is 1 char (below MIN_INDEX_ROMAN_LEN)
        index.insert("かんj"); // ends with an ASCII letter (okuri-ari conventional key)

        assert!(
            index.is_empty(),
            "neither a short reading nor an okuri-ari conventional key should be indexed: len={}",
            index.len()
        );
    }
}
