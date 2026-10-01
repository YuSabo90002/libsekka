// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! User dictionary (read-write) implementation
//!
//! A user dictionary backed by sled. It records and updates selection
//! frequencies.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::dictionary::{DictEntry, DictError, Dictionary, DictionaryMode, RomanKey};
use crate::roman_index::MemoryRomanIndex;
use crate::symspell::MemorySymSpellIndex;

/// User dictionary
///
/// A read-write dictionary backed by a sled database. Keys are UTF-8 reading
/// strings and values are JSON-serialized `Vec<DictEntry>`. It records how often
/// each conversion candidate was selected and returns frequent candidates first.
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
}

impl UserDict {
    /// Opens the user dictionary at the given path
    ///
    /// The database is created when the path does not exist. Every key from
    /// `db.iter()` is read as UTF-8 and passed to `MemoryRomanIndex::insert`.
    /// Keys that are not UTF-8 (or that fail to read) are skipped silently, so a
    /// damaged user dictionary does not fail the whole `open`.
    ///
    /// User dictionaries hold on the order of a thousand keys, so building the
    /// index in one pass at startup is enough. Upstream splits the work into
    /// 2000-key timer slices to avoid blocking Emacs; we do not expect to
    /// violate SC-006 (do not slow down startup) this way (02.1-CONTEXT.md,
    /// Claude's Discretion).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DictError> {
        let path = path.as_ref().to_path_buf();
        let db = sled::open(&path)?;

        let mut roman_index = MemoryRomanIndex::new();
        let mut symspell_index = MemorySymSpellIndex::new();
        for item in db.iter().flatten() {
            let (key, _) = item;
            if let Ok(reading) = std::str::from_utf8(&key) {
                roman_index.insert(reading);
                symspell_index.insert(reading);
            }
        }

