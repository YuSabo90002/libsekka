// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
// SPDX-FileCopyrightText: 2010-2026 Kiyoka Nishiyama <kiyoka@sumibi.org>
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Parts of this file are ported from Sekka (https://github.com/kiyoka/sekka),
// master @ 0f73ee9 (retrieved 2026-09-23): emacs/sekka-henkan.el.

//! Conversion requests and mode detection
//!
//! Analyses the uppercase pattern of romaji input and decides which conversion
//! mode applies.

/// Conversion mode, decided from the uppercase pattern
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionMode {
    /// All lowercase: "kanji" -> "かんじ" (committed as hiragana).
    HiraganaOnly,
    /// Leading uppercase only: "Kanji" -> kanji conversion.
    KanjiConvert,
    /// Lowercase head plus an uppercase letter inside: "kanJi" -> kanji
    /// conversion with okurigana.
    KanjiWithOkuri,
    /// Uppercase head plus an uppercase letter inside or at the end: "OkonaU" ->
    /// kanji conversion with an okurigana marker.
    OkuriMarker,
}

/// Conversion request, holding the result of input analysis
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionRequest {
    /// The raw romaji input.
    pub raw_input: String,
    /// The reading after kana conversion (currently the lowercased romaji; will
    /// be wired to the kana conversion module later).
    pub kana_reading: String,
    /// The conversion mode decided from the uppercase pattern.
    pub mode: ConversionMode,
    /// Byte offset where the okurigana starts.
    pub okuri_position: Option<usize>,
}

/// Analyses raw romaji input and decides the conversion mode from its uppercase
/// pattern.
///
/// # Uppercase pattern rules
///
/// - all lowercase -> `HiraganaOnly`
/// - leading uppercase only -> `KanjiConvert`
/// - uppercase head plus uppercase inside or at the end -> `OkuriMarker`
/// - lowercase head plus uppercase inside -> `KanjiWithOkuri`
///
/// # Arguments
///
/// * `raw_input` - the raw romaji input string
///
/// # Returns
///
/// A `ConversionRequest` holding the analysis result.
pub fn analyze_input(raw_input: &str) -> ConversionRequest {
    if raw_input.is_empty() {
        return ConversionRequest {
            raw_input: String::new(),
            kana_reading: String::new(),
            mode: ConversionMode::HiraganaOnly,
            okuri_position: None,
        };
    }

    let first_char = raw_input.chars().next().unwrap();
    let first_is_upper = first_char.is_ascii_uppercase();

    // Find the first uppercase letter from the second character on (byte offset).
    let second_upper_pos = find_second_uppercase(raw_input);

    let (mode, okuri_position) = match (first_is_upper, second_upper_pos) {
        // lowercase head, no uppercase -> hiragana only
        (false, None) => (ConversionMode::HiraganaOnly, None),
        // uppercase head, no uppercase after it -> kanji conversion
        (true, None) => (ConversionMode::KanjiConvert, None),
        // uppercase head plus uppercase inside or at the end -> okurigana marker
        (true, Some(pos)) => (ConversionMode::OkuriMarker, Some(pos)),
        // lowercase head plus uppercase inside -> kanji conversion with okurigana
        (false, Some(pos)) => (ConversionMode::KanjiWithOkuri, Some(pos)),
    };

    // The kana reading is provisionally the lowercased romaji
    // (the kana converter in romaji.rs will take over later).
    let kana_reading = raw_input.to_ascii_lowercase();

    ConversionRequest {
        raw_input: raw_input.to_string(),
        kana_reading,
        mode,
        okuri_position,
    }
}

/// Input shape - the classification that picks the candidate-building path,
/// decided in the same priority order as the `cond` branches of upstream
/// `sekka-henkan` (D-22)
///
/// The variants carry no payload. The arms for branch 2 (`CaseBased`) and
/// branch 5 (`HiraganaConvertible`) run the existing `analyze_input` /
/// `ConversionMode` code unmodified, which is what guarantees D-24 (do not
/// change existing behaviour). The mode details are still decided by
/// `analyze_input` inside `build_candidate_list`, as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputShape {
    /// Contains at least one ASCII uppercase letter (upstream branch 2; uses the
    /// existing `KanjiConvert` / `KanjiWithOkuri` / `OkuriMarker` paths as they
    /// are).
    CaseBased,
    /// No uppercase, every character is a digit (one or more; upstream branch 3,
    /// D-21).
    NumberOnly,
    /// No uppercase, starts with a digit and every character is one of
    /// `0-9`/`a-z`/`A-Z`/`@`/`;`/`-` (two or more; upstream branch 4, D-21).
    /// This is what `#`-substitution dictionary lookup applies to.
    NumberPrefixed,
    /// No uppercase, every character is convertible through the romaji table
    /// (upstream branch 5; uses the existing `HiraganaOnly` path as it is).
    HiraganaConvertible,
    /// Anything else (contains a symbol, or romaji conversion failed; upstream
    /// branch 6).
    Symbol,
}

