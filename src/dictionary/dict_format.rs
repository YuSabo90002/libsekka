// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! On-disk layout of the immutable master dictionary (sorted tables; decided at
//! the 02.1-01 checkpoint)
//!
//! A single read-only file, mmapped, keys sorted so they can be binary-searched
//! (D-61). A romaji prefix index is bundled in, so at runtime we only read a
//! bucket (D-59). There is no per-prefix-length bucket table: as long as the
//! romaji key table is sorted by byte order, the entries sharing a prefix always
//! form a contiguous range, so a single binary search yields the range for a
//! 2-, 3- or 4-character prefix alike.
//!
//! `FORMAT_VERSION = 2` (03.1-01, D-91 to D-93) bundles in the SymSpell
//! delete-variant index. The index is baked at build time and only read through
//! the mmap at runtime (D-91). The new table stores hashes only, never the
//! variant strings (D-92). A dictionary file whose `format_version` is not 2 is
//! rejected with `CorruptFormat` (D-93; there is no backward-compatible read
//! path).
//!
//! File layout (everything little-endian, fixed-length records):
//! - Header, fixed 48 bytes (`HEADER_LEN`): magic `SEKKADI1` (8 bytes),
//!   `format_version` u32, `entry_count` u32, `key_table_offset` u32,
//!   `roman_table_offset` u32, `string_region_offset` u32,
//!   `blob_region_offset` u32, `roman_count` u32, `symspell_table_offset` u32,
//!   `symspell_count` u32, `header_crc32` (CRC-32 of `[0..44)`, IEEE 802.3) u32
//! - Kana key table: `entry_count` fixed-length 16-byte records
//!   `{key_off, key_len, blob_off, blob_len}` (all absolute file offsets),
//!   sorted by the UTF-8 byte order of the kana key (this is what gets
//!   binary-searched)
//! - Romaji key table: `roman_count` fixed-length 12-byte records
//!   `{roman_off, roman_len, key_index}`, sorted by the byte order of the
//!   canonical Hepburn romaji. `key_index` refers into the kana key table.
//!   Several records may share the same romaji string (D-75: every collision
//!   between homophonous romaji is kept)
//! - SymSpell hash table (CSR, new in 03.1-01): `symspell_count + 1`
//!   fixed-length 12-byte records `{hash: u64, posting_offset: u32}`, sorted by
//!   ascending hash, with one sentinel row at the end (`hash = u64::MAX`, and
//!   `posting_offset` equal to the total number of postings). The end of a range
//!   is given by the next row's `posting_offset` (there is no `posting_count`
//!   field; this follows the existing style where string_region/blob_region
//!   derive their length implicitly from the next offset)
//! - SymSpell posting array (new in 03.1-01): an array of fixed-length `u32`
//!   elements, each an index into the kana key table, placed immediately after
//!   the hash table
//! - String and value regions: the raw UTF-8 bytes of the kana and romaji keys
//!   (the string region) and the `serde_json::to_vec(&Vec<DictEntry>)` value
//!   blobs (the blob region)
//!
//! This module contains no `unsafe` block at all. Every read is bounds-checked
//! through `slice::get(range)`; no raw pointer arithmetic and no type-punning
//! casts (the mitigation for T-02.1-01). Only creating the mmap is `unsafe`, and
//! that is the responsibility of `immutable_dict.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use crate::dictionary::{DictEntry, DictError};
use crate::kana_romaji::kana_to_hepburn;
use crate::roman_index::MIN_INDEX_ROMAN_LEN;
use crate::symspell;

/// Magic bytes; they double as the format identifier and an implicit version 1.
pub const MAGIC: &[u8; 8] = b"SEKKADI1";
/// Current format version. Raise it when the serialization of value blobs changes.
pub const FORMAT_VERSION: u32 = 2;
/// Fixed header length in bytes.
pub const HEADER_LEN: usize = 48;
/// Length of one kana key table record, in bytes.
pub const KEY_RECORD_LEN: usize = 16;
/// Length of one romaji key table record, in bytes.
pub const ROMAN_RECORD_LEN: usize = 12;
/// Length of one SymSpell hash table record, in bytes (`{hash: u64, posting_offset: u32}`).
pub const SYMSPELL_ROW_LEN: usize = 12;

// --- Offsets inside the header ---
const OFF_MAGIC: usize = 0;
const OFF_FORMAT_VERSION: usize = 8;
const OFF_ENTRY_COUNT: usize = 12;
const OFF_KEY_TABLE_OFFSET: usize = 16;
const OFF_ROMAN_TABLE_OFFSET: usize = 20;
const OFF_STRING_REGION_OFFSET: usize = 24;
const OFF_BLOB_REGION_OFFSET: usize = 28;
const OFF_ROMAN_COUNT: usize = 32;
const OFF_SYMSPELL_TABLE_OFFSET: usize = 36;
const OFF_SYMSPELL_COUNT: usize = 40;
const OFF_HEADER_CRC32: usize = 44;
/// The range the CRC32 covers (`[0..HEADER_CRC32_COVERED_LEN)`).
const HEADER_CRC32_COVERED_LEN: usize = 44;

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

