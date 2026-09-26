// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Read-only reader for the immutable master dictionary (mmap + binary search)
//!
//! Maps the on-disk layout defined by `dict_format` read-only and implements the
//! `Dictionary` trait. It never touches memmap2's mutable mapping APIs (the
//! mitigation for T-02.1-02: it prevents the accident where an unprivileged
//! process can no longer open a dictionary installed as root-owned 644).

use std::fs::File;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

use crate::dictionary::dict_format::{self, DictView};
use crate::dictionary::{DictEntry, DictError, Dictionary, DictionaryMode, RomanKey};
use crate::symspell;

/// Read-only dictionary backed by mmap and binary search
pub struct ImmutableFileDict {
    /// Path of the dictionary file.
    path: PathBuf,
    /// Kept so it outlives the mmap (drop order follows field declaration order).
    _file: File,
    /// The read-only mapping.
    mmap: Mmap,
}

impl ImmutableFileDict {
    /// Opens the immutable-format dictionary file at `path` as a read-only mmap.
    ///
    /// With `File::open` (read-only), `ErrorKind::NotFound` becomes
    /// `DictError::NotFound` and any other I/O error becomes `DictError::IoError`.
    /// Immediately after mapping, `dict_format::validate` runs and
    /// `DictError::CorruptFormat` is returned on failure (no offset value is used
    /// before that validation passes). Unlike sled, this never creates an empty
    /// dictionary for a path that does not exist (D-64).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DictError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DictError::NotFound(path.clone())
            } else {
                DictError::IoError(e)
            }
        })?;
        // SAFETY: the mapping is read-only. The file stays open read-only for the
        // whole scope of this function and no mutable mapping API is ever
        // touched. That no other process rewrites the file while it is mapped is
        // ideally guaranteed by how the caller deploys it (root-owned 644, under
        // package management); this function itself does not verify that
        // condition (02.1-REVIEW CR-01). The production caller,
        // `openDictionaries()` in `fcitx5-sekka`, does provide a minimal defence
        // by rejecting paths that are writable by the running user via
        // `access(path, W_OK)` before mapping them (the path is configurable from
        // the settings UI, and that single layer cannot fully prevent rewrites by
        // another process or by root, nor SIGBUS - see 02.1-REVIEW CR-01 for
        // possible future measures). Other callers of this library function
        // (tests, `sekka-dict-tool` and so on) are responsible for satisfying the
        // no-rewrite-while-mapped precondition themselves.
        let mmap = unsafe { Mmap::map(&file) }.map_err(DictError::IoError)?;
        dict_format::validate(&mmap[..])?;
        Ok(Self {
            path,
            _file: file,
            mmap,
        })
    }

    fn view(&self) -> DictView<'_> {
        DictView::new(&self.mmap[..])
    }
}

impl Dictionary for ImmutableFileDict {
    fn lookup(&self, reading: &str) -> Result<Vec<DictEntry>, DictError> {
        let view = self.view();
        match view.find_key(reading) {
            Some(i) => view.entries_at(i),
            None => Ok(Vec::new()),
        }
    }