/// Classifies the input shape of the romaji buffer (D-22)
///
/// Pinned to the same priority order (first match wins) as the `cond` in
/// upstream `emacs/sekka-henkan.el:365-414`:
///
/// 1. contains at least one ASCII uppercase letter -> `CaseBased`
/// 2. one or more characters, all digits -> `NumberOnly`
/// 3. starts with a digit, two or more characters, every character is one of
///    `0-9`/`a-z`/`A-Z`/`@`/`;`/`-` -> `NumberPrefixed`
/// 4. `is_strictly_convertible` is `true` -> `HiraganaConvertible`
/// 5. anything else -> `Symbol`
///
/// Empty input is `HiraganaConvertible` (matching the current default for empty
/// input). Checking uppercase before digits is mandatory: swap them and input
/// like `2023Nen` slips past `CaseBased` (branch 2) into the number branches by
/// mistake (RESEARCH.md Pitfall 1). Checking all-digits before `NumberPrefixed`
/// is equally mandatory, so that `2023` becomes `NumberOnly` rather than
/// `NumberPrefixed`. Of the symbols D-20 accepts, only `@`, `;` and `-` are in
/// the `NumberPrefixed` character class, so input like `2023.` falls through to
/// `Symbol` (RESEARCH.md Pitfall 2).
pub fn classify_input_shape(raw_input: &str) -> InputShape {
    if raw_input.is_empty() {
        return InputShape::HiraganaConvertible;
    }
    if raw_input.chars().any(|c| c.is_ascii_uppercase()) {
        return InputShape::CaseBased;
    }
    if raw_input.chars().all(|c| c.is_ascii_digit()) {
        return InputShape::NumberOnly;
    }
    let starts_with_digit = raw_input.chars().next().is_some_and(|c| c.is_ascii_digit());
    if starts_with_digit
        && raw_input.chars().count() >= 2
        && raw_input
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_alphabetic() || matches!(c, '@' | ';' | '-'))
    {
        return InputShape::NumberPrefixed;
    }
    if crate::romaji::is_strictly_convertible(raw_input) {
        return InputShape::HiraganaConvertible;
    }
    InputShape::Symbol
}

/// Returns the byte offset of the first uppercase letter from the second
/// character on.
///
/// Returns `None` when there is none.
fn find_second_uppercase(input: &str) -> Option<usize> {
    let mut chars = input.char_indices();
    // Skip the first character.
    chars.next();
    for (byte_pos, ch) in chars {
        if ch.is_ascii_uppercase() {
            return Some(byte_pos);
        }
    }
    None
}

