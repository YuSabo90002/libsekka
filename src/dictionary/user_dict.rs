// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! User dictionary (read-write) implementation
//!
//! A user dictionary backed by sled. It records which candidate the user selected
//! most recently (MRU learning, D-190): every selection stamps the word with the
//! next number of a dictionary-wide sequence, and the highest number comes first.
//!
//! Dictionaries written by v1.3 and earlier stored a selection count instead. `open`
//! migrates such values exactly once, by the shape of the value alone (D-195): the
//! old count only decides the order in which the new numbers are handed out, so each
//! reading keeps the order it had.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use serde::Deserialize;

use crate::dictionary::{DictEntry, DictError, Dictionary, DictionaryMode, RomanKey};
use crate::roman_index::MemoryRomanIndex;
use crate::symspell::MemorySymSpellIndex;

/// A value element as it may exist on disk
///
/// Reads both the v1.3 element (a count only) and the v1.4 one (a sequence number
/// only) during `open`; it is never written. An element is legacy when it has a
/// count and no sequence number; an element with neither is left as it is (v1.3
/// could not write one). The old count is received here and nowhere else - it is not
/// a field of `DictEntry`, and no serde alias reads it as a sequence number.
#[derive(Debug, Clone, Deserialize)]
struct StoredEntry {
    word: String,
    #[serde(default)]
    annotation: Option<String>,
    #[serde(default)]
    frequency: Option<u32>,
    #[serde(default)]
    last_selected: Option<u64>,
}

impl StoredEntry {
    /// True for a v1.3 element: it has a count and no sequence number.
    fn is_legacy(&self) -> bool {
        self.frequency.is_some() && self.last_selected.is_none()
    }
}

/// The rows `plan_migration` rewrites, and the highest sequence number in use afterwards
type MigrationPlan = (Vec<(String, Vec<DictEntry>)>, u64);

/// Plans the one-time migration of v1.3 elements (D-195)
///
/// `rows` are the readings that hold at least one legacy element, with all their
/// elements in stored order; `base` is the highest sequence number already in use.
/// Returns `None` when no element is legacy (nothing is written then). Otherwise
/// returns the rows to rewrite - only those containing a legacy element, with the
/// element order unchanged - and the highest number in use afterwards.
///
/// Every legacy element of every reading is numbered together, ascending by old
/// count (then by the reading's bytes, then by descending stored position), starting
/// at `base + 1`. v1.3's `lookup` was a stable sort by descending count, so equal
/// counts within a reading came out in ascending stored position; the new order is
/// descending number, so the later position must get the smaller number to keep
/// that order. Two elements of different readings with the same count have no
/// relative order to preserve (v1.3 never compared them); the reading's bytes
/// decide, which is arbitrary but deterministic.
fn plan_migration(rows: &[(String, Vec<StoredEntry>)], base: u64) -> Option<MigrationPlan> {
    let mut legacy: Vec<(u32, &str, usize, usize)> = Vec::new();
    for (row, (reading, entries)) in rows.iter().enumerate() {
        for (position, entry) in entries.iter().enumerate() {
            if entry.is_legacy() {
                legacy.push((
                    entry.frequency.unwrap_or(0),
                    reading.as_str(),
                    row,
                    position,
                ));
            }
        }
    }
    if legacy.is_empty() {
        return None;
    }
    legacy.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.as_bytes().cmp(b.1.as_bytes()))
            .then_with(|| b.3.cmp(&a.3))
    });

    let mut assigned: std::collections::HashMap<(usize, usize), u64> =
        std::collections::HashMap::new();
    let mut number = base;
    for (_, _, row, position) in &legacy {
        number = number.saturating_add(1);
        assigned.insert((*row, *position), number);
    }

    let writes = rows
        .iter()
        .enumerate()
        .filter(|(row, (_, entries))| {
            (0..entries.len()).any(|position| assigned.contains_key(&(*row, position)))
        })
        .map(|(row, (reading, entries))| {
            let rewritten = entries
                .iter()
                .enumerate()
                .map(|(position, entry)| DictEntry {
                    word: entry.word.clone(),
                    annotation: entry.annotation.clone(),
                    last_selected: assigned
                        .get(&(row, position))
                        .copied()
                        .unwrap_or_else(|| entry.last_selected.unwrap_or(0)),
                })
                .collect();
            (reading.clone(), rewritten)
        })
        .collect();

    Some((writes, number))
}

/// User dictionary
///
/// A read-write dictionary backed by a sled database. Keys are UTF-8 reading
/// strings and values are JSON-serialized `Vec<DictEntry>`. It records which
/// conversion candidate the user selected most recently and returns the most
/// recently selected candidate first (MRU, D-190).
pub struct UserDict {
    /// Path of the dictionary file.
    path: PathBuf,
    /// The sled database instance.
    db: sled::Db,
    /// In-memory romaji index (D-73). Interior mutability is required because the
    /// `Dictionary` methods take `&self` (the same reason as sled's interior
    /// mutability).
    roman_index: RwLock<MemoryRomanIndex>,
    /// In-memory SymSpell (delete-variant) index (D-95). Needs interior
    /// mutability for the same reason as `roman_index`.
    symspell_index: RwLock<MemorySymSpellIndex>,
    /// The highest sequence number in use - rebuilt from the stored values at
    /// `open`, never persisted on its own (D-190). `next_seq` hands out the
    /// following number.
    last_seq: AtomicU64,
    /// True for a handle opened with `migrate == false`: it must not write, so
    /// `record_selection` refuses with `DictError::ReadOnlyViolation` (WR-02).
    read_only: bool,
}

