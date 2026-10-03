// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
// SPDX-FileCopyrightText: 2010-2026 Kiyoka Nishiyama <kiyoka@sumibi.org>
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Parts of this file are ported from Sekka (https://github.com/kiyoka/sekka),
// master @ 0f73ee9 (retrieved 2026-09-23): emacs/sekka-sharp-number.el, emacs/sekka-tests.el.

//! Conversion candidate structs and sorting logic
//!
//! Builds conversion candidates from dictionary lookup results and decides the
//! order in which they are shown to the user.

use crate::conversion::ConversionMode;
use crate::dictionary::DictEntry;

/// Candidate kind
///
/// Describes which script a conversion candidate is written in. It is also used
/// to decide sort groups.
///
/// `Kanji` / `KanjiWithOkuri` are dictionary-derived candidates (upstream's kanji
/// group, without and with okurigana). `Hiragana` / `Katakana` /
/// `AlphabetZenkaku` / `AlphabetHankaku` are generated from the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateKind {
    /// Kanji (from the dictionary, no okurigana).
    Kanji,
    /// Hiragana (generated from the input).
    Hiragana,
    /// Katakana (generated from the input).
    Katakana,
    /// Kanji with okurigana (from the dictionary, okuri-ari).
    KanjiWithOkuri,
    /// Full-width alphabet (generated from the input).
    AlphabetZenkaku,
    /// Half-width alphabet (generated from the input).
    AlphabetHankaku,
}

/// Conversion candidate
///
/// One conversion candidate obtained from a dictionary lookup. It holds the
/// display string, the reading, the kind, the similarity score and the selection
/// frequency.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The string to display (e.g. "漢字").
    pub display: String,
    /// The kana reading (e.g. "かんじ").
    pub reading: String,
    /// The candidate kind.
    pub kind: CandidateKind,
    /// Jaro-Winkler similarity score (0.0 to 1.0).
    pub score: f64,
    /// Last-selected sequence number in the user dictionary (0 = never selected).
    pub last_selected: u64,
    /// Candidate tier (D-96, 03.1-01). 0 = exact match and JW=1.0, 1 = SymSpell
    /// (distance 1), 2 = JW<1.0. Scores for JW<1.0 run continuously from 0.94
    /// up to just below 1.0, so placing SymSpell "after JW=1.0 and before
    /// JW<1.0" cannot be expressed by a single score value and needs its own
    /// field.
    ///
    /// `sort_candidates` derives the match stage from this value with
    /// `match_stage` (D-144: tier 0 -> stage 0, tiers 1 and 2 -> stage 1) and
    /// compares the stage before frequency, replacing D-96's "frequency before
    /// tier" (03.1-01) so a fuzzy candidate learned under a different reading
    /// can no longer outrank an unlearned exact match (RANK-01). `tier` itself
    /// is still compared after frequency, inside the fuzzy stage, so a learned
    /// tier-2 candidate keeps outranking an unlearned tier-1 one (D-146).
    /// Generated candidates (hiragana, katakana, full-width and half-width
    /// alphabet) are always `0`, but never take part in the stage comparison:
    /// `group_rank` already separates them from dictionary candidates before
    /// the stage is even compared.
    pub tier: u8,
    /// Learning pair (dictionary key, raw word as stored in the dictionary) (D-33)
    ///
    /// Only candidates obtained from a dictionary lookup (through
    /// `build_candidates`) get `Some`. Generated candidates (hiragana, katakana,
    /// full-width and half-width alphabet) get `None` (the provenance decision of
    /// D-32 looks only at whether this field is `Some` or `None`, never at
    /// `CandidateKind`). Even after `display`/`reading` are reshaped by appending
    /// okurigana or by `#` substitution, this field keeps the raw dictionary data
    /// (the reading and word before any reshaping).
    pub learn_pair: Option<(String, String)>,
}

/// Returns the group rank for a conversion mode (lower wins; D-07)
///
/// - With uppercase (`KanjiConvert` / `KanjiWithOkuri` / `OkuriMarker`):
///   kanji (with and without okurigana, one group) -> hiragana -> katakana ->
///   full-width alphabet -> half-width alphabet
/// - All lowercase (`HiraganaOnly`):
///   hiragana -> katakana -> full-width alphabet -> half-width alphabet ->
///   kanji (with and without okurigana, one group)
fn group_rank(kind: &CandidateKind, mode: ConversionMode) -> u8 {
    match mode {
        ConversionMode::HiraganaOnly => match kind {
            CandidateKind::Hiragana => 0,
            CandidateKind::Katakana => 1,
            CandidateKind::AlphabetZenkaku => 2,
            CandidateKind::AlphabetHankaku => 3,
            CandidateKind::Kanji | CandidateKind::KanjiWithOkuri => 4,
        },
        ConversionMode::KanjiConvert
        | ConversionMode::KanjiWithOkuri
        | ConversionMode::OkuriMarker => match kind {
            CandidateKind::Kanji | CandidateKind::KanjiWithOkuri => 0,
            CandidateKind::Hiragana => 1,
            CandidateKind::Katakana => 2,
            CandidateKind::AlphabetZenkaku => 3,
            CandidateKind::AlphabetHankaku => 4,
        },
    }
}

