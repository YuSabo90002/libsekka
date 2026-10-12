// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Phase 14 integration tests (the romaji debt left over from v1.4)
//!
//! These tests use only the public API of the library. They never copy the
//! romaji-to-kana conversion steps, the SKK text parser or the way a dictionary
//! index is built (IN-02 must not come back): every conversion goes through
//! `SekkaContext`, and every dictionary is written by `dict_format::write_dict`.

use sekka::context::SekkaContext;

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