/// CRC-32 (IEEE 802.3, reflected polynomial `0xEDB88320`). Known answer: `crc32(b"123456789") == 0xCBF43926`.
pub fn crc32(data: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB8_8320;
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ POLY;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Validates the header. Returns `DictError::CorruptFormat` when the data is
/// too short, the magic does not match, the version does not match, the header
/// CRC32 does not match, or any table would end past the end of the file. The
/// caller (`immutable_dict.rs`) must not use any offset value before this
/// validation passes.
pub fn validate(bytes: &[u8]) -> Result<(), DictError> {
    if bytes.len() < HEADER_LEN {
        return Err(DictError::CorruptFormat(format!(
            "header too short: {} bytes ({} required)",
            bytes.len(),
            HEADER_LEN
        )));
    }
    if &bytes[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC.as_slice() {
        return Err(DictError::CorruptFormat(
            "magic bytes do not match".to_string(),
        ));
    }
    let format_version = read_u32(bytes, OFF_FORMAT_VERSION)
        .ok_or_else(|| DictError::CorruptFormat("cannot read format_version".to_string()))?;
    if format_version != FORMAT_VERSION {
        return Err(DictError::CorruptFormat(format!(
            "format version mismatch: {} (expected {})",
            format_version, FORMAT_VERSION
        )));
    }
    let header_crc32 = read_u32(bytes, OFF_HEADER_CRC32)
        .ok_or_else(|| DictError::CorruptFormat("cannot read header_crc32".to_string()))?;
    let computed = crc32(&bytes[0..HEADER_CRC32_COVERED_LEN]);
    if header_crc32 != computed {
        return Err(DictError::CorruptFormat(
            "header CRC32 does not match".to_string(),
        ));
    }

    let entry_count = read_u32(bytes, OFF_ENTRY_COUNT)
        .ok_or_else(|| DictError::CorruptFormat("cannot read entry_count".to_string()))?
        as u64;
    let key_table_offset = read_u32(bytes, OFF_KEY_TABLE_OFFSET)
        .ok_or_else(|| DictError::CorruptFormat("cannot read key_table_offset".to_string()))?
        as u64;
    let roman_table_offset = read_u32(bytes, OFF_ROMAN_TABLE_OFFSET)
        .ok_or_else(|| DictError::CorruptFormat("cannot read roman_table_offset".to_string()))?
        as u64;
    let string_region_offset = read_u32(bytes, OFF_STRING_REGION_OFFSET)
        .ok_or_else(|| DictError::CorruptFormat("cannot read string_region_offset".to_string()))?
        as u64;
    let blob_region_offset = read_u32(bytes, OFF_BLOB_REGION_OFFSET)
        .ok_or_else(|| DictError::CorruptFormat("cannot read blob_region_offset".to_string()))?
        as u64;
    let roman_count = read_u32(bytes, OFF_ROMAN_COUNT)
        .ok_or_else(|| DictError::CorruptFormat("cannot read roman_count".to_string()))?
        as u64;
    let symspell_table_offset = read_u32(bytes, OFF_SYMSPELL_TABLE_OFFSET)
        .ok_or_else(|| DictError::CorruptFormat("cannot read symspell_table_offset".to_string()))?
        as u64;
    let symspell_count = read_u32(bytes, OFF_SYMSPELL_COUNT)
        .ok_or_else(|| DictError::CorruptFormat("cannot read symspell_count".to_string()))?
        as u64;

    let file_len = bytes.len() as u64;
    for (name, offset) in [
        ("key_table_offset", key_table_offset),
        ("roman_table_offset", roman_table_offset),
        ("string_region_offset", string_region_offset),
        ("blob_region_offset", blob_region_offset),
        ("symspell_table_offset", symspell_table_offset),
    ] {
        if offset > file_len {
            return Err(DictError::CorruptFormat(format!(
                "{} extends past the end of the file: offset={} len={}",
                name, offset, file_len
            )));
        }
    }

    let key_table_end = key_table_offset + entry_count * (KEY_RECORD_LEN as u64);
    if key_table_end > file_len {
        return Err(DictError::CorruptFormat(format!(
            "the kana key table extends past the end of the file: end={} len={}",
            key_table_end, file_len
        )));
    }
    let roman_table_end = roman_table_offset + roman_count * (ROMAN_RECORD_LEN as u64);
    if roman_table_end > file_len {
        return Err(DictError::CorruptFormat(format!(
            "the romaji key table extends past the end of the file: end={} len={}",
            roman_table_end, file_len
        )));
    }
    // 03.1-01: end-of-table check for the SymSpell hash table (T-03.1-01-01).
    // That is symspell_count + 1 rows including the sentinel. The end of the
    // posting array itself is not validated here (same style as the existing
    // string_region/blob_region: the read side bounds-checks it lazily,
    // T-03.1-01-03).
    let symspell_table_len = symspell_count
        .checked_add(1)
        .and_then(|n| n.checked_mul(SYMSPELL_ROW_LEN as u64))
        .ok_or_else(|| {
            DictError::CorruptFormat("computing the symspell table length overflowed".to_string())
        })?;
    let symspell_table_end = symspell_table_offset
        .checked_add(symspell_table_len)
        .ok_or_else(|| {
            DictError::CorruptFormat("computing symspell_table_end overflowed".to_string())
        })?;
    if symspell_table_end > file_len {
        return Err(DictError::CorruptFormat(format!(
            "the SymSpell hash table extends past the end of the file: end={} len={}",
            symspell_table_end, file_len
        )));
    }

    Ok(())
}

/// Decides, from the build-time intermediate representation `write_dict`
/// receives, whether one kana key is eligible for the romaji index (the
/// precondition of D-74).
///
/// Only keys satisfying both (a) a canonical romaji length of at least
/// `MIN_INDEX_ROMAN_LEN` and (b) a last character that is not an ASCII letter go
/// into the romaji table. The okuri-ari conventional keys of SKK-JISYO.L
/// (`かんj` / `おこなu`) are excluded by (b); indexing them would let the
/// okuri-nashi query `kanji` pick up `kanj` and drag in unrelated okuri-ari
/// candidates (a recurrence of G-01.1-6b).
fn is_roman_index_eligible(kana_key: &str, roman: &str) -> bool {
    let roman_len_ok = roman.chars().count() >= MIN_INDEX_ROMAN_LEN;
    let last_char_is_ascii_alpha = kana_key
        .chars()
        .next_back()
        .map(|c| c.is_ascii_alphabetic())
        .unwrap_or(false);
    roman_len_ok && !last_char_is_ascii_alpha
}

/// Converts a `usize` length or count into `u32` with a check. Every offset and
/// length computation in `write_dict` goes through this function plus
/// `checked_add`/`checked_mul` (02.1-REVIEW WR-01: in release builds `as u32`
/// does not panic but silently wraps, so this closes the asymmetry with the read
/// side of `DictView`, which is rigorous about `checked_add`/`checked_mul`, and
/// returns an error past the 4 GiB boundary instead of silently writing a file
/// with wrapped, invalid offsets).
fn u32_from_len(n: usize, what: &str) -> Result<u32, DictError> {
    u32::try_from(n).map_err(|_| {
        DictError::SerializationError(format!("{what} is too large (over the 4 GiB limit)"))
    })
}

/// Writes the immutable master dictionary. Kana keys go into the key table in
/// the order of `entries` (a `BTreeMap`, i.e. ascending UTF-8 byte order). The
/// data is written to a temporary path and then `rename`d (an atomic replace).
pub fn write_dict(
    path: &Path,
    entries: &BTreeMap<String, Vec<DictEntry>>,
) -> Result<(), DictError> {
    let entry_count = u32_from_len(entries.len(), "entry count")?;

    let mut key_strings: Vec<u8> = Vec::new();
    let mut blob_bytes: Vec<u8> = Vec::new();
    // (key_off, key_len, blob_off, blob_len) - offsets relative to key_strings/blob_bytes
    let mut key_rows: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(entries.len());

    struct RomanRow {
        roman: String,
        key_index: u32,
    }
    let mut roman_rows: Vec<RomanRow> = Vec::new();

    // 03.1-01: the SymSpell index (D-91/92/94). No exclusion rules - okuri-ari
    // conventional keys, prefix/suffix keys and single-character keys all go in.
    // Deduplicated by (hash, key_index) pairs (matching the duplicate-key guard
    // in upstream sekka-symspell--index-key).
    let mut variant_pairs: BTreeSet<(u64, u32)> = BTreeSet::new();

    for (idx, (kana_key, dict_entries)) in entries.iter().enumerate() {
        let key_off = u32_from_len(key_strings.len(), "kana string region offset")?;
        key_strings.extend_from_slice(kana_key.as_bytes());
        let key_len = u32_from_len(kana_key.len(), "kana key length")?;

        let blob = serde_json::to_vec(dict_entries)?;
        let blob_off = u32_from_len(blob_bytes.len(), "value region offset")?;
        blob_bytes.extend_from_slice(&blob);
        let blob_len = u32_from_len(blob.len(), "value byte length")?;

        key_rows.push((key_off, key_len, blob_off, blob_len));

        let key_index = u32_from_len(idx, "key index referenced by romaji/SymSpell")?;

        let roman = kana_to_hepburn(kana_key);
        if is_roman_index_eligible(kana_key, &roman) {
            roman_rows.push(RomanRow { roman, key_index });
        }

        for variant in symspell::delete_variants(kana_key) {
            let hash = symspell::symspell_hash(&variant);
            variant_pairs.insert((hash, key_index));
        }
    }

    // Stable sort by the byte order of the romaji. Ties keep the ascending kana
    // key order (the rows were pushed while iterating the BTreeMap, i.e. in
    // ascending kana key order, and the sort is stable, so several keys sharing
    // one romaji naturally stay in ascending kana key order - the stable order
    // recommended by Open Question 2 of 02.1-RESEARCH.md).
    roman_rows.sort_by(|a, b| a.roman.as_bytes().cmp(b.roman.as_bytes()));

    let mut roman_strings: Vec<u8> = Vec::new();
    // (roman_off, roman_len, key_index) - offsets relative to roman_strings
    let mut roman_table_rows: Vec<(u32, u32, u32)> = Vec::with_capacity(roman_rows.len());
    for row in &roman_rows {
        let roman_off = u32_from_len(roman_strings.len(), "romaji string region offset")?;
        roman_strings.extend_from_slice(row.roman.as_bytes());
        let roman_len = u32_from_len(row.roman.len(), "romaji key length")?;
        roman_table_rows.push((roman_off, roman_len, row.key_index));
    }

    // 03.1-01: fold into CSR form. variant_pairs is a BTreeSet, so it iterates in
    // ascending (hash, key_index) order (ascending hash, and within one hash
    // ascending key index).
    let mut symspell_table_rows: Vec<(u64, u32)> = Vec::new(); // (hash, posting_offset)
    let mut postings: Vec<u32> = Vec::new();
    let mut last_hash: Option<u64> = None;
    for (hash, key_index) in &variant_pairs {
        if last_hash != Some(*hash) {
            let posting_offset = u32_from_len(postings.len(), "posting offset")?;
            symspell_table_rows.push((*hash, posting_offset));
            last_hash = Some(*hash);
        }
        postings.push(*key_index);
    }
    // Sentinel row: hash = u64::MAX, posting_offset = the total number of postings.
    let postings_total = u32_from_len(postings.len(), "total number of postings")?;
    symspell_table_rows.push((u64::MAX, postings_total));
    let symspell_count = u32_from_len(symspell_table_rows.len() - 1, "SymSpell entry count")?;

    let key_strings_len = u32_from_len(key_strings.len(), "kana string region length")?;
    // string_region = key_strings ++ roman_strings, so add key_strings_len to the
    // roman_strings-relative offsets to make them relative to string_region.
    for row in roman_table_rows.iter_mut() {
        row.0 = row.0.checked_add(key_strings_len).ok_or_else(|| {
            DictError::SerializationError(
                "computing the string region offset overflowed".to_string(),
            )
        })?;
    }

    let mut string_region = key_strings;
    string_region.extend_from_slice(&roman_strings);

    let roman_count = u32_from_len(roman_table_rows.len(), "romaji entry count")?;

    let key_table_offset = u32_from_len(HEADER_LEN, "header length")?;
    let key_table_len = entry_count
        .checked_mul(KEY_RECORD_LEN as u32)
        .ok_or_else(|| {
            DictError::SerializationError("the kana key table is too large".to_string())
        })?;
    let roman_table_offset = key_table_offset.checked_add(key_table_len).ok_or_else(|| {
        DictError::SerializationError(
            "computing the romaji key table offset overflowed".to_string(),
        )
    })?;
    let roman_table_len = roman_count
        .checked_mul(ROMAN_RECORD_LEN as u32)
        .ok_or_else(|| {
            DictError::SerializationError("the romaji key table is too large".to_string())
        })?;
    // 03.1-01: the SymSpell hash table sits right after the romaji key table and before the string region.
    let symspell_table_offset =
        roman_table_offset
            .checked_add(roman_table_len)
            .ok_or_else(|| {
                DictError::SerializationError(
                    "computing the SymSpell hash table offset overflowed".to_string(),
                )
            })?;
    let symspell_row_count = u32_from_len(
        symspell_table_rows.len(),
        "SymSpell row count (including the sentinel)",
    )?;
    let symspell_table_len = symspell_row_count
        .checked_mul(SYMSPELL_ROW_LEN as u32)
        .ok_or_else(|| {
            DictError::SerializationError("the SymSpell hash table is too large".to_string())
        })?;
    let postings_offset = symspell_table_offset
        .checked_add(symspell_table_len)
        .ok_or_else(|| {
            DictError::SerializationError(
                "computing the SymSpell posting array offset overflowed".to_string(),
            )
        })?;
    let postings_len = postings_total.checked_mul(4).ok_or_else(|| {
        DictError::SerializationError("the SymSpell posting array is too large".to_string())
    })?;
    let string_region_offset = postings_offset.checked_add(postings_len).ok_or_else(|| {
        DictError::SerializationError("computing the string region offset overflowed".to_string())
    })?;
    let string_region_len = u32_from_len(string_region.len(), "string region length")?;
    let blob_region_offset = string_region_offset
        .checked_add(string_region_len)
        .ok_or_else(|| {
            DictError::SerializationError(
                "computing the value region offset overflowed".to_string(),
            )
        })?;

    let mut buf: Vec<u8> = Vec::with_capacity(
        HEADER_LEN
            + string_region.len()
            + blob_bytes.len()
            + key_rows.len() * KEY_RECORD_LEN
            + roman_table_rows.len() * ROMAN_RECORD_LEN
            + symspell_table_rows.len() * SYMSPELL_ROW_LEN
            + postings.len() * 4,
    );

    // --- Header ---
    buf.extend_from_slice(MAGIC.as_slice());
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&entry_count.to_le_bytes());
    buf.extend_from_slice(&key_table_offset.to_le_bytes());
    buf.extend_from_slice(&roman_table_offset.to_le_bytes());
    buf.extend_from_slice(&string_region_offset.to_le_bytes());
    buf.extend_from_slice(&blob_region_offset.to_le_bytes());
    buf.extend_from_slice(&roman_count.to_le_bytes());
    buf.extend_from_slice(&symspell_table_offset.to_le_bytes());
    buf.extend_from_slice(&symspell_count.to_le_bytes());
    let crc_pos = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // header_crc32 placeholder
    debug_assert_eq!(buf.len(), HEADER_LEN);

    // --- Kana key table (written as absolute offsets) ---
    for (key_off, key_len, blob_off, blob_len) in &key_rows {
        let abs_key_off = string_region_offset.checked_add(*key_off).ok_or_else(|| {
            DictError::SerializationError(
                "computing the absolute offset of a kana key overflowed".to_string(),
            )
        })?;
        buf.extend_from_slice(&abs_key_off.to_le_bytes());
        buf.extend_from_slice(&key_len.to_le_bytes());
        let abs_blob_off = blob_region_offset.checked_add(*blob_off).ok_or_else(|| {
            DictError::SerializationError(
                "computing the absolute offset of a value overflowed".to_string(),
            )
        })?;
        buf.extend_from_slice(&abs_blob_off.to_le_bytes());
        buf.extend_from_slice(&blob_len.to_le_bytes());
    }

    // --- Romaji key table (written as absolute offsets) ---
    for (roman_off, roman_len, key_index) in &roman_table_rows {
        let abs_roman_off = string_region_offset
            .checked_add(*roman_off)
            .ok_or_else(|| {
                DictError::SerializationError(
                    "computing the absolute offset of a romaji key overflowed".to_string(),
                )
            })?;
        buf.extend_from_slice(&abs_roman_off.to_le_bytes());
        buf.extend_from_slice(&roman_len.to_le_bytes());
        buf.extend_from_slice(&key_index.to_le_bytes());
    }

    // --- SymSpell hash table (CSR; posting_offset is a relative index into the
    //     posting array, not an absolute byte offset, so no conversion) ---
    for (hash, posting_offset) in &symspell_table_rows {
        buf.extend_from_slice(&hash.to_le_bytes());
        buf.extend_from_slice(&posting_offset.to_le_bytes());
    }

    // --- SymSpell posting array ---
    for key_index in &postings {
        buf.extend_from_slice(&key_index.to_le_bytes());
    }

    // --- String and value regions ---
    buf.extend_from_slice(&string_region);
    buf.extend_from_slice(&blob_bytes);

    let crc = crc32(&buf[0..HEADER_CRC32_COVERED_LEN]);
    buf[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());

    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, &buf)?;
    std::fs::rename(&tmp_path, path)?;

    Ok(())
}