/// Maps a candidate's `tier` to its match stage (D-144)
///
/// The match stage is a 2-value grouping used by `sort_candidates` to keep
/// every exact match ahead of every fuzzy match, regardless of learning:
/// tier 0 (exact match / JW=1.0) maps to stage 0, and tiers 1 (SymSpell) and 2
/// (JW<1.0) both map to stage 1 (fuzzy). `Candidate` gets no new field for
/// this - the stage is always derived from `tier`.
pub(crate) fn match_stage(tier: u8) -> u8 {
    if tier == 0 {
        0
    } else {
        1
    }
}

/// Sorts a list of conversion candidates
///
/// Sorting follows this priority:
/// 1. group rank (ascending) - the fixed category order for the conversion mode
/// 2. match stage (ascending) - `match_stage(tier)` (D-144). Exact match (tier 0)
///    always outranks fuzzy match (tier 1 or 2), no matter how much a fuzzy
///    candidate has been learned. D-96 (Phase 03.1) used to compare frequency
///    before tier, which let a fuzzy candidate learned under a different
///    reading overtake an unlearned exact match (RANK-01); D-144 (Phase 8)
///    replaces that by inserting this stage ahead of frequency.
/// 3. selection frequency (descending) - only effective within one stage, so a
///    learned candidate still wins inside the exact-match stage (RANK-02) and
///    inside the fuzzy stage (D-145)
/// 4. tier (ascending) - exact match/JW=1.0 (0) -> SymSpell (1) -> JW<1.0 (2).
///    Only breaks ties within the fuzzy stage, where tier 1 and tier 2 already
///    share the same stage value (D-146: a learned tier-2 candidate still
///    outranks an unlearned tier-1 one, because frequency is compared first)
/// 5. similarity score (descending) - only effective within one tier
pub fn sort_candidates(candidates: &mut [Candidate], mode: ConversionMode) {
    candidates.sort_by(|a, b| {
        group_rank(&a.kind, mode)
            .cmp(&group_rank(&b.kind, mode))
            .then_with(|| match_stage(a.tier).cmp(&match_stage(b.tier)))
            .then_with(|| b.last_selected.cmp(&a.last_selected))
            .then_with(|| a.tier.cmp(&b.tier))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
}

/// Builds a list of conversion candidates from dictionary entries
///
/// Converts dictionary entries into conversion candidates using the given
/// reading, similarity score and tier (D-96). The candidate kind is detected
/// automatically from the scripts in the display string. This is the **only**
/// function that creates dictionary-derived candidates, and it sets `learn_pair`
/// to `(reading, entry.word)` (the raw data before any reshaping) (D-33). `tier`
/// is passed explicitly by the caller (0 from `lookup_dictionary_exact`, 1 from
/// the SymSpell layer, 2 from the JW<1.0 fuzzy search).
pub fn build_candidates(
    reading: &str,
    entries: &[DictEntry],
    score: f64,
    tier: u8,
) -> Vec<Candidate> {
    entries
        .iter()
        .map(|entry| {
            let kind = classify_dictionary_kind(&entry.word);
            Candidate {
                display: entry.word.clone(),
                reading: reading.to_string(),
                kind,
                score,
                last_selected: entry.last_selected,
                tier,
                learn_pair: Some((reading.to_string(), entry.word.clone())),
            }
        })
        .collect()
}

/// Builds the hiragana fallback candidate
///
/// Returns the reading itself as a hiragana candidate when the dictionary has no
/// matching conversion candidate. The score is 1.0 (an exact match) and the
/// frequency is 0.
pub fn hiragana_candidate(reading: &str) -> Candidate {
    Candidate {
        display: reading.to_string(),
        reading: reading.to_string(),
        kind: CandidateKind::Hiragana,
        score: 1.0,
        last_selected: 0,
        tier: 0,
        learn_pair: None,
    }
}

/// Builds the katakana candidate
///
/// Converts a hiragana reading to katakana and builds a candidate from it. The
/// score is 1.0 (an exact match) and the frequency is 0.
pub fn katakana_candidate(reading: &str) -> Candidate {
    let katakana = hiragana_to_katakana(reading);
    Candidate {
        display: katakana,
        reading: reading.to_string(),
        kind: CandidateKind::Katakana,
        score: 1.0,
        last_selected: 0,
        tier: 0,
        learn_pair: None,
    }
}

/// Builds the full-width alphabet candidate (D-05)
///
/// Displays the romaji buffer as typed (preserving case) converted to full-width
/// characters. The score is 1.0 (an exact match) and the frequency is 0.
pub fn alphabet_zenkaku_candidate(raw: &str) -> Candidate {
    Candidate {
        display: ascii_to_fullwidth(raw),
        reading: raw.to_string(),
        kind: CandidateKind::AlphabetZenkaku,
        score: 1.0,
        last_selected: 0,
        tier: 0,
        learn_pair: None,
    }
}

/// Builds the half-width alphabet candidate (D-05)
///
/// Displays the romaji buffer as typed (preserving case), unchanged. The score is
/// 1.0 (an exact match) and the frequency is 0.
pub fn alphabet_hankaku_candidate(raw: &str) -> Candidate {
    Candidate {
        display: raw.to_string(),
        reading: raw.to_string(),
        kind: CandidateKind::AlphabetHankaku,
        score: 1.0,
        last_selected: 0,
        tier: 0,
        learn_pair: None,
    }
}

/// Converts half-width ASCII to full-width (Fullwidth Forms)
///
/// Printable ASCII (U+0021 to U+007E) maps to U+FF01 to U+FF5E by the fixed
/// offset +0xFEE0, and the half-width space (U+0020) maps individually to U+3000
/// (the ideographic space). Any other character (including already full-width
/// ones) is preserved as it is.
fn ascii_to_fullwidth(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            ' ' => '\u{3000}',
            '\x21'..='\x7e' => char::from_u32(c as u32 + 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// Converts hiragana to katakana
///
/// In Unicode, hiragana (ぁ U+3041 to ん U+3093) sits 0x60 below the
/// corresponding katakana (ァ U+30A1 to ン U+30F3); the conversion uses that
/// difference. Characters outside the hiragana range are preserved.
fn hiragana_to_katakana(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            // Convert the range ぁ(U+3041) to ん(U+3093) into katakana.
            if ('\u{3041}'..='\u{3093}').contains(&c) {
                char::from_u32(c as u32 + 0x60).unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

/// Decides the candidate kind from the display string of a dictionary entry (D-07)
///
/// A word containing both kanji and hiragana counts as having okurigana
/// (`KanjiWithOkuri`); everything else (kanji only, hiragana only, katakana only)
/// counts as `Kanji`. Dictionary-derived words go into the dictionary (kanji)
/// group even when written in kana or katakana - our reading of the fact that
/// upstream sekka-henkan puts dictionary results first as they are.
fn classify_dictionary_kind(word: &str) -> CandidateKind {
    let has_kanji = word.chars().any(is_kanji);
    let has_hiragana = word.chars().any(is_hiragana);

    if has_kanji && has_hiragana {
        CandidateKind::KanjiWithOkuri
    } else {
        CandidateKind::Kanji
    }
}

/// Decides whether a character is a CJK unified ideograph
fn is_kanji(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
        || ('\u{3400}'..='\u{4DBF}').contains(&c)
        || ('\u{F900}'..='\u{FAFF}').contains(&c)
}

/// Decides whether a character is hiragana
fn is_hiragana(c: char) -> bool {
    ('\u{3041}'..='\u{309F}').contains(&c)
}

/// Kanji numerals for the digits 0-9 (without positional weight)
const KANJI_DIGITS: [char; 10] = ['〇', '一', '二', '三', '四', '五', '六', '七', '八', '九'];

/// Positions within a 4-digit group (the same 4 elements as upstream `sekka-kurai1`)
const KURAI1: [&str; 4] = ["", "十", "百", "千"];

/// Positions between 4-digit groups. Upstream `sekka-kurai2`
/// (`["", "万", "億", "兆", "京"]`) has 5 elements; we extended it above 京 with
/// the standard Japanese myriad-scale positions to 13 elements (a 52-digit
/// ceiling) (D-31). The first 5 elements are identical to upstream, so output up
/// to 20 digits does not differ from upstream's test vectors by a single
/// character. For 秭 we take the BMP variant (avoiding the non-BMP 𥝱).
const KURAI2: [&str; 13] = [
    "", "万", "億", "兆", "京", "垓", "秭", "穣", "溝", "澗", "正", "載", "極",
];

/// Performs the `#1`/`#2`/`#3` number conversions (ported from upstream
/// `emacs/sekka-sharp-number.el:84-102`, D-21)
///
/// - `#1`: half-width to full-width (delegated to the existing `ascii_to_fullwidth`)
/// - `#2`: map each digit independently to a kanji numeral (no positional weight)
/// - `#3`: positional kanji numerals (`kansuuji`)
/// - anything else (an unknown type such as `#0`): no conversion (upstream's
///   `(t num-str)` fallback)
///
/// Digits are extracted with `char::to_digit(10)`; any non-digit character mixed
/// in is left as it is without conversion (the `unwrap_or` fallback, T-01.3-02).
pub fn sharp_number(type_str: &str, num_str: &str) -> String {
    match type_str {
        "#1" => ascii_to_fullwidth(num_str),
        "#2" => num_str
            .chars()
            .map(|c| {
                c.to_digit(10)
                    .and_then(|d| KANJI_DIGITS.get(d as usize).copied())
                    .unwrap_or(c)
            })
            .collect(),
        "#3" => kansuuji(num_str),
        _ => num_str.to_string(),
    }
}

/// Positional kanji numerals (ported from upstream
/// `emacs/sekka-sharp-number.el:53-82`, D-21/D-31)
///
/// Splits the number into groups of 4 digits from the least significant end,
/// applies `kansuuji_sen` within each group and joins them with the positions
/// from `KURAI2`. Groups whose digits are all `0` are skipped (this is the rule
/// that makes `"1000000000002"` become `"一兆二"`). When the group index goes past
/// the end of `KURAI2` (53 digits or more, the terminus of D-31), positional
/// conversion is abandoned and `num_str` is returned unchanged as a no-conversion
/// fallback. `KURAI2` is never indexed directly; elements are taken safely with
/// `get` (T-01.3-01).
fn kansuuji(num_str: &str) -> String {
    let chars: Vec<char> = num_str.chars().collect();
    // Split into groups of 4 digits from the end, so groups[0] is the ones group (the last 4 digits).
    let mut groups: Vec<String> = Vec::new();
    let mut end = chars.len();
    while end > 0 {
        let start = end.saturating_sub(4);
        groups.push(chars[start..end].iter().collect());
        end = start;
    }

    let mut result = String::new();
    for (gi, group) in groups.iter().enumerate() {
        let Some(kurai2) = KURAI2.get(gi) else {
            // 53 digits or more (past the end of KURAI2). Fall back unconditionally
            // even when the group is all zeros (WR-01: check the bound before
            // skipping zeros) (T-01.3-01).
            return num_str.to_string();
        };
        if group.chars().all(|c| c == '0') {
            continue;
        }
        result = format!("{}{}{}", kansuuji_sen(group), kurai2, result);
    }

    if result.is_empty() {
        // All digits are 0 (e.g. "0", "00"): return 「〇」 (CR-01: closes the hole where an all-zero input became an empty string).
        KANJI_DIGITS[0].to_string()
    } else {
        result
    }
}

/// Positional kanji numerals for one 4-digit group (ported from upstream
/// `emacs/sekka-sharp-number.el:30-51`, D-21)
///
/// Processes a group of at most 4 digits from the least significant end, adding
/// the positional weight (`KURAI1`). A digit of `0` emits nothing. A digit of `1`
/// whose position is non-empty (十/百/千) emits the position only, without the
/// digit (this is the rule that makes `"10"` into `"十"` and `"1000"` into
/// `"千"`). When the position is empty (the ones place) only the digit is
/// emitted. Since it accumulates from the least significant end, the result is
/// reversed into most-significant-first order before being joined.
fn kansuuji_sen(group: &str) -> String {
    let digits: Vec<u32> = group
        .chars()
        .rev()
        .map(|c| c.to_digit(10).unwrap_or(0))
        .collect();
    let mut parts = Vec::new();
    for (i, &d) in digits.iter().enumerate() {
        let kurai = KURAI1.get(i).copied().unwrap_or("");
        let digit_char = KANJI_DIGITS.get(d as usize).copied().unwrap_or('〇');
        let part = match d {
            0 => String::new(),
            1 if !kurai.is_empty() => kurai.to_string(),
            _ if kurai.is_empty() => digit_char.to_string(),
            _ => format!("{}{}", digit_char, kurai),
        };
        parts.push(part);
    }
    parts.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiragana_converts_to_katakana() {
        assert_eq!(hiragana_to_katakana("かんじ"), "カンジ");
        assert_eq!(hiragana_to_katakana("ひらがな"), "ヒラガナ");
        assert_eq!(hiragana_to_katakana("あいうえお"), "アイウエオ");
    }

    #[test]
    fn non_hiragana_characters_are_preserved() {
        // Kanji and ASCII characters stay as they are.
        assert_eq!(hiragana_to_katakana("abc"), "abc");
        assert_eq!(hiragana_to_katakana("漢字"), "漢字");
        // In mixed input only the hiragana part is converted.
        assert_eq!(hiragana_to_katakana("あbc"), "アbc");
    }

    #[test]
    fn dictionary_candidate_kind_detection() {
        assert_eq!(classify_dictionary_kind("漢字"), CandidateKind::Kanji);
        assert_eq!(
            classify_dictionary_kind("感じる"),
            CandidateKind::KanjiWithOkuri
        );
        assert_eq!(classify_dictionary_kind("ひらがな"), CandidateKind::Kanji);
        assert_eq!(classify_dictionary_kind("カタカナ"), CandidateKind::Kanji);
    }

    #[test]
    fn sorting_by_frequency() {
        let mut candidates = vec![
            Candidate {
                display: "漢字A".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.9,
                last_selected: 1,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "漢字B".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.9,
                last_selected: 10,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "漢字C".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.9,
                last_selected: 5,
                tier: 0,
                learn_pair: None,
            },
        ];

        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);

        assert_eq!(candidates[0].display, "漢字B");
        assert_eq!(candidates[1].display, "漢字C");
        assert_eq!(candidates[2].display, "漢字A");
    }

    #[test]
    fn sorting_by_score() {
        let mut candidates = vec![
            Candidate {
                display: "感じ".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::KanjiWithOkuri,
                score: 0.7,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.95,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "幹事".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.85,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];

        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);

        // The frequencies are equal, so the order is by descending score.
        assert_eq!(candidates[0].display, "漢字");
        assert_eq!(candidates[1].display, "幹事");
        assert_eq!(candidates[2].display, "感じ");
    }

    #[test]
    fn group_order_in_the_uppercase_mode() {
        // Even listed in reverse order (AlphabetHankaku -> AlphabetZenkaku ->
        // Katakana -> Hiragana -> KanjiWithOkuri -> Kanji), the uppercase mode
        // yields Kanji and KanjiWithOkuri (one group, original order) -> Hiragana
        // -> Katakana -> AlphabetZenkaku -> AlphabetHankaku (D-07).
        let mut candidates = vec![
            Candidate {
                display: "Ka".to_string(),
                reading: "Ka".to_string(),
                kind: CandidateKind::AlphabetHankaku,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "Ｋａ".to_string(),
                reading: "Ka".to_string(),
                kind: CandidateKind::AlphabetZenkaku,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "カ".to_string(),
                reading: "か".to_string(),
                kind: CandidateKind::Katakana,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "か".to_string(),
                reading: "か".to_string(),
                kind: CandidateKind::Hiragana,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "感じる".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::KanjiWithOkuri,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];

        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);

        assert_eq!(candidates[0].kind, CandidateKind::KanjiWithOkuri);
        assert_eq!(candidates[1].kind, CandidateKind::Kanji);
        assert_eq!(candidates[2].kind, CandidateKind::Hiragana);
        assert_eq!(candidates[3].kind, CandidateKind::Katakana);
        assert_eq!(candidates[4].kind, CandidateKind::AlphabetZenkaku);
        assert_eq!(candidates[5].kind, CandidateKind::AlphabetHankaku);
    }

    #[test]
    fn group_order_in_the_all_lowercase_mode() {
        // In the all-lowercase mode the order is Hiragana -> Katakana ->
        // AlphabetZenkaku -> AlphabetHankaku -> Kanji and KanjiWithOkuri (one
        // group, original order) (D-07).
        let mut candidates = vec![
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "感じる".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::KanjiWithOkuri,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "Ka".to_string(),
                reading: "Ka".to_string(),
                kind: CandidateKind::AlphabetHankaku,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "Ｋａ".to_string(),
                reading: "Ka".to_string(),
                kind: CandidateKind::AlphabetZenkaku,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "カ".to_string(),
                reading: "か".to_string(),
                kind: CandidateKind::Katakana,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "か".to_string(),
                reading: "か".to_string(),
                kind: CandidateKind::Hiragana,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];

        sort_candidates(&mut candidates, ConversionMode::HiraganaOnly);

        assert_eq!(candidates[0].kind, CandidateKind::Hiragana);
        assert_eq!(candidates[1].kind, CandidateKind::Katakana);
        assert_eq!(candidates[2].kind, CandidateKind::AlphabetZenkaku);
        assert_eq!(candidates[3].kind, CandidateKind::AlphabetHankaku);
        assert_eq!(candidates[4].kind, CandidateKind::Kanji);
        assert_eq!(candidates[5].kind, CandidateKind::KanjiWithOkuri);
    }

    #[test]
    fn frequency_never_crosses_groups() {
        // With uppercase, a Katakana candidate with frequency 100 still comes after a Kanji one with frequency 0.
        let mut candidates = vec![
            Candidate {
                display: "カンジ".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Katakana,
                score: 1.0,
                last_selected: 100,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        assert_eq!(candidates[0].kind, CandidateKind::Kanji);
        assert_eq!(candidates[1].kind, CandidateKind::Katakana);

        // All lowercase: a Kanji candidate with frequency 100 comes after a Hiragana one with frequency 0.
        let mut candidates2 = vec![
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 100,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "かんじ".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Hiragana,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates2, ConversionMode::HiraganaOnly);
        assert_eq!(candidates2[0].kind, CandidateKind::Hiragana);
        assert_eq!(candidates2[1].kind, CandidateKind::Kanji);
    }

    #[test]
    fn a_lower_tier_wins_and_is_compared_between_frequency_and_score() {
        // Under D-144 (Phase 8, replacing D-96's "frequency before tier"), the
        // match stage (`match_stage(tier)`) is compared before frequency, and
        // `tier` itself is still compared after frequency and before score.
        // Here both candidates have the same frequency (0), so D-96 and D-144
        // produce the same result: a tier-1 candidate still loses to a tier-0
        // one even with a higher score (same group, same frequency).
        let mut candidates = vec![
            Candidate {
                display: "SymSpell候補".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 1,
                learn_pair: None,
            },
            Candidate {
                display: "完全一致候補".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.5,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        assert_eq!(candidates[0].display, "完全一致候補");
        assert_eq!(candidates[1].display, "SymSpell候補");
    }

    #[test]
    fn with_equal_tiers_descending_score_still_applies() {
        // Under D-96, and under D-144 (Phase 8, replacing D-96's "frequency
        // before tier"), descending score inside the same tier is unchanged:
        // both candidates share the same match stage (tier 1), so the stage
        // comparison is a no-op here and score still decides the order.
        let mut candidates = vec![
            Candidate {
                display: "低スコア".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.5,
                last_selected: 0,
                tier: 1,
                learn_pair: None,
            },
            Candidate {
                display: "高スコア".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.9,
                last_selected: 0,
                tier: 1,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        assert_eq!(candidates[0].display, "高スコア");
        assert_eq!(candidates[1].display, "低スコア");
    }

    #[test]
    fn sorting_with_combined_conditions() {
        let mut candidates = vec![
            Candidate {
                display: "かんじ".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Hiragana,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.9,
                last_selected: 5,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "幹事".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.85,
                last_selected: 5,
                tier: 0,
                learn_pair: None,
            },
        ];

        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);

        // The frequency-5 candidates come first, ordered by score among themselves.
        assert_eq!(candidates[0].display, "漢字");
        assert_eq!(candidates[1].display, "幹事");
        assert_eq!(candidates[2].display, "かんじ");
    }

    #[test]
    fn match_stage_maps_tier_0_to_the_exact_stage_and_tiers_1_and_2_to_the_fuzzy_stage() {
        // D-144: the match stage is a 2-value grouping derived from tier. Tier
        // 0 (exact match / JW=1.0) is stage 0; tiers 1 (SymSpell) and 2
        // (JW<1.0) both fall into stage 1 (fuzzy), so they compare equal here.
        assert_eq!(match_stage(0), 0);
        assert_eq!(match_stage(1), 1);
        assert_eq!(match_stage(2), 1);
    }

    #[test]
    fn an_unlearned_exact_match_outranks_learned_fuzzy_candidates() {
        // RANK-01 / D-144: no amount of learning on a fuzzy candidate (tier 1
        // or 2) should let it outrank an unlearned exact match (tier 0). This
        // is the regression this phase fixes - under D-96 (frequency before
        // tier), the SymSpell candidate's frequency of 5 would have put it
        // ahead of the unlearned exact match.
        let mut candidates = vec![
            Candidate {
                display: "SymSpell学習済み".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 5,
                tier: 1,
                learn_pair: None,
            },
            Candidate {
                display: "JW学習済み".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.99,
                last_selected: 5,
                tier: 2,
                learn_pair: None,
            },
            Candidate {
                display: "完全一致未学習".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.5,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        assert_eq!(candidates[0].display, "完全一致未学習");
        assert_eq!(candidates[1].display, "SymSpell学習済み");
        assert_eq!(candidates[2].display, "JW学習済み");
    }

    #[test]
    fn a_learned_exact_match_outranks_unlearned_exact_matches() {
        // RANK-02 / D-150: inside the exact-match stage, learning still wins,
        // exactly as it did under D-96. This test keeps the original intent
        // of D-96 (a learned candidate wins inside the exact-match group)
        // alive under the new D-144 ordering, as a separate test from the
        // RANK-01 regression above.
        let mut candidates = vec![
            Candidate {
                display: "完全一致未学習".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "完全一致学習済み".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.5,
                last_selected: 3,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "SymSpell未学習".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 1,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        assert_eq!(candidates[0].display, "完全一致学習済み");
        assert_eq!(candidates[1].display, "完全一致未学習");
        assert_eq!(candidates[2].display, "SymSpell未学習");
    }

    #[test]
    fn a_learned_tier_2_candidate_outranks_an_unlearned_tier_1_candidate() {
        // D-146: tier 1 and tier 2 share the same match stage, so a learned
        // tier-2 candidate (JW<1.0) still outranks an unlearned tier-1 one
        // (SymSpell) - frequency is compared before tier. Implementing the
        // stage as a 3-value comparison on `tier` itself (instead of the
        // 2-value `match_stage`) would make this test fail: tier 1 would
        // always sort ahead of tier 2 regardless of frequency (08-RESEARCH.md
        // Pitfall 3).
        let mut candidates = vec![
            Candidate {
                display: "SymSpell未学習".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 1,
                learn_pair: None,
            },
            Candidate {
                display: "JW学習済み".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.95,
                last_selected: 1,
                tier: 2,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        assert_eq!(candidates[0].display, "JW学習済み");
        assert_eq!(candidates[1].display, "SymSpell未学習");
    }

    #[test]
    fn unlearned_fuzzy_candidates_keep_tier_1_before_tier_2() {
        // D-146 / D-148: with no learning on either side (equal frequency),
        // the fuzzy stage falls back to comparing `tier` before `score`, so
        // tier 1 (SymSpell) still comes before tier 2 (JW<1.0) even when
        // tier 2 has the higher score.
        let mut candidates = vec![
            Candidate {
                display: "JW未学習".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.99,
                last_selected: 0,
                tier: 2,
                learn_pair: None,
            },
            Candidate {
                display: "SymSpell未学習".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.5,
                last_selected: 0,
                tier: 1,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        assert_eq!(candidates[0].display, "SymSpell未学習");
        assert_eq!(candidates[1].display, "JW未学習");
    }

    #[test]
    fn a_learned_fuzzy_candidate_leads_the_fuzzy_stage_behind_every_exact_match() {
        // D-145 (a copy of the Ato example from context.rs): a fuzzy
        // candidate learned under another reading (元, tier 1, frequency 1)
        // leads the fuzzy stage - ahead of the unlearned tier-1 candidate
        // (基) - but stays behind every exact match (後/跡, tier 0), no
        // matter how much it has been learned.
        let mut candidates = vec![
            Candidate {
                display: "基".to_string(),
                reading: "あと".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 1,
                learn_pair: None,
            },
            Candidate {
                display: "元".to_string(),
                reading: "あと".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 1,
                tier: 1,
                learn_pair: None,
            },
            Candidate {
                display: "後".to_string(),
                reading: "あと".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: "跡".to_string(),
                reading: "あと".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                last_selected: 0,
                tier: 0,
                learn_pair: None,
            },
        ];
        sort_candidates(&mut candidates, ConversionMode::KanjiConvert);
        let displays: Vec<&str> = candidates.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(displays, vec!["後", "跡", "元", "基"]);
    }

    #[test]
    fn candidates_are_built_from_dictionary_entries() {
        let entries = vec![
            DictEntry::new("漢字"),
            DictEntry::new("感じる").with_last_selected(3),
        ];

        let candidates = build_candidates("かんじ", &entries, 0.9, 0);

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].display, "漢字");
        assert_eq!(candidates[0].reading, "かんじ");
        assert_eq!(candidates[0].kind, CandidateKind::Kanji);
        assert_eq!(candidates[0].score, 0.9);
        assert_eq!(candidates[0].last_selected, 0);

        assert_eq!(candidates[1].display, "感じる");
        assert_eq!(candidates[1].kind, CandidateKind::KanjiWithOkuri);
        assert_eq!(candidates[1].last_selected, 3);
    }

    #[test]
    fn the_hiragana_fallback_candidate_is_built() {
        let candidate = hiragana_candidate("かんじ");

        assert_eq!(candidate.display, "かんじ");
        assert_eq!(candidate.reading, "かんじ");
        assert_eq!(candidate.kind, CandidateKind::Hiragana);
        assert_eq!(candidate.score, 1.0);
        assert_eq!(candidate.last_selected, 0);
    }

    #[test]
    fn the_katakana_candidate_is_built() {
        let candidate = katakana_candidate("かんじ");

        assert_eq!(candidate.display, "カンジ");
        assert_eq!(candidate.reading, "かんじ");
        assert_eq!(candidate.kind, CandidateKind::Katakana);
        assert_eq!(candidate.score, 1.0);
        assert_eq!(candidate.last_selected, 0);
    }

    #[test]
    fn conversion_to_full_width_alphabet() {
        assert_eq!(ascii_to_fullwidth("Kanji"), "Ｋａｎｊｉ");
        assert_eq!(ascii_to_fullwidth("ko-hi-"), "ｋｏ－ｈｉ－");
        assert_eq!(ascii_to_fullwidth("~"), "～");
        assert_eq!(ascii_to_fullwidth(" "), "\u{3000}");
        assert_eq!(ascii_to_fullwidth("あ"), "あ");
    }

    #[test]
    fn the_alphabet_candidates_are_built() {
        let hankaku = alphabet_hankaku_candidate("kanJi");
        assert_eq!(hankaku.display, "kanJi");
        assert_eq!(hankaku.kind, CandidateKind::AlphabetHankaku);
        assert_eq!(hankaku.score, 1.0);
        assert_eq!(hankaku.last_selected, 0);

        let zenkaku = alphabet_zenkaku_candidate("kanJi");
        assert_eq!(zenkaku.display, "ｋａｎＪｉ");
        assert_eq!(zenkaku.kind, CandidateKind::AlphabetZenkaku);
        assert_eq!(zenkaku.score, 1.0);
        assert_eq!(zenkaku.last_selected, 0);
    }

    #[test]
    fn building_candidates_from_an_empty_entry_list() {
        let candidates = build_candidates("かんじ", &[], 0.9, 0);
        assert!(candidates.is_empty());
    }

    #[test]
    fn building_a_candidate_from_an_empty_string() {
        let candidate = hiragana_candidate("");
        assert_eq!(candidate.display, "");

        let candidate = katakana_candidate("");
        assert_eq!(candidate.display, "");
    }

    // === #1/#2/#3 conversion and positional kanji numerals (D-21/D-31; test vectors from upstream emacs/sekka-tests.el:139-216) ===

    #[test]
    fn sharp_number_1_converts_half_width_to_full_width() {
        for (input, expected) in [
            ("1", "１"),
            ("0123456789", "０１２３４５６７８９"),
            (
                "01234567890123456789",
                "０１２３４５６７８９０１２３４５６７８９",
            ),
        ] {
            assert_eq!(sharp_number("#1", input), expected, "input={:?}", input);
        }
    }

    #[test]
    fn sharp_number_2_maps_each_digit_to_a_kanji_numeral() {
        for (input, expected) in [
            ("1", "一"),
            ("5500", "五五〇〇"),
            ("0123456789", "〇一二三四五六七八九"),
        ] {
            assert_eq!(sharp_number("#2", input), expected, "input={:?}", input);
        }
    }

    #[test]
    fn kansuuji_sen_cases() {
        for (input, expected) in [
            ("1", "一"),
            ("10", "十"),
            ("100", "百"),
            ("1000", "千"),
            ("5500", "五千五百"),
            ("5555", "五千五百五十五"),
            ("9999", "九千九百九十九"),
        ] {
            assert_eq!(kansuuji_sen(input), expected, "input={:?}", input);
        }
    }

    #[test]
    fn kansuuji_cases() {
        for (input, expected) in [
            // CR-01: an all-zero input returns 「〇」 rather than an empty string.
            ("0", "〇"),
            ("00", "〇"),
            ("10000", "一万"),
            ("100000000", "一億"),
            ("1000000000", "十億"),
            ("1000000000000", "一兆"),
            ("1000000000002", "一兆二"),
            ("0123456789", "一億二千三百四十五万六千七百八十九"),
            (
                "98765432109876543210",
                "九千八百七十六京五千四百三十二兆千九十八億七千六百五十四万三千二百十",
            ),
        ] {
            assert_eq!(kansuuji(input), expected, "input={:?}", input);
        }
    }

    #[test]
    fn sharp_number_3_produces_positional_kanji_numerals() {
        for (input, expected) in [
            ("5500", "五千五百"),
            ("55555", "五万五千五百五十五"),
            ("0123456789", "一億二千三百四十五万六千七百八十九"),
            // CR-01: an all-zero input becomes 「〇」 (the #3 path of NumberOnly/NumberPrefixed).
            ("0", "〇"),
            ("00", "〇"),
        ] {
            assert_eq!(sharp_number("#3", input), expected, "input={:?}", input);
        }
    }

    #[test]
    fn an_unknown_sharp_number_type_is_left_unconverted() {
        assert_eq!(sharp_number("#0", "2023"), "2023");
    }

    #[test]
    fn kansuuji_includes_goku_at_52_digits_and_does_not_panic_past_53() {
        // The terminus of D-31: 52 digits (exactly the KURAI2 ceiling) becomes
        // positional kanji numerals and includes 「極」. 53 digits (past the end
        // of KURAI2) falls back unconverted and returns the input digits as they
        // are. Neither panics (T-01.3-01).
        let digits_52 = "9".repeat(52);
        let result_52 = sharp_number("#3", &digits_52);
        assert!(
            result_52.contains('極'),
            "52 digits should become positional kanji numerals including 「極」: {:?}",
            result_52
        );

        let digits_53 = "9".repeat(53);
        let result_53 = sharp_number("#3", &digits_53);
        assert_eq!(
            result_53, digits_53,
            "53 digits should fall back unconverted and return the input as it is"
        );
    }

    #[test]
    fn kansuuji_falls_back_at_53_digits_even_when_the_excess_group_is_all_zeros() {
        // WR-01: even when the 53rd digit (past the end of KURAI2, the 13th
        // group) is all zeros, the bound is checked before skipping zeros, so the
        // fallback is unconditional.
        let digits_53_leading_zero = format!("0{}", "9".repeat(52));
        let result = sharp_number("#3", &digits_53_leading_zero);
        assert_eq!(
            result, digits_53_leading_zero,
            "even with a leading 0, 53 digits should fall back unconverted and return the input as it is"
        );
    }
}
