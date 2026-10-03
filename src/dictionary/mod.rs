// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Dictionary access layer
//!
//! Provides the `Dictionary` trait plus the file and user dictionary
//! implementations.

pub mod dict_format;
pub mod immutable_dict;
pub mod user_dict;

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Dictionary entry
///
/// Represents one conversion candidate: the converted text for a reading, its
/// annotation and when it was last selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictEntry {
    /// The converted text.
    pub word: String,
    /// Annotation (optional).
    pub annotation: Option<String>,
    /// The dictionary-wide sequence number of the last time the user selected
    /// this word (D-190, Phase 12): 0 means never selected, and the first
    /// selection in a dictionary gets 1. Master-dictionary entries are always 0.
    ///
    /// `#[serde(default)]` keeps master dictionaries built by v1.3's
    /// `sekka-dict-tool` readable: their value blobs carry only the old count
    /// key, which is ignored as an unknown field and never read as a sequence
    /// number (D-204, D-195). Serialization writes only this key (D-196).
    #[serde(default)]
    pub last_selected: u64,
}

impl DictEntry {
    /// Creates a new dictionary entry.
    pub fn new(word: impl Into<String>) -> Self {
        Self {
            word: word.into(),
            annotation: None,
            last_selected: 0,
        }
    }

    /// Sets the annotation.
    pub fn with_annotation(mut self, annotation: impl Into<String>) -> Self {
        self.annotation = Some(annotation.into());
        self
    }

    /// Sets the last-selected sequence number.
    pub fn with_last_selected(mut self, last_selected: u64) -> Self {
        self.last_selected = last_selected;
        self
    }
}

/// Dictionary operating mode
///
/// The master dictionary is read-only; the user dictionary is read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictionaryMode {
    /// Read-only (for the master dictionary).
    ReadOnly,
    /// Read-write (for the user dictionary).
    ReadWrite,
}

/// Errors that dictionary operations can raise
#[derive(Debug)]
pub enum DictError {
    /// The dictionary file was not found.
    NotFound(PathBuf),
    /// Reading or writing the dictionary data failed.
    IoError(std::io::Error),
    /// A sled database operation failed.
    BackendError(sled::Error),
    /// Serializing or deserializing an entry failed.
    SerializationError(String),
    /// A write to a read-only dictionary was attempted.
    ReadOnlyViolation,
    /// The dictionary file is corrupt (magic mismatch, version mismatch, header
    /// CRC32 mismatch, a table extending past the end of the file, and so on;
    /// D-64 (3)).
    CorruptFormat(String),
}

impl fmt::Display for DictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DictError::NotFound(path) => write!(f, "dictionary not found: {}", path.display()),
            DictError::IoError(e) => write!(f, "I/O error: {}", e),
            DictError::BackendError(e) => write!(f, "database error: {}", e),
            DictError::SerializationError(msg) => write!(f, "serialization error: {}", msg),
            DictError::ReadOnlyViolation => write!(f, "cannot write to a read-only dictionary"),
            DictError::CorruptFormat(msg) => write!(f, "dictionary file is corrupt: {}", msg),
        }
    }
}

impl std::error::Error for DictError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DictError::IoError(e) => Some(e),
            DictError::BackendError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DictError {
    fn from(e: std::io::Error) -> Self {
        DictError::IoError(e)
    }
}

impl From<sled::Error> for DictError {
    fn from(e: sled::Error) -> Self {
        DictError::BackendError(e)
    }
}

impl From<serde_json::Error> for DictError {
    fn from(e: serde_json::Error) -> Self {
        DictError::SerializationError(e.to_string())
    }
}

/// One entry of a romaji index bucket
///
/// `reading` is the kana key and `roman` its canonical Hepburn romaji. Several
/// `RomanKey`s may share the same `roman` (D-75: every collision between
/// homophonous romaji is kept; we do not port upstream's lossy one-to-one
/// romaji->kana hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomanKey {
    /// The kana key (a dictionary reading).
    pub reading: String,
    /// The canonical Hepburn romaji of `reading`.
    pub roman: String,
}

/// Dictionary trait
///
/// Defines the lookup interface shared by the master and user dictionaries.
pub trait Dictionary {
    /// Performs an exact-match lookup by reading.
    ///
    /// Returns every conversion candidate for the given reading.
    fn lookup(&self, reading: &str) -> Result<Vec<DictEntry>, DictError>;

    /// Performs a prefix search over readings.
    ///
    /// Returns every reading starting with the given prefix together with its
    /// conversion candidates. Used for fuzzy and incremental search.
    fn prefix_search(&self, prefix: &str) -> Result<Vec<(String, Vec<DictEntry>)>, DictError>;

    /// Returns the path of the dictionary file.
    fn path(&self) -> &Path;

    /// Returns the operating mode of the dictionary.
    fn mode(&self) -> DictionaryMode;

    /// Records the candidate the user selected as the most recently selected
    /// one (D-36, D-190)
    ///
    /// Word registration is recorded through this same method (D-194, Phase 12):
    /// there is no separate registration write, a registered word is simply the
    /// most recently selected word of its reading.
    ///
    /// The default implementation returns `DictError::ReadOnlyViolation` for
    /// read-only dictionaries. Only writable dictionaries (`UserDict`) override
    /// it. The receiver is `&self` so that sled's interior mutability lets us
    /// call it while the dictionary is shared through `Vec<Arc<dyn Dictionary>>`.
    fn record_selection(&self, _reading: &str, _word: &str) -> Result<(), DictError> {
        Err(DictError::ReadOnlyViolation)
    }