/// Read view that interprets an mmapped byte slice (or an ordinary `Vec<u8>`
/// slice). This module contains no `unsafe`; every read goes through `slice::get`.
pub struct DictView<'a> {
    bytes: &'a [u8],
}

impl<'a> DictView<'a> {
    /// Creates a view over a byte slice that passed `validate`. The view itself
    /// does not re-validate (the contract is that the caller runs `validate`
    /// right after `open`, T-02.1-01).
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn u32_at(&self, offset: usize) -> Option<u32> {
        read_u32(self.bytes, offset)
    }

    /// Number of kana keys.
    pub fn entry_count(&self) -> usize {
        self.u32_at(OFF_ENTRY_COUNT).unwrap_or(0) as usize
    }

    fn key_table_offset(&self) -> usize {
        self.u32_at(OFF_KEY_TABLE_OFFSET).unwrap_or(0) as usize
    }

    fn roman_table_offset(&self) -> usize {
        self.u32_at(OFF_ROMAN_TABLE_OFFSET).unwrap_or(0) as usize
    }

    /// Number of romaji keys.
    pub fn roman_count(&self) -> usize {
        self.u32_at(OFF_ROMAN_COUNT).unwrap_or(0) as usize
    }

    fn symspell_table_offset(&self) -> usize {
        self.u32_at(OFF_SYMSPELL_TABLE_OFFSET).unwrap_or(0) as usize
    }

