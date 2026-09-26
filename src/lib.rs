// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! # sekka - Japanese kana-kanji conversion library
//!
//! Sekka converts romaji input into kana and kanji and integrates with input
//! frameworks such as fcitx5 through its C ABI.

pub mod candidate;
pub mod capi;
pub mod context;
pub mod conversion;
pub mod dictionary;
pub mod fuzzy;
pub mod kana_romaji;
pub mod romaji;
pub mod roman_index;
pub mod symspell;
