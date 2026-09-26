// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
// SPDX-FileCopyrightText: 2010-2026 Kiyoka Nishiyama <kiyoka@sumibi.org>
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Parts of this file are ported from Sekka (https://github.com/kiyoka/sekka),
// master @ 0f73ee9 (retrieved 2026-09-23): emacs/sekka-symspell.el.

//! SymSpell (edit distance <= 1 between kana strings) shared logic
//!
//! The single place both the build-time path (`dictionary::dict_format::write_dict`)
//! and the runtime path (`dictionary::immutable_dict::ImmutableFileDict::symspell_bucket`)
//! call into. If the two ever diverge, the whole index silently misses.
//!
//! Ported from `sekka-symspell--delete-variants` (lines 71-80) in
//! `emacs/sekka-symspell.el` of upstream Sekka.
//!
//! Per D-92 (03.1-CONTEXT.md) the on-disk index stores only the hashes returned
//! by `symspell_hash` in this module; the original delete-variant strings are
//! not kept.

/// Returns every single-character deletion variant of `word` (per `char`, so
/// multi-byte kana are handled as one character). Duplicates are not removed
/// (upstream does not remove them either). An empty string yields no variants.
pub fn delete_variants(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    (0..chars.len())
        .map(|i| {
            chars
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, c)| *c)
                .collect()
        })
        .collect()
}

// FNV-1a 64-bit (well-known fixed constants, public domain). The standard
// library's default hasher is not used because its own documentation does not
// guarantee stability across rustc versions (building the dictionary tool and
// the runtime library with different rustc versions would silently corrupt the
// index). Same approach as `crc32` in dict_format.rs: implement a small,
// well-known algorithm in-tree and pin it with known-answer tests.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hashes a delete-variant string (D-92: the on-disk index keeps only this
/// hash, never the original string).
pub fn symspell_hash(s: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// In-memory delete-variant index for the user dictionary (D-95)
///
/// Ported from `sekka-jisyo-build-symspell-now` (lines 330-347, incremental
/// addition of user dictionary keys into the master dictionary index) in
/// `emacs/sekka-jisyo.el` of upstream Sekka. It follows the same asymmetry as
/// `roman_index::MemoryRomanIndex` (D-73) - the master dictionary is immutable
/// while only the user dictionary needs incremental additions - and plays the
/// same role.
///
/// The on-disk side (`ImmutableFileDict`, D-91/92) keeps hashes only, whereas
/// this one keeps the strings themselves. D-92 is a decision about the on-disk
/// representation, and the `Dictionary::symspell_bucket` trait boundary (which
/// takes a string argument) confines hashing to an implementation detail inside
/// `ImmutableFileDict`, so `MemorySymSpellIndex` can simply use a `BTreeMap`
/// keyed by strings.
///
/// Unlike `MemoryRomanIndex`, this index has **no exclusion rules** (D-94).
/// Single-character keys and okuri-ari conventional keys (e.g. `かんj`) go into
/// the index as they are, mirroring the master dictionary's SymSpell index
/// (`ImmutableFileDict`), which also covers every key.
///
/// `BTreeMap` is chosen because its iteration order is deterministic, for the
/// same reason as in `MemoryRomanIndex`.
#[derive(Debug, Default)]
pub struct MemorySymSpellIndex {
    buckets: std::collections::BTreeMap<String, Vec<String>>,
}

impl MemorySymSpellIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the kana key `reading` to the index.
    ///
    /// Each variant of `delete_variants(reading)` becomes a bucket key holding
    /// `reading`. No exclusion rules are applied (D-94). If the same `reading`
    /// is already present, nothing happens (this matches the duplicate-key
    /// guard `(unless (and existing (member key existing)) ...)` in upstream
    /// `sekka-symspell--index-key`).
    pub fn insert(&mut self, reading: &str) {
        for variant in delete_variants(reading) {
            let readings = self.buckets.entry(variant).or_default();
            if !readings.iter().any(|r| r == reading) {
                readings.push(reading.to_string());
            }
        }
    }

    /// Returns a copy of the original kana keys whose delete variants include `variant`.
    pub fn bucket(&self, variant: &str) -> Vec<String> {
        self.buckets.get(variant).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_pinned_by_known_answers() {
        assert_eq!(symspell_hash(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(symspell_hash("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(symspell_hash("かんじ"), 0xe8d4_5528_4141_5b94);
        assert_eq!(symspell_hash("かん"), 0x79d7_a704_d9fb_073c);
    }

    #[test]
    fn kanji_yields_three_delete_variants() {
        let mut v = delete_variants("かんじ");
        v.sort();
        let mut expected = vec!["んじ".to_string(), "かじ".to_string(), "かん".to_string()];
        expected.sort();
        assert_eq!(v, expected);
    }

    #[test]
    fn empty_string_has_no_delete_variants() {
        assert_eq!(delete_variants(""), Vec::<String>::new());
    }

    #[test]
    fn okuri_ari_conventional_key_also_yields_variants() {
        // D-94: SymSpell has no exclusion rules; keys ending in an ASCII letter
        // are treated like any other key.
        let mut v = delete_variants("かんj");
        v.sort();
        let mut expected = vec!["んj".to_string(), "かj".to_string(), "かん".to_string()];
        expected.sort();
        assert_eq!(v, expected);
    }

    // === MemorySymSpellIndex (D-95) ===

    #[test]
    fn bucket_is_found_through_a_delete_variant_of_the_inserted_reading() {
        let mut index = MemorySymSpellIndex::new();
        index.insert("こんにちは");

        let bucket = index.bucket("こんちは");
        assert!(
            bucket.contains(&"こんにちは".to_string()),
            "「こんちは」(a delete variant of こんにちは) should find こんにちは: {:?}",
            bucket
        );
    }

    #[test]
    fn inserting_the_same_reading_twice_does_not_duplicate_the_bucket() {
        let mut index = MemorySymSpellIndex::new();
        index.insert("こんにちは");
        index.insert("こんにちは");

        let bucket = index.bucket("こんちは");
        assert_eq!(
            bucket.len(),
            1,
            "inserting twice should still leave a single entry in the bucket: {:?}",
            bucket
        );
    }

    #[test]
    fn no_exclusion_rules_so_single_char_and_okuri_ari_keys_are_indexed() {
        // D-94: the length floor and trailing-ASCII-letter exclusions that
        // MemoryRomanIndex has do not exist in MemorySymSpellIndex.
        let mut index = MemorySymSpellIndex::new();
        index.insert("あ"); // single-character key
        index.insert("かんj"); // okuri-ari conventional key

        // The only delete variant of 「あ」 is the empty string.
        let bucket_empty = index.bucket("");
        assert!(
            bucket_empty.contains(&"あ".to_string()),
            "single-character key 「あ」 should be indexed, not excluded: {:?}",
            bucket_empty
        );

        // One of the delete variants of 「かんj」 is 「かん」.
        let bucket_kan = index.bucket("かん");
        assert!(
            bucket_kan.contains(&"かんj".to_string()),
            "okuri-ari conventional key 「かんj」 should be indexed, not excluded: {:?}",
            bucket_kan
        );
    }

    #[test]
    fn unknown_variant_returns_empty() {
        let index = MemorySymSpellIndex::new();
        assert!(index.bucket("そんざいしない").is_empty());
    }
}