    /// Number of unique hashes (delete variants) in the SymSpell index, excluding the sentinel row.
    pub fn symspell_count(&self) -> usize {
        self.u32_at(OFF_SYMSPELL_COUNT).unwrap_or(0) as usize
    }

    fn key_record(&self, i: usize) -> Option<(u32, u32, u32, u32)> {
        let off = self
            .key_table_offset()
            .checked_add(i.checked_mul(KEY_RECORD_LEN)?)?;
        let rec = self.bytes.get(off..off + KEY_RECORD_LEN)?;
        let key_off = u32::from_le_bytes(rec[0..4].try_into().ok()?);
        let key_len = u32::from_le_bytes(rec[4..8].try_into().ok()?);
        let blob_off = u32::from_le_bytes(rec[8..12].try_into().ok()?);
        let blob_len = u32::from_le_bytes(rec[12..16].try_into().ok()?);
        Some((key_off, key_len, blob_off, blob_len))
    }

    fn roman_record(&self, i: usize) -> Option<(u32, u32, u32)> {
        let off = self
            .roman_table_offset()
            .checked_add(i.checked_mul(ROMAN_RECORD_LEN)?)?;
        let rec = self.bytes.get(off..off + ROMAN_RECORD_LEN)?;
        let roman_off = u32::from_le_bytes(rec[0..4].try_into().ok()?);
        let roman_len = u32::from_le_bytes(rec[4..8].try_into().ok()?);
        let key_index = u32::from_le_bytes(rec[8..12].try_into().ok()?);
        Some((roman_off, roman_len, key_index))
    }

