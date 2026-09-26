// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
// SPDX-FileCopyrightText: 2010-2026 Kiyoka Nishiyama <kiyoka@sumibi.org>
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Parts of this file are ported from Sekka (https://github.com/kiyoka/sekka),
// master @ 0f73ee9 (retrieved 2026-09-23): emacs/sekka-jarowinkler.el.

//! Kana -> canonical Hepburn romaji conversion
//!
//! A one-way conversion ported from `sekka-jarowinkler--kana-to-hepburn-alist`
//! and `sekka-jarowinkler-hiragana->roman` in `emacs/sekka-jarowinkler.el` of
//! upstream Sekka. It exists solely to build the romaji index (at build time)
//! and is a different thing from `romaji.rs`, which maps romaji -> kana on the
//! input side and accepts several spellings for the same kana.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Canonical hiragana -> Hepburn romaji mapping (single stage).
///
/// Source: <https://raw.githubusercontent.com/kiyoka/sekka/master/emacs/sekka-jarowinkler.el>
/// (retrieved 2026-09-23; lines 104-157, `sekka-jarowinkler--kana-to-hepburn-alist`,
/// transcribed in full. Nothing was added, changed or dropped. 「ん」 and 「っ」
/// are absent from the upstream table and are handled by dedicated logic in
/// `kana_to_hepburn`.)
pub const KANA_TO_HEPBURN: &[(&str, &str)] = &[
    // Small kana
    ("ぁ", "a"),
    ("ぃ", "i"),
    ("ぅ", "u"),
    ("ぇ", "e"),
    ("ぉ", "o"),
    ("ゃ", "ya"),
    ("ゅ", "yu"),
    ("ょ", "yo"),
    ("ゎ", "wa"),
    // A row
    ("あ", "a"),
    ("い", "i"),
    ("う", "u"),
    ("え", "e"),
    ("お", "o"),
    // KA row
    ("か", "ka"),
    ("き", "ki"),
    ("く", "ku"),
    ("け", "ke"),
    ("こ", "ko"),
    ("きゃ", "kya"),
    ("きゅ", "kyu"),
    ("きょ", "kyo"),
    ("きぇ", "kye"),
    // GA row
    ("が", "ga"),
    ("ぎ", "gi"),
    ("ぐ", "gu"),
    ("げ", "ge"),
    ("ご", "go"),
    ("ぎゃ", "gya"),
    ("ぎゅ", "gyu"),
    ("ぎょ", "gyo"),
    ("ぎぇ", "gye"),
    // SA row
    ("さ", "sa"),
    ("し", "shi"),
    ("す", "su"),
    ("せ", "se"),
    ("そ", "so"),
    ("しゃ", "sha"),
    ("しゅ", "shu"),
    ("しょ", "sho"),
    ("しぇ", "she"),
    // ZA row
    ("ざ", "za"),
    ("じ", "ji"),
    ("ず", "zu"),
    ("ぜ", "ze"),
    ("ぞ", "zo"),
    ("じゃ", "ja"),
    ("じゅ", "ju"),
    ("じょ", "jo"),
    ("じぇ", "je"),
    // TA row
    ("た", "ta"),
    ("ち", "chi"),
    ("つ", "tsu"),
    ("て", "te"),
    ("と", "to"),
    ("ちゃ", "cha"),
    ("ちゅ", "chu"),
    ("ちょ", "cho"),
    ("ちぇ", "che"),
    ("てぃ", "ti"),
    ("とぅ", "tu"),
    // DA row
    ("だ", "da"),
    ("ぢ", "ji"),
    ("づ", "zu"),
    ("で", "de"),
    ("ど", "do"),
    ("でぃ", "di"),
    ("どぅ", "du"),
    // NA row
    ("な", "na"),
    ("に", "ni"),
    ("ぬ", "nu"),
    ("ね", "ne"),
    ("の", "no"),
    ("にゃ", "nya"),
    ("にゅ", "nyu"),
    ("にょ", "nyo"),
    ("にぇ", "nye"),
    // HA row
    ("は", "ha"),
    ("ひ", "hi"),
    ("ふ", "fu"),
    ("へ", "he"),
    ("ほ", "ho"),
    ("ひゃ", "hya"),
    ("ひゅ", "hyu"),
    ("ひょ", "hyo"),
    ("ひぇ", "hye"),
    ("ふぁ", "fa"),
    ("ふぃ", "fi"),
    ("ふぇ", "fe"),
    ("ふぉ", "fo"),
    // BA row
    ("ば", "ba"),
    ("び", "bi"),
    ("ぶ", "bu"),
    ("べ", "be"),
    ("ぼ", "bo"),
    ("びゃ", "bya"),
    ("びゅ", "byu"),
    ("びょ", "byo"),
    ("びぇ", "bye"),
    // PA row
    ("ぱ", "pa"),
    ("ぴ", "pi"),
    ("ぷ", "pu"),
    ("ぺ", "pe"),
    ("ぽ", "po"),
    ("ぴゃ", "pya"),
    ("ぴゅ", "pyu"),
    ("ぴょ", "pyo"),
    ("ぴぇ", "pye"),
    // MA row
    ("ま", "ma"),
    ("み", "mi"),
    ("む", "mu"),
    ("め", "me"),
    ("も", "mo"),
    ("みゃ", "mya"),
    ("みゅ", "myu"),
    ("みょ", "myo"),
    ("みぇ", "mye"),
    // YA row
    ("や", "ya"),
    ("ゆ", "yu"),
    ("よ", "yo"),
    // RA row
    ("ら", "ra"),
    ("り", "ri"),
    ("る", "ru"),
    ("れ", "re"),
    ("ろ", "ro"),
    ("りゃ", "rya"),
    ("りゅ", "ryu"),
    ("りょ", "ryo"),
    ("りぇ", "rye"),
    // WA row
    ("わ", "wa"),
    ("ゐ", "wi"),
    ("ゑ", "we"),
    ("を", "wo"),
    // VU row
    ("う゛", "vu"),
    ("う゛ぁ", "va"),
    ("う゛ぃ", "vi"),
    ("う゛ぇ", "ve"),
    ("う゛ぉ", "vo"),
    // Long vowel mark and others
    ("ー", "-"),
];

/// Lookup table built by hashing `KANA_TO_HEPBURN` exactly once.
fn kana_hash() -> &'static HashMap<&'static str, &'static str> {
    static HASH: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    HASH.get_or_init(|| KANA_TO_HEPBURN.iter().copied().collect())
}