/// Okurigana split - splits the input into its stem and its okurigana.
///
/// When `okuri_position` is `None`, the whole input is returned as the stem.
///
/// # Arguments
///
/// * `raw_input` - the raw romaji input
/// * `okuri_position` - byte offset where the okurigana starts
///
/// # Returns
///
/// A `(stem, okurigana)` tuple; the okurigana is an empty string when there is
/// none. Both parts are returned lowercased.
pub fn split_okuri(raw_input: &str, okuri_position: Option<usize>) -> (String, String) {
    match okuri_position {
        Some(pos) => {
            let stem = raw_input[..pos].to_ascii_lowercase();
            let okuri = raw_input[pos..].to_ascii_lowercase();
            (stem, okuri)
        }
        None => (raw_input.to_ascii_lowercase(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_input_shape_branches() {
        // CaseBased (branch 2): contains at least one ASCII uppercase letter.
        for input in ["Kanji", "kanJi", "OkonaU"] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::CaseBased,
                "input={:?}",
                input
            );
        }
        // HiraganaConvertible (branch 5): no uppercase, every character convertible through the romaji table.
        for input in ["kanji", ""] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::HiraganaConvertible,
                "input={:?}",
                input
            );
        }
        // NumberOnly (branch 3): no uppercase, every character a digit.
        for input in ["2023", "0"] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::NumberOnly,
                "input={:?}",
                input
            );
        }
        // NumberPrefixed (branch 4): starts with a digit, every character in 0-9/a-z/A-Z/@/;/-.
        for input in ["2023nen", "12;34"] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::NumberPrefixed,
                "input={:?}",
                input
            );
        }
        // Symbol (branch 6): anything else.
        for input in [".", ",", "nihongodesu."] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::Symbol,
                "input={:?}",
                input
            );
        }
    }

    #[test]
    fn classify_input_shape_handles_input_containing_a_hyphen() {
        // 02-01: pins the input shape of input containing `-` (part of the
        // character key set of D-20). "ra-men" does not start with a digit, so
        // NumberPrefixed does not apply (that requires starts_with_digit). It
        // contains no uppercase and is not all digits either, so it falls to the
        // HiraganaConvertible decision via is_strictly_convertible ("-" is
        // convertible by rule!("-", "ー") in romaji.rs, so "ra", "-" and "men"
        // all match the romaji table).
        assert_eq!(
            classify_input_shape("ra-men"),
            InputShape::HiraganaConvertible
        );
        // "2023-nen" starts with a digit, has two or more characters and every
        // character is a digit, a letter or one of `@` `;` `-` (this pins that
        // `-` is in the NumberPrefixed character class; implementation fact:
        // `matches!(c, '@' | ';' | '-')`).
        assert_eq!(classify_input_shape("2023-nen"), InputShape::NumberPrefixed);
    }

    #[test]
    fn number_branch_boundaries_pinned_together() {
        // Pins RESEARCH.md Pitfall 1 and 2 side by side in a single test.
        assert_eq!(
            classify_input_shape("2023nen"),
            InputShape::NumberPrefixed,
            "input starting with a digit and staying inside the D-20 character class is NumberPrefixed"
        );
        assert_eq!(
            classify_input_shape("12;34"),
            InputShape::NumberPrefixed,
            "a semicolon is in the NumberPrefixed character class"
        );
        assert_eq!(
            classify_input_shape("2023."),
            InputShape::Symbol,
            "a period is not in the NumberPrefixed character class, so it falls to Symbol (Pitfall 2)"
        );
        assert_eq!(
            classify_input_shape("2023"),
            InputShape::NumberOnly,
            "all digits means NumberOnly (which takes priority over NumberPrefixed)"
        );
        assert_eq!(
            classify_input_shape("2023Nen"),
            InputShape::CaseBased,
            "digit-leading input containing uppercase is CaseBased (Pitfall 1)"
        );
    }

    #[test]
    fn digit_leading_input_with_uppercase_takes_branch_2() {
        // RESEARCH.md Pitfall 1: "2023Nen" contains uppercase, so it takes
        // branch 2 (CaseBased) and never enters the number branches
        // (NumberOnly/NumberPrefixed).
        assert_eq!(classify_input_shape("2023Nen"), InputShape::CaseBased);
    }

    #[test]
    /// All-lowercase input yields HiraganaOnly mode.
    fn test_hiragana_only_kanji() {
        let req = analyze_input("kanji");
        assert_eq!(req.mode, ConversionMode::HiraganaOnly);
        assert_eq!(req.okuri_position, None);
        assert_eq!(req.kana_reading, "kanji");
    }

    #[test]
    /// All-lowercase input "nihongo" also yields HiraganaOnly mode.
    fn test_hiragana_only_nihongo() {
        let req = analyze_input("nihongo");
        assert_eq!(req.mode, ConversionMode::HiraganaOnly);
        assert_eq!(req.okuri_position, None);
        assert_eq!(req.kana_reading, "nihongo");
    }

    #[test]
    /// Leading uppercase only yields KanjiConvert mode.
    fn test_kanji_convert() {
        let req = analyze_input("Kanji");
        assert_eq!(req.mode, ConversionMode::KanjiConvert);
        assert_eq!(req.okuri_position, None);
        assert_eq!(req.kana_reading, "kanji");
    }

    #[test]
    /// Leading uppercase only, "Nihongo", also yields KanjiConvert mode.
    fn test_kanji_convert_nihongo() {
        let req = analyze_input("Nihongo");
        assert_eq!(req.mode, ConversionMode::KanjiConvert);
        assert_eq!(req.okuri_position, None);
        assert_eq!(req.kana_reading, "nihongo");
    }

    #[test]
    /// Lowercase head plus uppercase inside yields KanjiWithOkuri mode.
    fn test_kanji_with_okuri() {
        let req = analyze_input("kanJi");
        assert_eq!(req.mode, ConversionMode::KanjiWithOkuri);
        assert_eq!(req.okuri_position, Some(3));
        assert_eq!(req.raw_input, "kanJi");
        assert_eq!(req.kana_reading, "kanji");
    }

    #[test]
    /// Uppercase head plus uppercase inside or at the end yields OkuriMarker mode.
    fn test_okuri_marker() {
        let req = analyze_input("OkonaU");
        assert_eq!(req.mode, ConversionMode::OkuriMarker);
        assert_eq!(req.okuri_position, Some(5));
        assert_eq!(req.raw_input, "OkonaU");
        assert_eq!(req.kana_reading, "okonau");
    }

    #[test]
    /// Empty input yields HiraganaOnly mode and empty strings.
    fn test_empty_input() {
        let req = analyze_input("");
        assert_eq!(req.mode, ConversionMode::HiraganaOnly);
        assert_eq!(req.okuri_position, None);
        assert_eq!(req.raw_input, "");
        assert_eq!(req.kana_reading, "");
    }

    #[test]
    /// Okurigana split - KanjiWithOkuri mode.
    fn test_split_okuri_with_position() {
        let (stem, okuri) = split_okuri("kanJi", Some(3));
        assert_eq!(stem, "kan");
        assert_eq!(okuri, "ji");
    }

    #[test]
    /// Okurigana split - OkuriMarker mode.
    fn test_split_okuri_marker() {
        let (stem, okuri) = split_okuri("OkonaU", Some(5));
        assert_eq!(stem, "okona");
        assert_eq!(okuri, "u");
    }

    #[test]
    /// With no okurigana, the whole input is the stem.
    fn test_split_okuri_none() {
        let (stem, okuri) = split_okuri("kanji", None);
        assert_eq!(stem, "kanji");
        assert_eq!(okuri, "");
    }

    /// Feeds a romaji string through the converter one character at a time and
    /// returns the kana (what `romaji_to_kana` does for a stem or an okurigana).
    fn kana_of(romaji: &str) -> String {
        let mut converter = crate::romaji::RomajiConverter::new();
        let mut kana = String::new();
        for ch in romaji.chars() {
            kana.extend(converter.feed(ch));
        }
        kana.extend(converter.flush());
        kana
    }

    #[test]
    fn newly_spellable_lowercase_words_move_from_symbol_to_hiragana_convertible() {
        // D-212: the v1.3 table classified these as Symbol; the new rules make them
        // spellable to the end, and D-212 accepts that Ctrl-J now offers hiragana
        // first while the half-width alphabet stays selectable with Ctrl-L.
        for input in [
            "che", "tye", "she", "je", "va", "whi", "thu", "dyi", "java", "live", "video",
        ] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::HiraganaConvertible,
                "input={:?}",
                input
            );
        }
        // These still leave a letter that cannot be spelled at the end (or have no
        // rule), so they stay Symbol.
        for input in ["vim", "vivid", "web", "cat", "the", "type"] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::Symbol,
                "input={:?}",
                input
            );
        }
        // wine was already convertible (wi was a rule before); only its reading
        // changes, from ゐね to うぃね.
        assert_eq!(
            classify_input_shape("wine"),
            InputShape::HiraganaConvertible
        );
    }

    #[test]
    fn okurigana_boundary_splits_a_new_rule_into_stem_and_okuri() {
        // Claude's Discretion (three forms): lowercase, leading capital, and a
        // capital inside the word (okurigana). The rule never crosses the stem /
        // okurigana boundary because each part is converted on its own.
        for (input, stem_kana, okuri_kana) in [
            ("kaTye", "か", "ちぇ"),
            ("kaVa", "か", "\u{3046}\u{309B}\u{3041}"),
        ] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::CaseBased,
                "input={:?}",
                input
            );
            let req = analyze_input(input);
            assert_eq!(
                req.mode,
                ConversionMode::KanjiWithOkuri,
                "input={:?}",
                input
            );
            let (stem, okuri) = split_okuri(input, req.okuri_position);
            assert_eq!(stem, "ka", "input={:?}", input);
            assert_eq!(okuri, input[2..].to_ascii_lowercase(), "input={:?}", input);
            assert_eq!(kana_of(&stem), stem_kana, "input={:?}", input);
            assert_eq!(kana_of(&okuri), okuri_kana, "input={:?}", input);
        }
        // A leading capital alone is KanjiConvert and the whole word is the stem.
        for (input, expected) in [("Tye", "ちぇ"), ("Va", "\u{3046}\u{309B}\u{3041}")] {
            assert_eq!(
                classify_input_shape(input),
                InputShape::CaseBased,
                "input={:?}",
                input
            );
            let req = analyze_input(input);
            assert_eq!(req.mode, ConversionMode::KanjiConvert, "input={:?}", input);
            let (stem, okuri) = split_okuri(input, req.okuri_position);
            assert_eq!(okuri, "", "input={:?}", input);
            assert_eq!(kana_of(&stem), expected, "input={:?}", input);
        }
    }
}