impl UserDict {
    /// Opens the user dictionary at the given path, migrating v1.3 values once
    ///
    /// Calls `open_with(path, true)`; see `open_with` for what the open does.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DictError> {
        Self::open_with(path, true)
    }

    /// Opens the user dictionary, optionally migrating v1.3 values (D-195)
    ///
    /// The database is created when the path does not exist. One scan over every
    /// key does three things: each key that is UTF-8 is passed to
    /// `MemoryRomanIndex::insert` and the SymSpell index; each value is read as
    /// `StoredEntry`s to find the highest sequence number in use (the start of
    /// `last_seq`, D-190) and the legacy elements. Keys that are not UTF-8 and
    /// values that are not JSON are skipped - never removed or rewritten - so a
    /// damaged user dictionary does not fail the whole `open`. A read error from
    /// sled itself is different: the scan decides the sequence base and what gets
    /// migrated, so it is returned as `DictError::BackendError` rather than skipped.
    ///
    /// When `migrate` is true and some element is legacy (it has a count and no
    /// sequence number - decided by the shape of the value alone, with no external
    /// marker or version key), the legacy elements are numbered by
    /// `plan_migration` and written in one `sled::Batch` and flushed before this
    /// returns, so the migration happens exactly once, and a dictionary without a
    /// legacy element is not written at all (a read-only location causes no new
    /// failure). A failed write or flush is returned as `DictError::BackendError`,
    /// which `sekka_user_dict_new` turns into a null handle and the existing
    /// dictionary-error notice. Nothing is printed and no copy of the old
    /// dictionary is made (D-198, D-199).
    ///
    /// `migrate: false` is the reading-only entry for `sekka-dict-tool dump`: the
    /// dictionary is opened without writing anything. The handle is read-only:
    /// `record_selection` returns `DictError::ReadOnlyViolation`, `save` does
    /// nothing and `mode` is `DictionaryMode::ReadOnly`, because a write would
    /// rewrite the readings it touches into the new shape and the legacy elements
    /// of those readings could no longer be migrated (the ordering invariant of
    /// D-195).
    ///
    /// User dictionaries hold on the order of a thousand keys, so building the
    /// index in one pass at startup is enough. Upstream splits the work into
    /// 2000-key timer slices to avoid blocking Emacs; we do not expect to
    /// violate SC-006 (do not slow down startup) this way (02.1-CONTEXT.md,
    /// Claude's Discretion).
    pub fn open_with(path: impl AsRef<Path>, migrate: bool) -> Result<Self, DictError> {
        let path = path.as_ref().to_path_buf();
        let db = sled::open(&path)?;

        let mut roman_index = MemoryRomanIndex::new();
        let mut symspell_index = MemorySymSpellIndex::new();
        let mut base: u64 = 0;
        let mut rows: Vec<(String, Vec<StoredEntry>)> = Vec::new();
        for item in db.iter() {
            // A sled read error is not a row to skip: `base` and the migration
            // depend on this scan seeing every row, so it fails the open.
            let (key, value) = item.map_err(DictError::BackendError)?;
            let Ok(reading) = std::str::from_utf8(&key) else {
                continue;
            };
            roman_index.insert(reading);
            symspell_index.insert(reading);

            let Ok(entries) = serde_json::from_slice::<Vec<StoredEntry>>(&value) else {
                continue;
            };
            for entry in &entries {
                base = base.max(entry.last_selected.unwrap_or(0));
            }
            if entries.iter().any(StoredEntry::is_legacy) {
                rows.push((reading.to_string(), entries));
            }
        }

        let mut last_seq = base;
        if migrate {
            if let Some((writes, new_max)) = plan_migration(&rows, base) {
                let mut batch = sled::Batch::default();
                for (reading, entries) in &writes {
                    batch.insert(reading.as_bytes(), serde_json::to_vec(entries)?);
                }
                db.apply_batch(batch)?;
                db.flush()?;
                last_seq = new_max;
            }
        }

        Ok(Self {
            path,
            db,
            roman_index: RwLock::new(roman_index),
            symspell_index: RwLock::new(symspell_index),
            last_seq: AtomicU64::new(last_seq),
            read_only: !migrate,
        })
    }

    /// Hands out the next sequence number (D-190)
    ///
    /// Called once per `record_selection`, outside the CAS closure, so a retried
    /// closure never consumes another number. Numbers are unique and increase
    /// across threads; the first number of an empty dictionary is 1.
    fn next_seq(&self) -> u64 {
        // A compare-exchange loop rather than `fetch_update`: that name is deprecated
        // on current stable, and its replacement `try_update` is newer than the MSRV.
        let mut current = self.last_seq.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_add(1);
            match self.last_seq.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return next,
                Err(actual) => current = actual,
            }
        }
    }

    /// Flushes the sled database to disk
    ///
    /// A read-only handle (`open_with(path, false)`) never writes, so there is
    /// nothing to flush and this does nothing - the same convention as the
    /// `Dictionary::save` default for read-only dictionaries.
    pub fn save(&self) -> Result<(), DictError> {
        if self.read_only {
            return Ok(());
        }
        self.db.flush()?;
        Ok(())
    }

    /// Loads the entries for a reading from the database
    ///
    /// Returns an empty vector when the key does not exist.
    fn load_entries(&self, reading: &str) -> Result<Vec<DictEntry>, DictError> {
        match self.db.get(reading.as_bytes())? {
            Some(data) => {
                let entries: Vec<DictEntry> = serde_json::from_slice(&data)?;
                Ok(entries)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Read-modify-write primitive under `record_selection`, the single write
    /// path of both learning and word registration (D-194: stamp the next
    /// sequence number, D-190), so that every write to the user dictionary goes
    /// through exactly one CAS discipline and exactly one pair of index-update
    /// hooks (RESEARCH Pitfall 1/4: an index hook duplicated per write-path is
    /// exactly the kind of place a future write-path could silently miss it).
    ///
    /// The read-modify-write is confined to a single `sled::Tree::fetch_and_update`
    /// call (a compare-and-swap), so `apply`'s effect is not lost when several
    /// threads record against the same reading concurrently
    /// (`record-selection-lost-update`, `01.2-REVIEW` WR-02 / `01.4-REVIEW`
    /// WR-02; pinned by regression tests where 8 threads x 100 iterations record
    /// distinct words - exactly 8 entries with 8 distinct `last_selected` numbers,
    /// the highest being 800 - and the same word, which lands on exactly 800).
    ///
    /// If either deserialization or serialization fails, the closure **returns
    /// the unmodified byte slice as `Some`** (an effectively no-op update).
    /// Returning `None` from a `fetch_and_update` closure deletes the key (the
    /// `next: None` of the `compare_and_swap(key, tmp, next)` that sled 0.34.7
    /// `tree.rs:715-738` calls internally is CASed as a deletion), so returning
    /// `None` here would destroy the user's learning data itself. Errors are
    /// captured into `serialize_error` outside the closure and propagated after
    /// `fetch_and_update` returns. Because `fetch_and_update` re-runs the closure
    /// on every CAS conflict, `serialize_error` is **reset to `None` at the top
    /// of the closure every time** (04-REVIEW.md WR-01). Without that reset, an
    /// error raised by a losing call could be returned as the result of the call
    /// that subsequently won the CAS (no data is lost, but a call that actually
    /// succeeded is reported as `Err` - a false negative).
    ///
    /// `apply` receives the reading's current entries (empty when the key does
    /// not yet exist) and mutates them in place; it may be called more than
    /// once (retried on every CAS conflict), so it must have no side effects
    /// beyond mutating its argument.
    fn update_entries_atomically<F>(&self, reading: &str, mut apply: F) -> Result<(), DictError>
    where
        F: FnMut(&mut Vec<DictEntry>),
    {
        let mut serialize_error: Option<DictError> = None;

        self.db
            .fetch_and_update(reading.as_bytes(), |old: Option<&[u8]>| {
                // Always reset on every CAS retry (04-REVIEW.md WR-01).
                // `fetch_and_update` re-runs this closure on every CAS conflict,
                // but `serialize_error` is a single variable declared outside the
                // closure, so without a reset here an error raised by the
                // previous (losing) call would survive and masquerade as the
                // result of this (winning) call. Resetting per call guarantees
                // that the value finally observed always belongs to the most
                // recent call - the one whose CAS actually succeeded.
                serialize_error = None;
                let mut entries: Vec<DictEntry> = match old {
                    Some(bytes) => match serde_json::from_slice(bytes) {
                        Ok(v) => v,
                        Err(e) => {
                            // Deserialization failed: return the original bytes
                            // unmodified (returning None would delete the key).
                            serialize_error = Some(DictError::SerializationError(e.to_string()));
                            return old.map(|b| b.to_vec());
                        }
                    },
                    None => Vec::new(),
                };

                apply(&mut entries);

                match serde_json::to_vec(&entries) {
                    Ok(json) => Some(json),
                    Err(e) => {
                        serialize_error = Some(DictError::SerializationError(e.to_string()));
                        old.map(|b| b.to_vec())
                    }
                }
            })
            .map_err(DictError::BackendError)?;

        if let Some(e) = serialize_error {
            return Err(e);
        }

        // The hooks for D-73/D-95 (Pitfall 2: outside the CAS, exactly once after
        // success is certain). Both `insert` calls deduplicate internally, so
        // they are idempotent; putting them here expresses the design intent
        // that this is one-shot post-processing unrelated to the number of CAS
        // retries. Do not drop either hook - the romaji index of D-73 or the
        // SymSpell index of D-95 (as the warning in the code says).
        // D-73/D-95 used to have no observable effect through learning alone
        // (every user dictionary key also existed in the master dictionary,
        // since `learn_pair` only becomes `Some` for a candidate that already
        // came from some dictionary). Word registration (D-194:
        // `finish_registration` -> `record_selection`) is what first creates a
        // reading that exists only in the user dictionary, and this hook is
        // what makes that reading reachable through fuzzy search (REG-08).
        if let Ok(mut idx) = self.roman_index.write() {
            idx.insert(reading);
        }
        if let Ok(mut idx) = self.symspell_index.write() {
            idx.insert(reading);
        }

        Ok(())
    }
}

impl Dictionary for UserDict {
    /// Performs an exact-match lookup by reading
    ///
    /// Results are sorted by most recently selected first (descending
    /// `last_selected`).
    fn lookup(&self, reading: &str) -> Result<Vec<DictEntry>, DictError> {
        let mut entries = self.load_entries(reading)?;
        // Sort by most recently selected first.
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_selected));
        Ok(entries)
    }

    /// Performs a prefix search over readings
    ///
    /// Returns every reading starting with the given prefix together with its
    /// conversion candidates. The candidates of each reading are sorted by most
    /// recently selected first.
    fn prefix_search(&self, prefix: &str) -> Result<Vec<(String, Vec<DictEntry>)>, DictError> {
        let mut results = Vec::new();

        for item in self.db.scan_prefix(prefix.as_bytes()) {
            let (key, value) = item?;
            let reading = String::from_utf8(key.to_vec()).map_err(|e| {
                DictError::SerializationError(format!("failed to decode a key as UTF-8: {}", e))
            })?;
            let mut entries: Vec<DictEntry> = serde_json::from_slice(&value)?;
            // Sort by most recently selected first.
            entries.sort_by_key(|e| std::cmp::Reverse(e.last_selected));
            results.push((reading, entries));
        }

        Ok(results)
    }

    /// Returns the path of the dictionary file
    fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the operating mode of the dictionary (read-write, or read-only for
    /// a handle opened with `open_with(path, false)`)
    fn mode(&self) -> DictionaryMode {
        if self.read_only {
            return DictionaryMode::ReadOnly;
        }
        DictionaryMode::ReadWrite
    }

    /// Records the conversion candidate the user selected as the most recently
    /// selected one (D-36, D-103, D-190)
    ///
    /// Stamps the candidate for the given reading with the next number of the
    /// dictionary-wide sequence, whether or not it was already first (D-192: a
    /// word committed as the first candidate is selected too). When no such entry
    /// exists, it is created with that number (D-40: keep the overlay sparse). The
    /// number is taken once, outside the CAS closure, so a closure that is re-run
    /// after a conflict does not consume another number; the closure itself only
    /// takes `max(old, seq)`. Implemented as a single `update_entries_atomically`
    /// call; see that method's documentation for the CAS discipline (atomicity,
    /// error handling, the index hooks). Word registration is recorded here
    /// too (D-194): the registered word is the most recently selected word of
    /// its reading, so it heads the reading and is never duplicated (REG-06).
    fn record_selection(&self, reading: &str, word: &str) -> Result<(), DictError> {
        if self.read_only {
            return Err(DictError::ReadOnlyViolation);
        }
        let seq = self.next_seq();
        self.update_entries_atomically(reading, |entries| {
            let mut found = false;
            for entry in entries.iter_mut() {
                if entry.word == word {
                    entry.last_selected = entry.last_selected.max(seq);
                    found = true;
                    break;
                }
            }
            if !found {
                entries.push(DictEntry::new(word).with_last_selected(seq));
            }
        })
    }

    /// Delegates a save request through the trait to `UserDict::save()`
    /// (`self.db.flush()`) (D-102)
    ///
    /// It has the same name as the inherent method `UserDict::save`, so it is
    /// called fully qualified (`self.save()` would recurse forever).
    fn save(&self) -> Result<(), DictError> {
        UserDict::save(self)
    }

    /// Reads the romaji index bucket for a romaji prefix (D-59/D-73)
    ///
    /// When the lock cannot be acquired (poisoned), returns an empty vector so
    /// the conversion path is not stopped (the mitigation for T-02.1-09).
    fn roman_bucket(&self, roman_prefix: &str) -> Result<Vec<RomanKey>, DictError> {
        match self.roman_index.read() {
            Ok(index) => Ok(index.bucket(roman_prefix)),
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Reads the in-memory SymSpell index bucket for the delete variant
    /// `variant` (D-95)
    ///
    /// When the lock cannot be acquired (poisoned), returns an empty vector so
    /// the conversion path is not stopped (same as `roman_bucket`, following the
    /// mitigation for T-02.1-09). The contents of user dictionary keys (the
    /// history of words the user actually typed) are never written to stdout,
    /// logs or error messages through this path either.
    fn symspell_bucket(&self, variant: &str) -> Result<Vec<String>, DictError> {
        match self.symspell_index.read() {
            Ok(index) => Ok(index.bucket(variant)),
            Err(_) => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that creates a user dictionary in a temporary test directory
    fn create_test_dict() -> (UserDict, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");
        let dict = UserDict::open(&dict_path).expect("failed to open the user dictionary");
        (dict, tmp)
    }

    #[test]
    fn recording_a_selection_stamps_a_nonzero_last_selected() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].word, "漢字");
        assert_eq!(
            entries[0].last_selected, 1,
            "the first number is 1 (0 = never)"
        );

        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].last_selected, 2);
    }

    #[test]
    fn lookup_results_are_sorted_by_most_recently_selected() {
        let (dict, _tmp) = create_test_dict();

        // 感じ x3, 漢字 x5, 幹事 x1: the count does not matter, the last one wins.
        for _ in 0..3 {
            dict.record_selection("かんじ", "感じ")
                .expect("failed to record the selection");
        }
        for _ in 0..5 {
            dict.record_selection("かんじ", "漢字")
                .expect("failed to record the selection");
        }
        dict.record_selection("かんじ", "幹事")
            .expect("failed to record the selection");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        let words: Vec<&str> = entries.iter().map(|e| e.word.as_str()).collect();
        assert_eq!(words, vec!["幹事", "漢字", "感じ"]);
        assert!(entries[0].last_selected > entries[1].last_selected);
        assert!(entries[1].last_selected > entries[2].last_selected);
    }

    #[test]
    fn recording_the_same_word_repeatedly_keeps_one_entry_with_an_increasing_last_selected() {
        let (dict, _tmp) = create_test_dict();

        let mut previous = 0;
        for _ in 0..10 {
            dict.record_selection("とうきょう", "東京")
                .expect("failed to record the selection");
            let entries = dict.lookup("とうきょう").expect("lookup failed");
            assert_eq!(entries.len(), 1);
            assert!(
                entries[0].last_selected > previous,
                "each record must stamp a larger number than the previous one"
            );
            previous = entries[0].last_selected;
        }
        assert_eq!(previous, 10);
    }

    #[test]
    fn looking_up_an_unknown_reading_returns_nothing() {
        let (dict, _tmp) = create_test_dict();

        let entries = dict.lookup("そんざいしない").expect("lookup failed");
        assert!(entries.is_empty());
    }

    #[test]
    fn prefix_search_finds_several_readings() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("かん", "缶")
            .expect("failed to record the selection");
        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");
        dict.record_selection("かんたん", "簡単")
            .expect("failed to record the selection");
        dict.record_selection("きかん", "期間")
            .expect("failed to record the selection");

        let results = dict.prefix_search("かん").expect("prefix_search failed");

        // Only readings starting with 「かん」 come back (「きかん」 is excluded).
        let readings: Vec<&str> = results.iter().map(|(r, _)| r.as_str()).collect();
        assert!(readings.contains(&"かん"));
        assert!(readings.contains(&"かんじ"));
        assert!(readings.contains(&"かんたん"));
        assert!(!readings.contains(&"きかん"));
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn the_dictionary_mode_is_read_write() {
        let (dict, _tmp) = create_test_dict();
        assert_eq!(dict.mode(), DictionaryMode::ReadWrite);
    }

    #[test]
    fn flushing_completes_successfully() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("てすと", "テスト")
            .expect("failed to record the selection");
        dict.save().expect("flush failed");
    }

    // === D-73: in-memory romaji index ===

    /// RED target: `roman_bucket` is still a stub (always empty), so the key
    /// written by `record_selection` is not found in the index right after open
    /// and this fails.
    #[test]
    fn opening_a_dictionary_with_existing_keys_populates_the_index() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");

        {
            let dict = UserDict::open(&dict_path).expect("failed to open the user dictionary");
            dict.record_selection("にほんご", "日本語")
                .expect("failed to record the selection");
            dict.save().expect("flush failed");
        }

        // Reopen and confirm the index is rebuilt on open.
        let reopened = UserDict::open(&dict_path).expect("failed to reopen the user dictionary");
        let bucket = reopened.roman_bucket("ni").expect("roman_bucket failed");
        assert!(
            bucket.iter().any(|rk| rk.reading == "にほんご"),
            "にほんご should be in roman_bucket right after reopening: {:?}",
            bucket
        );
    }

    #[test]
    fn record_selection_incrementally_adds_a_new_reading_to_the_index() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("あたらしいよみ", "新語")
            .expect("failed to record the selection");

        let bucket = dict.roman_bucket("ata").expect("roman_bucket failed");
        assert!(
            bucket.iter().any(|rk| rk.reading == "あたらしいよみ"),
            "あたらしいよみ should be in roman_bucket right after record_selection: {:?}",
            bucket
        );
    }

    #[test]
    fn recording_the_same_reading_twice_does_not_duplicate_it_in_the_index() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");
        dict.record_selection("かんじ", "幹事")
            .expect("failed to record the selection");

        let bucket = dict.roman_bucket("ka").expect("roman_bucket failed");
        let count = bucket.iter().filter(|rk| rk.reading == "かんじ").count();
        assert_eq!(count, 1, "かんじ should not be duplicated: {:?}", bucket);
    }

    // === D-95: in-memory SymSpell index ===

    #[test]
    fn open_bulk_builds_the_symspell_index_from_existing_keys() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");

        {
            let dict = UserDict::open(&dict_path).expect("failed to open the user dictionary");
            dict.record_selection("にほんご", "日本語")
                .expect("failed to record the selection");
            dict.save().expect("flush failed");
        }

        // Reopen and confirm the SymSpell index is rebuilt on open.
        let reopened = UserDict::open(&dict_path).expect("failed to reopen the user dictionary");
        let bucket = reopened
            .symspell_bucket("にほご")
            .expect("symspell_bucket failed");
        assert!(
            bucket.contains(&"にほんご".to_string()),
            "にほんご should be in symspell_bucket right after reopening: {:?}",
            bucket
        );
    }

    #[test]
    fn record_selection_incrementally_adds_a_new_reading_to_the_symspell_index() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("あたらしいよみ", "新語")
            .expect("failed to record the selection");

        // Deleting one character from "あたらしいよみ" gives "あたらしよみ" (「い」 removed), which finds it.
        let bucket = dict
            .symspell_bucket("あたらしよみ")
            .expect("symspell_bucket failed");
        assert!(
            bucket.contains(&"あたらしいよみ".to_string()),
            "あたらしいよみ should be in symspell_bucket right after record_selection: {:?}",
            bucket
        );
    }

    #[test]
    fn okuri_ari_conventional_keys_also_enter_the_symspell_index() {
        // D-94: MemoryRomanIndex excludes keys ending in an ASCII letter
        // (okuri-ari conventional keys), while MemorySymSpellIndex has no
        // exclusion rules at all (the asymmetry).
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("かんj", "感")
            .expect("failed to record the selection");

        // It does not enter the romaji index (the existing exclusion rule).
        let roman_bucket = dict.roman_bucket("ka").expect("roman_bucket failed");
        assert!(
            !roman_bucket.iter().any(|rk| rk.reading == "かんj"),
            "an okuri-ari conventional key should not enter the romaji index: {:?}",
            roman_bucket
        );

        // It does enter the SymSpell index (deleting "j" from "かんj" gives "かん", which finds it).
        let symspell_bucket = dict
            .symspell_bucket("かん")
            .expect("symspell_bucket failed");
        assert!(
            symspell_bucket.contains(&"かんj".to_string()),
            "an okuri-ari conventional key should enter the SymSpell index: {:?}",
            symspell_bucket
        );
    }

    // === D-102: Dictionary::save() の UserDict override ===

    #[test]
    fn save_through_the_trait_returns_ok_on_userdict() {
        let (dict, _tmp) = create_test_dict();
        let dict_ref: &dyn Dictionary = &dict;
        assert!(dict_ref.save().is_ok());
    }

    #[test]
    fn calling_the_equivalent_of_save_dictionaries_twice_leaves_the_contents_unchanged() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");

        let before = dict.prefix_search("").expect("prefix_search failed");

        let dict_ref: &dyn Dictionary = &dict;
        dict_ref.save().expect("the first save failed");
        dict_ref.save().expect("the second save failed");

        let after = dict.prefix_search("").expect("prefix_search failed");
        assert_eq!(
            before, after,
            "calling save twice should leave the contents unchanged"
        );
    }

    // === D-103: CAS atomicity of record_selection ===

    #[test]
    fn record_selection_keeps_other_words_last_selected_when_adding_another_word() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("かんじ", "漢字")
            .expect("the first record failed");
        dict.record_selection("かんじ", "漢字")
            .expect("the second record failed");
        dict.record_selection("かんじ", "幹事")
            .expect("recording the other word failed");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(
            entries.len(),
            2,
            "there should be one more entry: {:?}",
            entries
        );
        let kanji = entries
            .iter()
            .find(|e| e.word == "漢字")
            .expect("漢字 not found");
        assert_eq!(kanji.last_selected, 2, "the existing number should be kept");
        let kanji_role = entries
            .iter()
            .find(|e| e.word == "幹事")
            .expect("幹事 not found");
        assert_eq!(kanji_role.last_selected, 3);
    }

    #[test]
    fn concurrent_selection_of_distinct_words_keeps_every_entry_with_unique_last_selected() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");
        let dict = std::sync::Arc::new(
            UserDict::open(&dict_path).expect("failed to open the user dictionary"),
        );

        let mut handles = Vec::new();
        for i in 0..8 {
            let dict = std::sync::Arc::clone(&dict);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    dict.record_selection("かんじ", &format!("語{}", i))
                        .expect("failed to record the selection");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("failed to join a thread");
        }

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 8, "no word may be lost: {:?}", entries);
        let numbers: std::collections::HashSet<u64> =
            entries.iter().map(|e| e.last_selected).collect();
        assert_eq!(numbers.len(), 8, "every word needs its own number");
        assert_eq!(
            entries.iter().map(|e| e.last_selected).max(),
            Some(800),
            "800 numbers were handed out in total"
        );
    }

    #[test]
    fn concurrent_selection_of_the_same_word_lands_on_the_last_issued_number() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");
        let dict = std::sync::Arc::new(
            UserDict::open(&dict_path).expect("failed to open the user dictionary"),
        );

        let mut handles = Vec::new();
        for _ in 0..8 {
            let dict = std::sync::Arc::clone(&dict);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    dict.record_selection("かんじ", "漢字")
                        .expect("failed to record the selection");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("failed to join a thread");
        }

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].last_selected, 800,
            "the entry must end up on the last number issued (max, never overwritten by a smaller one)"
        );
    }

    #[test]
    fn the_last_selected_sequence_continues_after_a_reopen() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");

        {
            let dict = UserDict::open(&dict_path).expect("failed to open the user dictionary");
            dict.record_selection("かんじ", "漢字")
                .expect("failed to record a");
            dict.record_selection("かんじ", "幹事")
                .expect("failed to record b");
            dict.save().expect("flush failed");
        }

        let reopened = UserDict::open(&dict_path).expect("failed to reopen the user dictionary");
        reopened
            .record_selection("かんじ", "感じ")
            .expect("failed to record c");

        let entries = reopened.lookup("かんじ").expect("lookup failed");
        let c = entries
            .iter()
            .find(|e| e.word == "感じ")
            .expect("感じ not found");
        assert_eq!(
            c.last_selected, 3,
            "the sequence must continue from the stored maximum"
        );
        assert_eq!(entries[0].word, "感じ");
    }

    #[test]
    fn opening_skips_an_unparseable_value_when_rebuilding_the_sequence() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");

        {
            let dict = UserDict::open(&dict_path).expect("failed to open the user dictionary");
            dict.db
                .insert("こわれ".as_bytes(), b"not json".to_vec())
                .expect("the direct write failed");
            dict.save().expect("flush failed");
        }

        let reopened = UserDict::open(&dict_path).expect("open must not fail on a broken value");
        let raw = reopened
            .db
            .get("こわれ".as_bytes())
            .expect("failed to access the db")
            .expect("the key was deleted");
        assert_eq!(
            raw.as_ref(),
            b"not json",
            "the broken value must stay as it is"
        );

        reopened
            .record_selection("かんじ", "漢字")
            .expect("failed to record the selection");
        let entries = reopened.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries[0].last_selected, 1);
    }

    #[test]
    fn record_selection_returns_an_error_without_deleting_a_key_holding_a_broken_value() {
        let (dict, _tmp) = create_test_dict();

        dict.db
            .insert("かんじ".as_bytes(), b"not json".to_vec())
            .expect("the direct write failed");

        let result = dict.record_selection("かんじ", "漢字");
        assert!(result.is_err(), "a broken value should yield Err");

        let raw = dict
            .db
            .get("かんじ".as_bytes())
            .expect("failed to access the db")
            .expect("the key was deleted");
        assert_eq!(
            raw.as_ref(),
            b"not json",
            "a key holding a broken value should keep its original bytes rather than be deleted"
        );
    }

    // === D-194: word registration goes through record_selection ===

    /// REG-06 / D-194: recording the same (reading, word) pair again never
    /// duplicates it, and the most recent record is always the head.
    #[test]
    fn record_selection_never_duplicates_a_reading_word_pair() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("せっか", "石火")
            .expect("the first record failed");
        dict.record_selection("せっか", "石火")
            .expect("the second record failed");

        let entries = dict.lookup("せっか").expect("lookup failed");
        assert_eq!(
            entries.len(),
            1,
            "recording the same (reading, word) pair twice must not duplicate it: {:?}",
            entries
        );
        assert_eq!(entries[0].word, "石火");

        for _ in 0..5 {
            dict.record_selection("せっか", "赤化")
                .expect("failed to record the selection");
        }
        let entries = dict.lookup("せっか").expect("lookup failed");
        assert_eq!(entries.len(), 2, "{:?}", entries);
        assert_eq!(entries[0].word, "赤化");

        dict.record_selection("せっか", "石火")
            .expect("the third record failed");
        let entries = dict.lookup("せっか").expect("lookup failed");
        assert_eq!(entries.len(), 2, "{:?}", entries);
        assert_eq!(entries[0].word, "石火");
        assert_eq!(entries[1].word, "赤化");
        assert!(
            entries[0].last_selected > entries[1].last_selected,
            "the latest record must carry the highest number: {:?}",
            entries
        );
    }

    /// REG-08 / D-194: a reading that exists only in the user dictionary (a
    /// registered word, not a learned master-dictionary candidate) reaches the
    /// roman and SymSpell indexes through `record_selection`'s index hooks.
    #[test]
    fn record_selection_indexes_a_reading_that_exists_only_in_the_user_dictionary() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("りなっくす", "Linux")
            .expect("failed to record the selection");

        let roman_bucket = dict.roman_bucket("rin").expect("roman_bucket failed");
        assert!(
            roman_bucket.iter().any(|rk| rk.reading == "りなっくす"),
            "りなっくす should be in roman_bucket right after record_selection: {:?}",
            roman_bucket
        );

        // Deleting one character from "りなっくす" gives "りなくす" (「っ」 removed).
        let symspell_bucket = dict
            .symspell_bucket("りなくす")
            .expect("symspell_bucket failed");
        assert!(
            symspell_bucket.contains(&"りなっくす".to_string()),
            "りなっくす should be in symspell_bucket right after record_selection: {:?}",
            symspell_bucket
        );
    }

    // === D-195: one-time migration of v1.3 values ===

    /// Builds `plan_migration` input from `(reading, JSON value)` pairs. The JSON
    /// strings are v1.3 (`word, annotation, frequency`) or v1.4 serializations.
    fn stored_rows(spec: &[(&str, &str)]) -> Vec<(String, Vec<StoredEntry>)> {
        spec.iter()
            .map(|(reading, json)| {
                (
                    reading.to_string(),
                    serde_json::from_str(json).expect("the test JSON must be valid"),
                )
            })
            .collect()
    }

    /// The number a planned write gives to `(reading, word)`.
    fn planned_number(writes: &[(String, Vec<DictEntry>)], reading: &str, word: &str) -> u64 {
        writes
            .iter()
            .find(|(r, _)| r == reading)
            .and_then(|(_, entries)| entries.iter().find(|e| e.word == word))
            .unwrap_or_else(|| panic!("{reading}/{word} is not in the planned writes"))
            .last_selected
    }

    #[test]
    fn plan_migration_returns_none_when_no_legacy_entry_exists() {
        assert!(plan_migration(&[], 0).is_none());

        let rows = stored_rows(&[
            (
                "かんじ",
                r#"[{"word":"漢字","annotation":null,"last_selected":5}]"#,
            ),
            // An element with neither key is not legacy (v1.3 could not write one).
            ("こわれ", r#"[{"word":"x"}]"#),
        ]);
        assert!(plan_migration(&rows, 5).is_none());
    }

    #[test]
    fn plan_migration_numbers_legacy_entries_by_ascending_frequency_across_readings() {
        let rows = stored_rows(&[
            (
                "かんじ",
                r#"[{"word":"漢字","annotation":null,"frequency":1},{"word":"幹事","annotation":null,"frequency":5}]"#,
            ),
            (
                "かんj",
                r#"[{"word":"感","annotation":null,"frequency":2}]"#,
            ),
        ]);
        let (writes, max) = plan_migration(&rows, 0).expect("legacy entries exist");

        assert_eq!(planned_number(&writes, "かんじ", "漢字"), 1);
        assert_eq!(planned_number(&writes, "かんj", "感"), 2);
        assert_eq!(planned_number(&writes, "かんじ", "幹事"), 3);
        assert_eq!(max, 3);

        // Equal counts across readings: the reading's bytes decide (ascending).
        let rows = stored_rows(&[
            ("い", r#"[{"word":"y","annotation":null,"frequency":1}]"#),
            ("あ", r#"[{"word":"x","annotation":null,"frequency":1}]"#),
        ]);
        let (writes, _) = plan_migration(&rows, 0).expect("legacy entries exist");
        assert_eq!(planned_number(&writes, "あ", "x"), 1);
        assert_eq!(planned_number(&writes, "い", "y"), 2);
    }

    #[test]
    fn plan_migration_keeps_the_v13_lookup_order_for_equal_frequencies_within_a_reading() {
        // v1.3 sorted stably by descending count, so A (stored first) came first.
        let rows = stored_rows(&[(
            "かんじ",
            r#"[{"word":"A","annotation":null,"frequency":3},{"word":"B","annotation":null,"frequency":3}]"#,
        )]);
        let (writes, _) = plan_migration(&rows, 0).expect("legacy entries exist");

        assert!(
            planned_number(&writes, "かんじ", "A") > planned_number(&writes, "かんじ", "B"),
            "A must outrank B as it did in v1.3: {:?}",
            writes
        );
    }

    #[test]
    fn plan_migration_gives_a_zero_frequency_entry_a_positive_number() {
        let rows = stored_rows(&[(
            "とうきょう",
            r#"[{"word":"東京","annotation":null,"frequency":0}]"#,
        )]);
        let (writes, max) = plan_migration(&rows, 0).expect("legacy entries exist");

        assert_eq!(planned_number(&writes, "とうきょう", "東京"), 1);
        assert_eq!(max, 1);
    }

    #[test]
    fn plan_migration_stacks_legacy_entries_above_the_highest_existing_number() {
        let rows = stored_rows(&[(
            "かんじ",
            r#"[{"word":"漢字","annotation":null,"last_selected":7},{"word":"幹事","annotation":null,"frequency":1}]"#,
        )]);
        let (writes, max) = plan_migration(&rows, 7).expect("legacy entries exist");

        assert_eq!(planned_number(&writes, "かんじ", "漢字"), 7);
        assert_eq!(planned_number(&writes, "かんじ", "幹事"), 8);
        assert_eq!(max, 8);
    }

    #[test]
    fn plan_migration_keeps_annotations_and_drops_the_frequency_key() {
        let rows = stored_rows(&[(
            "とうきょう",
            r#"[{"word":"東京","annotation":"地名","frequency":4}]"#,
        )]);
        let (writes, _) = plan_migration(&rows, 0).expect("legacy entries exist");

        assert_eq!(writes[0].1[0].annotation, Some("地名".to_string()));
        let json = serde_json::to_string(&writes[0].1).expect("serialization failed");
        assert!(
            !json.contains("frequency"),
            "the old key must not be written: {json}"
        );
    }

    #[test]
    fn open_migrates_a_v13_dictionary_once_preserving_each_readings_order() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let dict_path = tmp.path().join("test_user_dict");

        {
            // Build the v1.3 dictionary without migrating it.
            let old = UserDict::open_with(&dict_path, false).expect("failed to open");
            old.db
                .insert(
                    "かんじ".as_bytes(),
                    r#"[{"word":"漢字","annotation":null,"frequency":1},{"word":"幹事","annotation":null,"frequency":5},{"word":"感じ","annotation":null,"frequency":3}]"#
                        .as_bytes(),
                )
                .expect("the direct write failed");
            old.db
                .insert(
                    "かんj".as_bytes(),
                    r#"[{"word":"感","annotation":null,"frequency":2}]"#.as_bytes(),
                )
                .expect("the direct write failed");
            old.db.flush().expect("flush failed");
        }

        let dict = UserDict::open(&dict_path).expect("failed to open the user dictionary");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        let words: Vec<&str> = entries.iter().map(|e| e.word.as_str()).collect();
        assert_eq!(
            words,
            vec!["幹事", "感じ", "漢字"],
            "the v1.3 order is kept"
        );
        let numbers: Vec<u64> = entries.iter().map(|e| e.last_selected).collect();
        assert_eq!(numbers, vec![4, 3, 1]);

        let entries = dict.lookup("かんj").expect("lookup failed");
        assert_eq!(entries[0].last_selected, 2);

        let raw = dict
            .db
            .get("かんじ".as_bytes())
            .expect("failed to access the db")
            .expect("the key is missing");
        let raw = std::str::from_utf8(&raw).expect("the value must be UTF-8");
        assert!(
            !raw.contains("frequency"),
            "the migrated value must not carry the old key: {raw}"
        );

        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");
        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries[0].word, "漢字");
        assert_eq!(
            entries[0].last_selected, 5,
            "the sequence continues from the migrated maximum"
        );
    }

    // === D-195: boundary tests of the one-time migration (integration) ===

    /// Writes v1.3-shaped values straight into a new dictionary without migrating,
    /// then closes it (sled holds an exclusive lock, so the handle must be dropped
    /// before the next open).
    fn write_v13_dictionary(path: &Path, rows: &[(&str, &str)]) {
        let old = UserDict::open_with(path, false).expect("failed to open");
        for (reading, json) in rows {
            old.db
                .insert(reading.as_bytes(), json.as_bytes())
                .expect("the direct write failed");
        }
        old.db.flush().expect("flush failed");
    }

    /// Every raw `(key, value)` pair of the dictionary at `path`, in key order.
    fn raw_snapshot(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
        let db = sled::open(path).expect("failed to open the raw sled database");
        db.iter()
            .map(|pair| {
                let (key, value) = pair.expect("failed to read a raw pair");
                (key.to_vec(), value.to_vec())
            })
            .collect()
    }

    /// The raw value stored under `reading` in a snapshot.
    fn raw_value<'a>(snapshot: &'a [(Vec<u8>, Vec<u8>)], reading: &str) -> &'a [u8] {
        snapshot
            .iter()
            .find(|(key, _)| key == reading.as_bytes())
            .map(|(_, value)| value.as_slice())
            .unwrap_or_else(|| panic!("{reading} is not in the snapshot"))
    }

    const V13_KANJI: &str = r#"[{"word":"漢字","annotation":null,"frequency":1},{"word":"幹事","annotation":null,"frequency":5}]"#;
    const V13_KAN_J: &str = r#"[{"word":"感","annotation":null,"frequency":2}]"#;

    #[test]
    fn opening_a_migrated_dictionary_again_changes_nothing() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test_user_dict");
        write_v13_dictionary(&path, &[("かんじ", V13_KANJI), ("かんj", V13_KAN_J)]);

        // First open migrates.
        drop(UserDict::open(&path).expect("failed to open (migrating)"));
        let migrated = raw_snapshot(&path);
        assert!(
            !String::from_utf8_lossy(raw_value(&migrated, "かんじ")).contains("frequency"),
            "the first open must have migrated the dictionary"
        );

        // Opening it again - twice - writes nothing.
        drop(UserDict::open(&path).expect("failed to open (second)"));
        assert_eq!(raw_snapshot(&path), migrated);
        drop(UserDict::open(&path).expect("failed to open (third)"));
        assert_eq!(raw_snapshot(&path), migrated);
    }

    #[test]
    fn open_writes_nothing_to_a_dictionary_without_legacy_entries() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test_user_dict");
        {
            let dict = UserDict::open(&path).expect("failed to open");
            dict.record_selection("かんじ", "漢字")
                .expect("record failed");
            dict.record_selection("かんじ", "幹事")
                .expect("record failed");
            dict.record_selection("かんj", "感").expect("record failed");
            dict.save().expect("flush failed");
        }
        let before = raw_snapshot(&path);
        assert_eq!(before.len(), 2, "two readings were recorded");

        drop(UserDict::open(&path).expect("failed to open"));
        assert_eq!(
            raw_snapshot(&path),
            before,
            "open must not write when no element is legacy"
        );
    }

    #[test]
    fn open_leaves_an_unparseable_value_untouched_while_migrating_the_rest() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test_user_dict");
        let not_json = "not json";
        let negative = r#"[{"word":"a","annotation":null,"frequency":-1}]"#;
        write_v13_dictionary(
            &path,
            &[
                ("こわれ", not_json),
                ("まいなす", negative),
                ("かんじ", V13_KANJI),
            ],
        );
        let before = raw_snapshot(&path);

        let dict = UserDict::open(&path).expect("a damaged value must not fail the open");
        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.last_selected >= 1));
        drop(dict);

        let after = raw_snapshot(&path);
        assert_eq!(raw_value(&after, "こわれ"), not_json.as_bytes());
        assert_eq!(raw_value(&after, "まいなす"), negative.as_bytes());
        assert_eq!(
            raw_value(&after, "こわれ"),
            raw_value(&before, "こわれ"),
            "the unparseable value must be byte-for-byte unchanged"
        );
        assert_ne!(
            raw_value(&after, "かんじ"),
            raw_value(&before, "かんじ"),
            "the readable legacy value must have been migrated"
        );
        assert!(!String::from_utf8_lossy(raw_value(&after, "かんじ")).contains("frequency"));
    }

    #[test]
    fn open_with_migrate_false_never_writes_and_a_later_open_completes_the_migration() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test_user_dict");
        write_v13_dictionary(&path, &[("かんじ", V13_KANJI), ("かんj", V13_KAN_J)]);
        let before = raw_snapshot(&path);

        {
            // The non-migrating entry is the state a crash before the Batch leaves.
            let dict = UserDict::open_with(&path, false).expect("failed to open");
            let entries = dict.lookup("かんじ").expect("lookup failed");
            assert_eq!(entries.len(), 2);
            assert!(
                entries.iter().all(|e| e.last_selected == 0),
                "an unmigrated element reads as number 0: {entries:?}"
            );
        }
        assert_eq!(
            raw_snapshot(&path),
            before,
            "the non-migrating entry must not write"
        );

        // The next open completes the migration, preserving the v1.3 order.
        let dict = UserDict::open(&path).expect("failed to open");
        let entries = dict.lookup("かんじ").expect("lookup failed");
        let words: Vec<&str> = entries.iter().map(|e| e.word.as_str()).collect();
        assert_eq!(words, vec!["幹事", "漢字"]);
        assert_eq!(
            entries.iter().map(|e| e.last_selected).collect::<Vec<_>>(),
            vec![3, 1]
        );
        assert_eq!(
            dict.lookup("かんj").expect("lookup failed")[0].last_selected,
            2
        );
        drop(dict);
        assert!(
            !String::from_utf8_lossy(raw_value(&raw_snapshot(&path), "かんじ"))
                .contains("frequency")
        );
    }

    #[test]
    fn a_non_migrating_handle_refuses_to_record_and_leaves_legacy_rows_intact() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test_user_dict");
        write_v13_dictionary(&path, &[("かんじ", V13_KANJI), ("かんj", V13_KAN_J)]);
        let before = raw_snapshot(&path);

        {
            let dict = UserDict::open_with(&path, false).expect("failed to open");
            assert_eq!(dict.mode(), DictionaryMode::ReadOnly);
            assert!(
                matches!(
                    dict.record_selection("かんじ", "漢字"),
                    Err(DictError::ReadOnlyViolation)
                ),
                "recording on a non-migrating handle must be refused"
            );
            assert!(
                matches!(
                    dict.record_selection("あたらしい", "新しい"),
                    Err(DictError::ReadOnlyViolation)
                ),
                "a new reading must be refused too"
            );
            dict.save().expect("save on a read-only handle is a no-op");
        }
        assert_eq!(
            raw_snapshot(&path),
            before,
            "the refused writes must leave the legacy rows byte-for-byte intact"
        );

        // The migration is still possible afterwards, in the v1.3 order.
        let dict = UserDict::open(&path).expect("failed to open");
        assert_eq!(dict.mode(), DictionaryMode::ReadWrite);
        let words: Vec<String> = dict
            .lookup("かんじ")
            .expect("lookup failed")
            .into_iter()
            .map(|e| e.word)
            .collect();
        assert_eq!(words, vec!["幹事", "漢字"]);
    }

    #[test]
    fn open_stacks_legacy_entries_above_the_numbers_already_in_use() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test_user_dict");
        {
            let dict = UserDict::open(&path).expect("failed to open");
            for _ in 0..3 {
                dict.record_selection("かんじ", "漢字")
                    .expect("record failed");
            }
            dict.save().expect("flush failed");
        }
        {
            // A v1.3 reading appears next to the already-numbered one.
            let old = UserDict::open_with(&path, false).expect("failed to open");
            old.db
                .insert("かんj".as_bytes(), V13_KAN_J.as_bytes())
                .expect("the direct write failed");
            old.db.flush().expect("flush failed");
        }

        let dict = UserDict::open(&path).expect("failed to open");
        let kan_j = dict.lookup("かんj").expect("lookup failed");
        assert_eq!(
            kan_j[0].last_selected, 4,
            "the legacy element stacks above the highest number in use (3)"
        );
        let kanji = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(
            kanji[0].last_selected, 3,
            "the numbered reading is untouched"
        );

        dict.record_selection("かんj", "感").expect("record failed");
        assert_eq!(
            dict.lookup("かんj").expect("lookup failed")[0].last_selected,
            5,
            "the sequence continues above the migrated maximum"
        );
    }

    /// Measurement only (SC-006): how long `open` takes with a migration and with
    /// only the rescan of an already-migrated dictionary. It asserts nothing.
    /// Run: `cargo test --release -- --ignored --nocapture open_scan`
    #[test]
    #[ignore = "records timings for SC-006; not a pass/fail gate"]
    fn open_scan_time_is_recorded_for_1k_10k_100k_keys() {
        for n in [1_000usize, 10_000, 100_000] {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let path = tmp.path().join("test_user_dict");
            {
                let old = UserDict::open_with(&path, false).expect("failed to open");
                for i in 0..n {
                    let value = format!(
                        r#"[{{"word":"語{i}","annotation":null,"frequency":{}}},{{"word":"別{i}","annotation":null,"frequency":{}}}]"#,
                        i % 7 + 1,
                        i % 11 + 1
                    );
                    old.db
                        .insert(format!("よみ{i}").as_bytes(), value.as_bytes())
                        .expect("the direct write failed");
                }
                old.db.flush().expect("flush failed");
            }

            let started = std::time::Instant::now();
            drop(UserDict::open(&path).expect("failed to open (migrating)"));
            let migrate_ms = started.elapsed().as_millis();

            let started = std::time::Instant::now();
            drop(UserDict::open(&path).expect("failed to open (rescan)"));
            let rescan_ms = started.elapsed().as_millis();

            println!("open_scan n={n} migrate_ms={migrate_ms} rescan_ms={rescan_ms}");
        }
    }
}