    /// Reads row `i` of the SymSpell hash table (`0..=symspell_count`, including the sentinel).
    fn symspell_row(&self, i: usize) -> Option<(u64, u32)> {
        let off = self
            .symspell_table_offset()
            .checked_add(i.checked_mul(SYMSPELL_ROW_LEN)?)?;
        let rec = self.bytes.get(off..off + SYMSPELL_ROW_LEN)?;
        let hash = u64::from_le_bytes(rec[0..8].try_into().ok()?);
        let posting_offset = u32::from_le_bytes(rec[8..12].try_into().ok()?);
        Some((hash, posting_offset))
    }

    /// Binary-searches the SymSpell hash table and returns the index of the
    /// exactly matching row (data rows only, `0..symspell_count`; the sentinel
    /// row is not searched).
    fn find_symspell_row(&self, hash: u64) -> Option<usize> {
        let n = self.symspell_count();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (h, _) = self.symspell_row(mid)?;
            match h.cmp(&hash) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Absolute byte offset of the SymSpell posting array (right after the hash
    /// table). `None` on overflow.
    fn symspell_postings_array_offset(&self) -> Option<usize> {
        let table_off = self.symspell_table_offset();
        let n = self.symspell_count();
        let rows_len = n.checked_add(1)?.checked_mul(SYMSPELL_ROW_LEN)?;
        table_off.checked_add(rows_len)
    }

    /// Returns the indices into the kana key table for the SymSpell delete
    /// variant with hash `hash` (empty when not found). Reads of the posting
    /// array are bounds-checked with `slice::get` and stop as soon as they would
    /// go out of range (`validate` only checks the end of the hash table, so the
    /// posting array is bounds-checked lazily, T-03.1-01-03).
    pub fn symspell_postings(&self, hash: u64) -> Vec<usize> {
        let Some(i) = self.find_symspell_row(hash) else {
            return Vec::new();
        };
        let Some((_, start)) = self.symspell_row(i) else {
            return Vec::new();
        };
        let Some((_, end)) = self.symspell_row(i + 1) else {
            return Vec::new();
        };
        let Some(postings_offset) = self.symspell_postings_array_offset() else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for idx in start..end {
            let Some(byte_off) = (idx as usize)
                .checked_mul(4)
                .and_then(|rel| postings_offset.checked_add(rel))
            else {
                break;
            };
            let Some(bytes) = self.bytes.get(byte_off..byte_off + 4) else {
                break;
            };
            let key_index = match <[u8; 4]>::try_from(bytes) {
                Ok(arr) => u32::from_le_bytes(arr) as usize,
                Err(_) => break,
            };
            result.push(key_index);
        }
        result
    }

    /// Returns the kana key string at index `i`. `None` when out of range or not valid UTF-8.
    pub fn key_at(&self, i: usize) -> Option<&'a str> {
        let (key_off, key_len, _, _) = self.key_record(i)?;
        let start = key_off as usize;
        let end = start.checked_add(key_len as usize)?;
        let bytes = self.bytes.get(start..end)?;
        std::str::from_utf8(bytes).ok()
    }

    /// Returns the value blob at index `i` (the `serde_json`-serialized bytes).
    pub fn blob_at(&self, i: usize) -> Option<&'a [u8]> {
        let (_, _, blob_off, blob_len) = self.key_record(i)?;
        let start = blob_off as usize;
        let end = start.checked_add(blob_len as usize)?;
        self.bytes.get(start..end)
    }