/// Ported from `sekka-jarowinkler-hiragana->roman`.
///
/// 「ん」 alone becomes `n`; 「っ」 doubles the leading consonant of the
/// following kana's romaji (doubling `t` when that romaji starts with `ch`, so
/// 「まっちゃ」 is `matcha`, not `ccha`). Two-character combinations (youon,
/// 「う゛ぁ」 and friends) are tried before single characters. Characters that
/// match neither are passed through to the output (a fallback for non-hiragana
/// input).
pub fn kana_to_hepburn(hiragana: &str) -> String {
    let h = kana_hash();
    let chars: Vec<char> = hiragana.chars().collect();
    let len = chars.len();
    let mut pos = 0usize;
    let mut result = String::new();
    let mut pending_sokuon = false;

    while pos < len {
        let c1 = chars[pos];

        if c1 == 'ん' {
            result.push('n');
            pos += 1;
            continue;
        }
        if c1 == 'っ' {
            pending_sokuon = true;
            pos += 1;
            continue;
        }

        let remain = len - pos;
        let s2 = if remain >= 2 {
            Some(chars[pos..pos + 2].iter().collect::<String>())
        } else {
            None
        };
        let m2 = s2.as_deref().and_then(|s| h.get(s).copied());

        if let Some(m2) = m2 {
            if pending_sokuon {
                if m2.starts_with("ch") {
                    result.push('t');
                } else {
                    result.push_str(&m2[..1]);
                }
                pending_sokuon = false;
            }
            result.push_str(m2);
            pos += 2;
            continue;
        }

        let s1 = c1.to_string();
        if let Some(m1) = h.get(s1.as_str()).copied() {
            if pending_sokuon {
                result.push_str(&m1[..1]);
                pending_sokuon = false;
            }
            result.push_str(m1);
            pos += 1;
            continue;
        }

        // Pass unknown characters through unchanged.
        result.push(c1);
        pos += 1;
        pending_sokuon = false;
    }

    if pending_sokuon {
        result.push('t');
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn henkan_romanizes_to_henkan() {
        assert_eq!(kana_to_hepburn("へんかん"), "henkan");
    }

    #[test]
    fn shizengengoshori_romanizes_correctly() {
        assert_eq!(kana_to_hepburn("しぜんげんごしょり"), "shizengengoshori");
    }

    #[test]
    fn nikki_doubles_the_consonant_after_sokuon() {
        assert_eq!(kana_to_hepburn("にっき"), "nikki");
    }

    #[test]
    fn matcha_turns_sokuon_before_ch_into_t() {
        assert_eq!(kana_to_hepburn("まっちゃ"), "matcha");
    }

    #[test]
    fn kanji_romanizes_to_kanji() {
        assert_eq!(kana_to_hepburn("かんじ"), "kanji");
    }

    #[test]
    fn kani_collision_material_for_d75() {
        assert_eq!(kana_to_hepburn("かんい"), "kani");
        assert_eq!(kana_to_hepburn("かに"), "kani");
    }

    #[test]
    fn long_vowel_mark_becomes_a_hyphen() {
        assert_eq!(kana_to_hepburn("ー"), "-");
    }
}