        Ok(Self {
            path,
            db,
            roman_index: RwLock::new(roman_index),
            symspell_index: RwLock::new(symspell_index),
        })
    }

    /// Flushes the sled database to disk
    pub fn save(&self) -> Result<(), DictError> {
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

    /// Shared read-modify-write primitive behind both `record_selection`
    /// (+1) and `record_registration` (D-182: move to the head), so that
    /// every write to the user dictionary goes through exactly one CAS
    /// discipline and exactly one pair of index-update hooks (RESEARCH
    /// Pitfall 1/4: an index hook duplicated per write-path is exactly the
    /// kind of place a future write-path could silently miss it).
    ///
    /// The read-modify-write is confined to a single `sled::Tree::fetch_and_update`
    /// call (a compare-and-swap), so `apply`'s effect is not lost when several
    /// threads record against the same reading concurrently
    /// (`record-selection-lost-update`, `01.2-REVIEW` WR-02 / `01.4-REVIEW`
    /// WR-02; pinned by a regression test where 8 threads x 100 iterations must
    /// land on exactly 800, for both callers).
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
        // D-73/D-95 used to have no observable effect through `record_selection`
        // alone (every user dictionary key also existed in the master
        // dictionary, since `learn_pair` only becomes `Some` for a candidate
        // that already came from some dictionary). Phase 10's word registration
        // (D-182, `record_registration`) is what first creates a reading that
        // exists only in the user dictionary, and this shared hook is what
        // makes that reading reachable through fuzzy search (REG-08).
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
    /// Results are sorted by descending selection frequency.
    fn lookup(&self, reading: &str) -> Result<Vec<DictEntry>, DictError> {
        let mut entries = self.load_entries(reading)?;
        // Sort by descending frequency.
        entries.sort_by_key(|e| std::cmp::Reverse(e.frequency));
        Ok(entries)
    }

    /// Performs a prefix search over readings
    ///
    /// Returns every reading starting with the given prefix together with its
    /// conversion candidates. The candidates of each reading are sorted by
    /// descending frequency.
    fn prefix_search(&self, prefix: &str) -> Result<Vec<(String, Vec<DictEntry>)>, DictError> {
        let mut results = Vec::new();

        for item in self.db.scan_prefix(prefix.as_bytes()) {
            let (key, value) = item?;
            let reading = String::from_utf8(key.to_vec()).map_err(|e| {
                DictError::SerializationError(format!("failed to decode a key as UTF-8: {}", e))
            })?;
            let mut entries: Vec<DictEntry> = serde_json::from_slice(&value)?;
            // Sort by descending frequency.
            entries.sort_by_key(|e| std::cmp::Reverse(e.frequency));
            results.push((reading, entries));
        }

        Ok(results)
    }

    /// Returns the path of the dictionary file
    fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the operating mode of the dictionary (read-write)
    fn mode(&self) -> DictionaryMode {
        DictionaryMode::ReadWrite
    }

    /// Records the frequency of a conversion candidate the user selected (D-36, D-103)
    ///
    /// Increments the selection frequency of the candidate for the given
    /// reading. When no such entry exists, it is created with frequency 1 (D-40:
    /// keep the overlay sparse). Implemented as a single `update_entries_atomically`
    /// call whose closure applies exactly this rule; see that method's
    /// documentation for the CAS discipline (atomicity, error handling, the
    /// index hooks) shared with `record_registration`.
    fn record_selection(&self, reading: &str, word: &str) -> Result<(), DictError> {
        self.update_entries_atomically(reading, |entries| {
            let mut found = false;
            for entry in entries.iter_mut() {
                if entry.word == word {
                    entry.frequency += 1;
                    found = true;
                    break;
                }
            }
            if !found {
                entries.push(DictEntry::new(word).with_frequency(1));
            }
        })
    }

    /// Moves a registered word to the head of its reading (D-182, Phase 10)
    ///
    /// The new frequency is the maximum frequency among `reading`'s existing
    /// entries, plus one - for both a brand new word and a re-registration of
    /// the same (reading, word) pair (REG-06: the existing entry's frequency
    /// is replaced, not duplicated, so re-registering never creates a second
    /// entry for the same pair). Implemented as a single
    /// `update_entries_atomically` call; see that method's documentation for
    /// the CAS discipline shared with `record_selection`.
    fn record_registration(&self, reading: &str, word: &str) -> Result<(), DictError> {
        self.update_entries_atomically(reading, |entries| {
            let max_frequency = entries.iter().map(|e| e.frequency).max().unwrap_or(0);
            let new_frequency = max_frequency.saturating_add(1);
            let mut found = false;
            for entry in entries.iter_mut() {
                if entry.word == word {
                    entry.frequency = new_frequency;
                    found = true;
                    break;
                }
            }
            if !found {
                entries.push(DictEntry::new(word).with_frequency(new_frequency));
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
    fn recording_a_selection_increases_the_frequency() {
        let (dict, _tmp) = create_test_dict();

        // Record the first selection.
        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].word, "漢字");
        assert_eq!(entries[0].frequency, 1);

        // Record a second selection.
        dict.record_selection("かんじ", "漢字")
            .expect("failed to record the selection");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].word, "漢字");
        assert_eq!(entries[0].frequency, 2);
    }

    #[test]
    fn lookup_results_are_sorted_by_descending_frequency() {
        let (dict, _tmp) = create_test_dict();

        // Record several candidates for 「かんじ」 with different frequencies.
        // Select 「感じ」 three times.
        for _ in 0..3 {
            dict.record_selection("かんじ", "感じ")
                .expect("failed to record the selection");
        }
        // Select 「漢字」 five times.
        for _ in 0..5 {
            dict.record_selection("かんじ", "漢字")
                .expect("failed to record the selection");
        }
        // Select 「幹事」 once.
        dict.record_selection("かんじ", "幹事")
            .expect("failed to record the selection");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 3);
        // Descending frequency: 漢字(5) > 感じ(3) > 幹事(1)
        assert_eq!(entries[0].word, "漢字");
        assert_eq!(entries[0].frequency, 5);
        assert_eq!(entries[1].word, "感じ");
        assert_eq!(entries[1].frequency, 3);
        assert_eq!(entries[2].word, "幹事");
        assert_eq!(entries[2].frequency, 1);
    }

    #[test]
    fn recording_the_same_word_repeatedly_accumulates_frequency() {
        let (dict, _tmp) = create_test_dict();

        // Record the same reading and word ten times.
        for _ in 0..10 {
            dict.record_selection("とうきょう", "東京")
                .expect("failed to record the selection");
        }

        let entries = dict.lookup("とうきょう").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].word, "東京");
        assert_eq!(entries[0].frequency, 10);
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
    fn calling_record_selection_twice_yields_a_frequency_of_2() {
        let (dict, _tmp) = create_test_dict();

        dict.record_selection("かんじ", "漢字")
            .expect("the first record failed");
        dict.record_selection("かんじ", "漢字")
            .expect("the second record failed");

        let entries = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].frequency, 2);
    }

    #[test]
    fn record_selection_keeps_existing_frequencies_when_adding_another_word() {
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
        assert_eq!(kanji.frequency, 2, "the existing frequency should be kept");
        let kanji_role = entries
            .iter()
            .find(|e| e.word == "幹事")
            .expect("幹事 not found");
        assert_eq!(kanji_role.frequency, 1);
    }

    #[test]
    fn record_selection_lands_on_exactly_800_with_8_threads_of_100() {
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
            entries[0].frequency, 800,
            "8 threads x 100 iterations should land on exactly 800 (no lost updates)"
        );
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
}