    /// Deserializes the value blob at index `i` into `Vec<DictEntry>`.
    pub fn entries_at(&self, i: usize) -> Result<Vec<DictEntry>, DictError> {
        let blob = self.blob_at(i).ok_or_else(|| {
            DictError::CorruptFormat(format!("the blob of key index {} is out of range", i))
        })?;
        let entries: Vec<DictEntry> = serde_json::from_slice(blob)?;
        Ok(entries)
    }

    /// Returns the romaji key at index `i` and the kana key table index it points to.
    pub fn roman_at(&self, i: usize) -> Option<(&'a str, u32)> {
        let (roman_off, roman_len, key_index) = self.roman_record(i)?;
        let start = roman_off as usize;
        let end = start.checked_add(roman_len as usize)?;
        let bytes = self.bytes.get(start..end)?;
        let s = std::str::from_utf8(bytes).ok()?;
        Some((s, key_index))
    }

    /// Binary-searches the kana key table and returns the exactly matching index.
    pub fn find_key(&self, key: &str) -> Option<usize> {
        let n = self.entry_count();
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let mid_key = self.key_at(mid)?;
            match mid_key.cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// Binary-searches for the end of the range where the predicate `pred` holds
    /// (the first index where it becomes false). `pred` is assumed to be
    /// monotonic - true then false - over the sorted array.
    fn partition_point_keys<F: Fn(&str) -> bool>(&self, n: usize, pred: F) -> usize {
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let k = self.key_at(mid).unwrap_or("");
            if pred(k) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn partition_point_romans<F: Fn(&str) -> bool>(&self, n: usize, pred: F) -> usize {
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = self.roman_at(mid).map(|(s, _)| s).unwrap_or("");
            if pred(r) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Returns the contiguous prefix-matching range of the kana key table (a
    /// binary search that relies on the kana key table being sorted by key; two
    /// binary searches give the lower and upper bounds).
    pub fn key_prefix_range(&self, prefix: &str) -> Range<usize> {
        let n = self.entry_count();
        let lo = self.partition_point_keys(n, |k| k < prefix);
        let hi = self.partition_point_keys(n, |k| k < prefix || k.starts_with(prefix));
        lo..hi
    }

    /// Returns the contiguous prefix-matching range of the romaji key table
    /// (D-59: no per-prefix-length bucket table; the range is read as a
    /// contiguous run in ascending byte order).
    pub fn roman_prefix_range(&self, prefix: &str) -> Range<usize> {
        let n = self.roman_count();
        let lo = self.partition_point_romans(n, |r| r < prefix);
        let hi = self.partition_point_romans(n, |r| r < prefix || r.starts_with(prefix));
        lo..hi
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_and_read(
        entries: &BTreeMap<String, Vec<DictEntry>>,
    ) -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let path = tmp.path().join("test.dict");
        write_dict(&path, entries).expect("write_dict failed");
        let bytes = std::fs::read(&path).expect("failed to read the file");
        (tmp, path, bytes)
    }

    #[test]
    fn crc32_known_answer_is_pinned() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn an_empty_dictionary_can_be_written_and_opened() {
        let entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        let (_tmp, _path, bytes) = write_and_read(&entries);
        validate(&bytes).expect("validate failed on an empty dictionary");
        let view = DictView::new(&bytes);
        assert_eq!(view.entry_count(), 0);
        assert_eq!(view.roman_count(), 0);
        assert_eq!(view.symspell_count(), 0);
    }

    #[test]
    fn a_single_entry_dictionary_can_be_written_and_opened() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("にほんご".to_string(), vec![DictEntry::new("日本語")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        validate(&bytes).expect("validate failed");
        let view = DictView::new(&bytes);
        assert_eq!(view.entry_count(), 1);
        assert_eq!(view.key_at(0), Some("にほんご"));
        let restored = view.entries_at(0).expect("entries_at failed");
        assert_eq!(restored[0].word, "日本語");
    }

    #[test]
    fn kani_and_kani_both_appear_as_kani_in_the_romaji_table() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かんい".to_string(), vec![DictEntry::new("感為")]);
        entries.insert("かに".to_string(), vec![DictEntry::new("蟹")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        let view = DictView::new(&bytes);
        assert_eq!(view.roman_count(), 2);
        let romans: Vec<&str> = (0..view.roman_count())
            .map(|i| view.roman_at(i).unwrap().0)
            .collect();
        assert_eq!(romans, vec!["kani", "kani"]);
    }

    #[test]
    fn keys_whose_canonical_romaji_is_shorter_than_2_are_not_in_the_romaji_table() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("あ".to_string(), vec![DictEntry::new("亜")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        let view = DictView::new(&bytes);
        assert_eq!(view.roman_count(), 0);
    }

    #[test]
    fn keys_ending_in_an_ascii_letter_are_not_in_the_romaji_table() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かんj".to_string(), vec![DictEntry::new("感")]);
        entries.insert("おこなu".to_string(), vec![DictEntry::new("行")]);
        entries.insert("にほんご".to_string(), vec![DictEntry::new("日本語")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        let view = DictView::new(&bytes);
        assert_eq!(view.roman_count(), 1);
        assert_eq!(view.roman_at(0).unwrap().0, "nihongo");
    }

    #[test]
    fn roman_prefix_range_returns_the_expected_contiguous_range() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("にほんご".to_string(), vec![DictEntry::new("日本語")]);
        entries.insert("にっき".to_string(), vec![DictEntry::new("日記")]);
        entries.insert("かんじ".to_string(), vec![DictEntry::new("漢字")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        let view = DictView::new(&bytes);
        let range = view.roman_prefix_range("ni");
        let romans: Vec<&str> = range.map(|i| view.roman_at(i).unwrap().0).collect();
        assert_eq!(romans, vec!["nihongo", "nikki"]);
    }

    #[test]
    fn a_magic_mismatch_yields_corruptformat() {
        let entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        let (_tmp, _path, mut bytes) = write_and_read(&entries);
        bytes[0] = b'X';
        assert!(matches!(validate(&bytes), Err(DictError::CorruptFormat(_))));
    }

    #[test]
    fn a_version_mismatch_yields_corruptformat() {
        let entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        let (_tmp, _path, mut bytes) = write_and_read(&entries);
        let bad_version = (FORMAT_VERSION + 1).to_le_bytes();
        bytes[OFF_FORMAT_VERSION..OFF_FORMAT_VERSION + 4].copy_from_slice(&bad_version);
        assert!(matches!(validate(&bytes), Err(DictError::CorruptFormat(_))));
    }

    /// D-93: an old v1 dictionary (`format_version = 1`) is not allowed to be
    /// read without the second layer; it is explicitly rejected with
    /// CorruptFormat. No backward-compatible read path exists.
    #[test]
    fn a_v1_dictionary_is_rejected_with_corruptformat() {
        let entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        let (_tmp, _path, mut bytes) = write_and_read(&entries);
        let v1 = 1u32.to_le_bytes();
        bytes[OFF_FORMAT_VERSION..OFF_FORMAT_VERSION + 4].copy_from_slice(&v1);
        let err = validate(&bytes).expect_err("v1 should yield CorruptFormat");
        assert!(matches!(err, DictError::CorruptFormat(_)));
    }

    #[test]
    fn a_header_crc_mismatch_yields_corruptformat() {
        let entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        let (_tmp, _path, mut bytes) = write_and_read(&entries);
        let bad_crc = 0xDEAD_BEEFu32.to_le_bytes();
        bytes[OFF_HEADER_CRC32..OFF_HEADER_CRC32 + 4].copy_from_slice(&bad_crc);
        assert!(matches!(validate(&bytes), Err(DictError::CorruptFormat(_))));
    }

    #[test]
    fn a_truncated_file_yields_corruptformat() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("にほんご".to_string(), vec![DictEntry::new("日本語")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        // Truncate in the middle of the kana key table (one 16-byte record,
        // [48..64)). key_table_offset(48) + entry_count(1) * KEY_RECORD_LEN(16)
        // = 64 is the end of the validated region, so anything shorter is
        // certain to be rejected by validate.
        let cut = HEADER_LEN + 10;
        assert!(
            cut < bytes.len(),
            "test precondition: the original file is longer than cut"
        );
        let truncated = &bytes[0..cut];
        assert!(matches!(
            validate(truncated),
            Err(DictError::CorruptFormat(_))
        ));
    }

    /// A byte slice whose `symspell_count` was overwritten with an excessive
    /// value, so the end of the SymSpell table would fall past the end of the
    /// file, yields CorruptFormat (T-03.1-01-01).
    #[test]
    fn an_excessive_symspell_count_yields_corruptformat() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かんじ".to_string(), vec![DictEntry::new("漢字")]);
        let (_tmp, _path, mut bytes) = write_and_read(&entries);
        let huge_count = u32::MAX.to_le_bytes();
        bytes[OFF_SYMSPELL_COUNT..OFF_SYMSPELL_COUNT + 4].copy_from_slice(&huge_count);
        // Overwriting symspell_count alone would break the header CRC and the
        // file would be rejected for a different reason, so recompute the CRC to
        // make sure we take the "table end past the end of file" path.
        let crc = crc32(&bytes[0..HEADER_CRC32_COVERED_LEN]);
        bytes[OFF_HEADER_CRC32..OFF_HEADER_CRC32 + 4].copy_from_slice(&crc.to_le_bytes());
        let err =
            validate(&bytes).expect_err("an excessive symspell_count should yield CorruptFormat");
        assert!(matches!(err, DictError::CorruptFormat(_)));
    }

    /// Writing the SymSpell hash table and reading it back through `DictView`
    /// finds a key's index from the hash of one of its delete variants (a round
    /// trip, D-91/92).
    #[test]
    fn a_symspell_hash_table_round_trip_finds_the_key_index() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かんじ".to_string(), vec![DictEntry::new("漢字")]);
        entries.insert("かんj".to_string(), vec![DictEntry::new("感")]);
        entries.insert(".".to_string(), vec![DictEntry::new("．")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        validate(&bytes).expect("validate failed");
        let view = DictView::new(&bytes);

        // We do not need the delete variant 「かん」 of 「かんじ」 to find
        // 「かんじ」 itself, but since 「かん」 is also a delete variant of
        // 「かんj」, at least the index of 「かんj」 must be found.
        let kanj_index = view.find_key("かんj").expect("かんj not found");
        let kanji_index = view.find_key("かんじ").expect("かんじ not found");

        let hash_kan = symspell::symspell_hash("かん");
        let postings = view.symspell_postings(hash_kan);
        assert!(
            postings.contains(&kanj_index),
            "the postings of 「かん」 should contain 「かんj」: {:?}",
            postings
        );
        assert!(
            postings.contains(&kanji_index),
            "the postings of 「かん」 should contain 「かんじ」: {:?}",
            postings
        );

        // D-94: the only delete variant of the single-character key 「.」 is the
        // empty string. With no exclusion rules it must be in the index.
        let hash_empty = symspell::symspell_hash("");
        let dot_index = view.find_key(".").expect(". not found");
        let empty_postings = view.symspell_postings(hash_empty);
        assert!(
            empty_postings.contains(&dot_index),
            "the postings of the empty-string variant should contain 「.」: {:?}",
            empty_postings
        );
    }

    /// D-94: both an okuri-ari conventional key (`かんj`) and a single-character
    /// key (`.`) go into the index (a direct check that there are no exclusion
    /// rules).
    #[test]
    fn okuri_ari_conventional_keys_and_single_char_keys_are_in_the_symspell_index() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かんj".to_string(), vec![DictEntry::new("感")]);
        entries.insert(".".to_string(), vec![DictEntry::new("．")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        let view = DictView::new(&bytes);
        // Check that symspell_count (the number of unique hashes) is not zero
        // (at least the 3 delete variants of 「かんj」 plus the 1 of 「.」).
        assert!(view.symspell_count() > 0);
    }

    /// A hash that is not present returns nothing.
    #[test]
    fn postings_for_an_unknown_hash_are_empty() {
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("にほんご".to_string(), vec![DictEntry::new("日本語")]);
        let (_tmp, _path, bytes) = write_and_read(&entries);
        let view = DictView::new(&bytes);
        assert_eq!(
            view.symspell_postings(0xDEAD_BEEF_0000_0000),
            Vec::<usize>::new()
        );
    }
}