    fn prefix_search(&self, prefix: &str) -> Result<Vec<(String, Vec<DictEntry>)>, DictError> {
        let view = self.view();
        let range = view.key_prefix_range(prefix);
        let mut results = Vec::with_capacity(range.len());
        for i in range {
            let key = view.key_at(i).ok_or_else(|| {
                DictError::CorruptFormat(format!("key index {} is out of range", i))
            })?;
            let entries = view.entries_at(i)?;
            results.push((key.to_string(), entries));
        }
        Ok(results)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn mode(&self) -> DictionaryMode {
        DictionaryMode::ReadOnly
    }

    fn roman_bucket(&self, roman_prefix: &str) -> Result<Vec<RomanKey>, DictError> {
        let view = self.view();
        let range = view.roman_prefix_range(roman_prefix);
        let mut results = Vec::with_capacity(range.len());
        for i in range {
            let (roman, key_index) = view.roman_at(i).ok_or_else(|| {
                DictError::CorruptFormat(format!("romaji index {} is out of range", i))
            })?;
            let reading = view.key_at(key_index as usize).ok_or_else(|| {
                DictError::CorruptFormat(format!(
                    "the key index {} a romaji entry points to is out of range",
                    key_index
                ))
            })?;
            results.push(RomanKey {
                reading: reading.to_string(),
                roman: roman.to_string(),
            });
        }
        Ok(results)
    }

    fn symspell_bucket(&self, variant: &str) -> Result<Vec<String>, DictError> {
        let view = self.view();
        let hash = symspell::symspell_hash(variant);
        let mut results = Vec::new();
        for key_index in view.symspell_postings(hash) {
            let reading = view.key_at(key_index).ok_or_else(|| {
                DictError::CorruptFormat(format!(
                    "the key index {} a SymSpell posting points to is out of range",
                    key_index
                ))
            })?;
            results.push(reading.to_string());
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::dict_format::{write_dict, HEADER_LEN, KEY_RECORD_LEN};
    use crate::roman_index::{length_ok, query_prefix};
    use std::collections::BTreeMap;
    use strsim::jaro_winkler;

    /// Helper that writes an immutable-format dictionary file into a temporary
    /// test directory. The analogue of `create_test_db` in `file_dict.rs`.
    fn create_test_dict(dir: &Path, entries: &BTreeMap<String, Vec<DictEntry>>) -> PathBuf {
        let path = dir.join("test.dict");
        write_dict(&path, entries).expect("write_dict failed");
        path
    }

    fn mini_dict_entries() -> BTreeMap<String, Vec<DictEntry>> {
        // Same content as tests/fixtures/mini-dict.skk (the 6 mandatory entries of D-52).
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("にほんご".to_string(), vec![DictEntry::new("日本語")]);
        entries.insert("かんj".to_string(), vec![DictEntry::new("感")]);
        entries.insert("おこなu".to_string(), vec![DictEntry::new("行")]);
        entries.insert(
            ".".to_string(),
            vec![
                DictEntry::new("．"),
                DictEntry::new("・"),
                DictEntry::new("。"),
                DictEntry::new("…"),
            ],
        );
        entries.insert(
            ",".to_string(),
            vec![DictEntry::new("，"), DictEntry::new("、")],
        );
        entries.insert("#ねん".to_string(), vec![DictEntry::new("#3年")]);
        entries
    }

    #[test]
    fn typo_nihogno_reaches_the_nihongo_entry() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());

        // 1. Open the temporary file written by write_dict with ImmutableFileDict::open.
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        // 2. query_prefix returns "nih" for a 7-character query (prefix length 3).
        let prefix = query_prefix("nihogno").expect("failed to get the prefix");
        assert_eq!(prefix, "nih");

        // 3. roman_bucket("nih") contains にほんご/nihongo.
        let bucket = dict.roman_bucket(&prefix).expect("roman_bucket failed");
        assert!(bucket.contains(&RomanKey {
            reading: "にほんご".to_string(),
            roman: "nihongo".to_string(),
        }));

        // 4. length_ok(7, 7) is true and jaro_winkler("nihogno", "nihongo") is >= 0.94
        //    (use the measured value as it is; never write a guess).
        assert!(length_ok("nihogno".len(), "nihongo".len()));
        let sim = jaro_winkler("nihogno", "nihongo");
        assert!(
            sim >= 0.94,
            "jaro_winkler(\"nihogno\", \"nihongo\") = {} is below 0.94",
            sim
        );

        // 5. lookup("にほんご") returns a DictEntry containing 日本語.
        let looked_up = dict.lookup("にほんご").expect("lookup failed");
        assert!(looked_up.iter().any(|e| e.word == "日本語"));
    }

    /// D-91/92/94: `symspell_bucket` binary-searches the hash table through the
    /// mmap and returns the original kana keys that share a delete variant.
    /// 「こんちは」 is a delete variant of 「こんにちは」 (with 「に」 deleted), so
    /// the lookup hits.
    #[test]
    fn symspell_bucket_finds_konnichiha_from_konchiha() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("こんにちは".to_string(), vec![DictEntry::new("今日は")]);
        let path = create_test_dict(tmp.path(), &entries);
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let bucket = dict
            .symspell_bucket("こんちは")
            .expect("symspell_bucket failed");
        assert!(
            bucket.contains(&"こんにちは".to_string()),
            "the bucket of 「こんちは」 should contain 「こんにちは」: {:?}",
            bucket
        );
    }

