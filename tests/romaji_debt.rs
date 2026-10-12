// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Phase 14 integration tests (the romaji debt left over from v1.4)
//!
//! These tests use only the public API of the library. They never copy the
//! romaji-to-kana conversion steps, the SKK text parser or the way a dictionary
//! index is built (IN-02 must not come back): every conversion goes through
//! `SekkaContext`, and every dictionary is written by `dict_format::write_dict`.

use std::collections::BTreeMap;
use std::sync::Arc;

use sekka::candidate::Candidate;
use sekka::context::SekkaContext;
use sekka::dictionary::dict_format;
use sekka::dictionary::immutable_dict::ImmutableFileDict;
use sekka::dictionary::{DictEntry, Dictionary};

#[test]
fn vyi_reaches_the_katakana_vi_candidate() {
    // ROMA-09 / D-213: typing Vyi must not leak the old wi kana. The hiragana
    // preedit stays the two-character form u + U+309B (D-207), and only the
    // katakana candidate is the single character U+30F4.
    let mut ctx = SekkaContext::new();
    for ch in "Vyi".chars() {
        ctx.process_key(ch, false);
    }
    // First Ctrl-J: stage the preedit as the commit display.
    assert!(ctx.process_key('\0', true));
    assert_eq!(ctx.get_preedit(), "\u{3046}\u{309B}\u{3043}");
    // Second Ctrl-J: go back to selecting, then take the katakana candidate.
    assert!(ctx.process_key('\0', true));
    assert!(ctx.select_katakana());
    assert_eq!(ctx.get_preedit(), "\u{30F4}\u{30A3}");
}

/// Writes a dictionary the way production does, types `typed`, presses Ctrl-J
/// twice (stage the first candidate, then go back to selecting) and returns the
/// candidate list.
fn candidates_for(entries: &[(&str, &str)], typed: &str) -> Vec<Candidate> {
    // Declared before `ctx` so the directory outlives the mapped dictionary file.
    let dir = tempfile::tempdir().expect("create a temporary directory");
    let mut map: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
    for (reading, word) in entries {
        map.entry((*reading).to_string())
            .or_default()
            .push(DictEntry::new(*word));
    }
    let path = dir.path().join("mini.dict");
    // The file is written once and never rewritten while it is mapped.
    dict_format::write_dict(&path, &map).expect("write the dictionary");
    let dict: Arc<dyn Dictionary> =
        Arc::new(ImmutableFileDict::open(&path).expect("open the dictionary"));

    let mut ctx = SekkaContext::new();
    ctx.set_dictionaries(vec![dict]);
    for ch in typed.chars() {
        ctx.process_key(ch, false);
    }
    assert!(ctx.process_key('\0', true), "first Ctrl-J for {typed:?}");
    assert!(ctx.process_key('\0', true), "second Ctrl-J for {typed:?}");
    ctx.get_candidates().to_vec()
}

#[test]
fn yuwe_reaches_a_word_whose_reading_is_yuwe_through_the_fuzzy_layer() {
    // ROMA-07 / D-218: the kana for the typed yuwe is ゆうぇ, which is neither an
    // exact match for the reading ゆゑ nor within reach of SymSpell, so only the
    // fuzzy layer can find it, through the reverse table (ゑ -> we). The decoy
    // ゆうえ is reached by SymSpell and is not counted here.
    for typed in ["yuwe", "Yuwe"] {
        let candidates = candidates_for(&[("ゆゑ", "故"), ("ゆうえ", "夕絵")], typed);
        assert!(
            candidates
                .iter()
                .any(|c| c.display == "故" && c.reading == "ゆゑ"),
            "typed={typed:?} candidates={candidates:?}"
        );
    }
}

#[test]
fn awi_and_wido_reach_words_whose_readings_have_wi() {
    // ROMA-07 / D-217: the ゐ side of the reverse table (ゐ -> wi).
    let entries = [("あゐ", "藍"), ("ゐど", "井戸")];
    for (typed, word) in [("awi", "藍"), ("wido", "井戸")] {
        let candidates = candidates_for(&entries, typed);
        assert!(
            candidates.iter().any(|c| c.display == word),
            "typed={typed:?} candidates={candidates:?}"
        );
    }
}