    /// Flushes the dictionary's pending changes to disk (D-102)
    ///
    /// The default implementation does nothing for read-only dictionaries (it
    /// returns `Ok(())`). Only writable dictionaries (`UserDict`) override it.
    /// Note that the default points the opposite way from `record_selection`
    /// (`record_selection` returns `Err(DictError::ReadOnlyViolation)`, `save`
    /// returns `Ok(())`): saving is treated as success rather than refusal,
    /// because for a read-only dictionary there is simply nothing to do.
    fn save(&self) -> Result<(), DictError> {
        Ok(())
    }

    /// Reads the romaji index bucket for a romaji prefix (D-59)
    ///
    /// The default implementation returns nothing. Implementations without an
    /// index (test mocks, and the user dictionary, which has no romaji index)
    /// silently drop out of fuzzy search - this is the intended design (see
    /// `<assumption_delta_decision>` in 02.1-01-PLAN.md). Implementations that
    /// do have an index (`ImmutableFileDict`) must override this method.
    fn roman_bucket(&self, _roman_prefix: &str) -> Result<Vec<RomanKey>, DictError> {
        Ok(Vec::new())
    }

    /// Looks up the delete-variant string `variant` (hashing it internally) and
    /// returns the original kana keys that have that delete variant (SymSpell;
    /// D-91/92/94, 03.1-01).
    ///
    /// The default implementation returns nothing. Implementations without an
    /// index (test mocks, and the user dictionary, which has no SymSpell index)
    /// silently drop out of fuzzy search - the intended design, same style as
    /// `roman_bucket`. Implementations that do have an index
    /// (`ImmutableFileDict`) must override this method.
    ///
    /// The argument is a string, not a hash. Hashing stays an implementation
    /// detail inside `ImmutableFileDict` and never reaches the trait boundary,
    /// which lets the user dictionary use a plain string-keyed structure.
    fn symspell_bucket(&self, _variant: &str) -> Result<Vec<String>, DictError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_round_trips_through_serialization() {
        let entry = DictEntry {
            word: "漢字".to_string(),
            annotation: Some("常用漢字".to_string()),
            last_selected: 42,
        };

        let json = serde_json::to_string(&entry).expect("serialization failed");
        let restored: DictEntry = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(entry, restored);
    }

    #[test]
    fn entry_without_an_annotation_serializes() {
        let entry = DictEntry::new("試験");

        let json = serde_json::to_string(&entry).expect("serialization failed");
        let restored: DictEntry = serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(restored.word, "試験");
        assert_eq!(restored.annotation, None);
        assert_eq!(restored.last_selected, 0);
    }

    #[test]
    fn builder_pattern_creates_an_entry() {
        let entry = DictEntry::new("変換")
            .with_annotation("テスト用")
            .with_last_selected(10);

        assert_eq!(entry.word, "変換");
        assert_eq!(entry.annotation, Some("テスト用".to_string()));
        assert_eq!(entry.last_selected, 10);
    }

    #[test]
    fn dictionary_modes_compare() {
        assert_eq!(DictionaryMode::ReadOnly, DictionaryMode::ReadOnly);
        assert_ne!(DictionaryMode::ReadOnly, DictionaryMode::ReadWrite);
    }

    #[test]
    fn error_display() {
        let err = DictError::ReadOnlyViolation;
        let msg = format!("{}", err);
        assert!(msg.contains("read-only"));
    }

    /// D-204 / D-195: a master dictionary built by v1.3's `sekka-dict-tool`
    /// stores `[{"word":..,"annotation":..,"frequency":0}]` value blobs. They
    /// must keep deserializing, and the old count is never read as a number.
    #[test]
    fn a_v13_master_dictionary_blob_still_deserializes_with_last_selected_zero() {
        let blob = r#"[{"word":"東京","annotation":null,"frequency":0}]"#;
        let entries: Vec<DictEntry> = serde_json::from_str(blob).expect("deserialization failed");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].word, "東京");
        assert_eq!(entries[0].last_selected, 0);

        let counted = r#"{"word":"東京","annotation":"地名","frequency":100}"#;
        let entry: DictEntry = serde_json::from_str(counted).expect("deserialization failed");
        assert_eq!(entry.annotation, Some("地名".to_string()));
        assert_eq!(
            entry.last_selected, 0,
            "an old count must never be read as a sequence number"
        );
    }

    /// D-196: only the new key is written; the old key never comes back.
    #[test]
    fn entry_serializes_last_selected_and_never_frequency() {
        let json = serde_json::to_string(&DictEntry::new("x").with_last_selected(5))
            .expect("serialization failed");

        assert_eq!(json, r#"{"word":"x","annotation":null,"last_selected":5}"#);
    }

    /// Pitfall 7: the master dictionary (read-only, immutable format) refuses
    /// `record_selection` through the trait's default implementation. This is
    /// the only test pinning the trait's read-only default - word registration
    /// writes through `record_selection` too (D-194), so there is no separate
    /// registration method left to refuse.
    #[test]
    fn read_only_dictionaries_refuse_record_selection() {
        use crate::dictionary::immutable_dict::ImmutableFileDict;
        use std::collections::BTreeMap;

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test.dict");
        let entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        dict_format::write_dict(&path, &entries).expect("write_dict failed");

        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");
        let result = dict.record_selection("せっか", "石火");
        assert!(
            matches!(result, Err(DictError::ReadOnlyViolation)),
            "a read-only dictionary should refuse record_selection: {:?}",
            result
        );
    }
}