    /// Looking up a variant that is not indexed returns nothing (not found means
    /// empty, per the D-91 specification).
    #[test]
    fn symspell_bucket_returns_empty_for_an_unknown_variant() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let bucket = dict
            .symspell_bucket("ぜんぜんちがうもじれつ")
            .expect("symspell_bucket failed");
        assert!(bucket.is_empty());
    }

    #[test]
    fn a_missing_path_returns_notfound_and_creates_no_file_or_directory() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let missing_path = tmp.path().join("does-not-exist.dict");

        let result = ImmutableFileDict::open(&missing_path);
        assert!(matches!(result, Err(DictError::NotFound(_))));
        assert!(!missing_path.exists());
    }

    #[test]
    fn a_broken_magic_yields_corruptformat() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());
        let mut bytes = std::fs::read(&path).expect("failed to read the file");
        bytes[0] = b'X';
        std::fs::write(&path, &bytes).expect("failed to write the file back");

        let result = ImmutableFileDict::open(&path);
        assert!(matches!(result, Err(DictError::CorruptFormat(_))));
    }

    #[test]
    fn a_version_mismatch_yields_corruptformat() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());
        let mut bytes = std::fs::read(&path).expect("failed to read the file");
        let bad_version = (dict_format::FORMAT_VERSION + 1).to_le_bytes();
        bytes[8..12].copy_from_slice(&bad_version);
        std::fs::write(&path, &bytes).expect("failed to write the file back");

        let result = ImmutableFileDict::open(&path);
        assert!(matches!(result, Err(DictError::CorruptFormat(_))));
    }

    #[test]
    fn a_header_crc_mismatch_yields_corruptformat() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());
        let mut bytes = std::fs::read(&path).expect("failed to read the file");
        let bad_crc = 0xDEAD_BEEFu32.to_le_bytes();
        // OFF_HEADER_CRC32 = 44 (the v2 format extended to HEADER_LEN=48, 03.1-01).
        bytes[44..48].copy_from_slice(&bad_crc);
        std::fs::write(&path, &bytes).expect("failed to write the file back");

        let result = ImmutableFileDict::open(&path);
        assert!(matches!(result, Err(DictError::CorruptFormat(_))));
    }

    #[test]
    fn a_truncated_file_yields_corruptformat() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());
        let bytes = std::fs::read(&path).expect("failed to read the file");
        // Truncate in the middle of the kana key table (partway through the 6
        // records). There is no need to cut below key_table_offset plus one
        // record; cutting clearly short of the end of all tables is enough.
        let cut = HEADER_LEN + KEY_RECORD_LEN; // long enough for 1 of the 6 entries
        assert!(
            cut < bytes.len(),
            "test precondition: the original file is longer than cut"
        );
        std::fs::write(&path, &bytes[0..cut]).expect("failed to write the file back");

        let result = ImmutableFileDict::open(&path);
        assert!(matches!(result, Err(DictError::CorruptFormat(_))));
    }

    #[test]
    fn the_dictionary_mode_is_read_only() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");
        assert_eq!(dict.mode(), DictionaryMode::ReadOnly);
    }

    #[test]
    fn save_on_immutablefiledict_does_nothing_and_returns_ok() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = create_test_dict(tmp.path(), &mini_dict_entries());
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let before = std::fs::metadata(&path).expect("failed to get the metadata");
        let before_len = before.len();
        let before_mtime = before.modified().expect("failed to get the mtime");

        assert!(dict.save().is_ok());

        let after = std::fs::metadata(&path).expect("failed to get the metadata");
        assert_eq!(
            before_len,
            after.len(),
            "save should not change the file length (the default implementation does nothing)"
        );
        assert_eq!(
            before_mtime,
            after.modified().expect("failed to get the mtime"),
            "save should not change the modification time (the default implementation does nothing)"
        );
    }
}
