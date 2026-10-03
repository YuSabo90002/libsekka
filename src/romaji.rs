// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Romaji-to-kana conversion state machine
//!
//! Implements Sekka's own romaji rules, based on the SKK romaji table. Input is
//! fed one character at a time and kana is emitted as soon as a complete rule
//! matches.

use std::collections::HashMap;

/// A romaji conversion rule
///
/// Defines the kana output and the unconsumed input remainder for one romaji
/// input pattern.
#[derive(Debug, Clone)]
struct RomajiRule {
    /// The romaji input pattern (e.g. "ka").
    input: String,
    /// The kana this converts to (e.g. "か").
    output: String,
    /// Unconsumed input (e.g. the next consonant kept when handling a sokuon).
    remaining: String,
}

/// Romaji-to-kana converter
///
/// A state machine that processes input one character at a time and emits kana as
/// soon as a rule matches.
pub struct RomajiConverter {
    /// The current unconverted buffer.
    buffer: String,
    /// The romaji rule table.
    rules: Vec<RomajiRule>,
    /// Map from input pattern to rule, for fast lookup.
    rule_map: HashMap<String, usize>,
}

impl Default for RomajiConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl RomajiConverter {
    /// Creates a new romaji converter
    pub fn new() -> Self {
        let rules = build_romaji_rules();
        let mut rule_map = HashMap::new();
        for (i, rule) in rules.iter().enumerate() {
            rule_map.insert(rule.input.clone(), i);
        }
        RomajiConverter {
            buffer: String::new(),
            rules,
            rule_map,
        }
    }

    /// Feeds one character and returns the list of settled kana strings
    ///
    /// The input character is normalized to lowercase before being appended to
    /// the buffer. Detecting uppercase is the caller's job.
    pub fn feed(&mut self, ch: char) -> Vec<String> {
        let lower = ch.to_ascii_lowercase();
        self.buffer.push(lower);
        self.try_convert()
    }

    /// Forcibly converts and emits whatever unconverted text is left in the buffer
    ///
    /// A lone "n" becomes "ん". Any other unconverted text is emitted as it is.
    pub fn flush(&mut self) -> Vec<String> {
        let mut result = Vec::new();
        if !self.buffer.is_empty() {
            // A lone "n" becomes "ん".
            if self.buffer == "n" {
                result.push("ん".to_string());
            } else {
                // Emit whatever cannot be converted as it is.
                result.push(self.buffer.clone());
            }
            self.buffer.clear();
        }
        result
    }

    /// Resets the converter state
    pub fn reset(&mut self) {
        self.buffer.clear();
    }

    /// Returns a reference to the current unconverted buffer
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Matches the buffer contents against the rules and converts as much as possible
    fn try_convert(&mut self) -> Vec<String> {
        let mut result = Vec::new();

        loop {
            if self.buffer.is_empty() {
                break;
            }

            // Look for an exactly matching rule.
            if let Some(&idx) = self.rule_map.get(&self.buffer) {
                let rule = &self.rules[idx];
                result.push(rule.output.clone());
                let remaining = rule.remaining.clone();
                self.buffer = remaining;
                continue;
            }

            // "n" + consonant (other than "n" or "y") -> emit "ん" and keep the consonant.
            // "n'" -> "ん".
            if self.buffer.starts_with('n') && self.buffer.len() >= 2 {
                let second = self.buffer.chars().nth(1).unwrap();
                if second == '\'' {
                    result.push("ん".to_string());
                    self.buffer = self.buffer[2..].to_string();
                    continue;
                }
                // "n" + vowel, "n" + "y" and "n" + "n" are handled by the rules;
                // an "n" before any other consonant becomes "ん".
                if second != 'a'
                    && second != 'i'
                    && second != 'u'
                    && second != 'e'
                    && second != 'o'
                    && second != 'y'
                    && second != 'n'
                {
                    result.push("ん".to_string());
                    self.buffer = self.buffer[1..].to_string();
                    continue;
                }
            }

            // Sokuon handling: the same consonant repeated (except nn).
            if self.buffer.len() >= 2 {
                let chars: Vec<char> = self.buffer.chars().collect();
                if chars[0] == chars[1]
                    && chars[0] != 'a'
                    && chars[0] != 'i'
                    && chars[0] != 'u'
                    && chars[0] != 'e'
                    && chars[0] != 'o'
                    && chars[0] != 'n'
                {
                    result.push("っ".to_string());
                    self.buffer = self.buffer[1..].to_string();
                    continue;
                }
            }

            // Check whether a match is still possible (prefix match).
            let has_prefix_match = self
                .rules
                .iter()
                .any(|rule| rule.input.starts_with(&self.buffer));

            if has_prefix_match {
                // An exact match is still possible, so wait for more input.
                break;
            }

            // Nothing matches: emit the first character and retry.
            let first_char = self.buffer.remove(0);
            result.push(first_char.to_string());
        }

        result
    }
}

/// Strictly decides whether the entire buffer (down to the last character) is
/// convertible through the romaji table (D-26)
///
/// The exact opposite of `try_convert` (which passes unknown characters through by
/// emitting the first one): a single non-matching character makes this return
/// `false` immediately (the equivalent of the "err" case in upstream
/// `sekka--roman->hiragana-with-hash`). Sokuon and "ん" handling trace the same
/// logic as `try_convert`. Input that ends with a lone trailing `n` counts as
/// `true` (because `flush` will emit `ん`).
///
/// This function is used only for the branch decision in
/// `classify_input_shape`. Building dictionary keys (where the symbol branch
/// passes `.` through as `.`) still uses `try_convert` (via `romaji_to_kana`).
/// `try_convert` itself is unchanged.
pub fn is_strictly_convertible(input: &str) -> bool {
    let conv = RomajiConverter::new();
    let mut buffer = String::new();

    for ch in input.chars() {
        buffer.push(ch.to_ascii_lowercase());

        loop {
            if buffer.is_empty() {
                break;
            }

            if let Some(&idx) = conv.rule_map.get(&buffer) {
                buffer = conv.rules[idx].remaining.clone();
                continue;
            }

            // "n" + consonant (other than "n" or "y") -> "ん" and keep the consonant. "n'" -> "ん".
            if buffer.starts_with('n') && buffer.len() >= 2 {
                let second = buffer.chars().nth(1).unwrap();
                if second == '\'' {
                    buffer = buffer[2..].to_string();
                    continue;
                }
                if !matches!(second, 'a' | 'i' | 'u' | 'e' | 'o' | 'y' | 'n') {
                    buffer = buffer[1..].to_string();
                    continue;
                }
            }

            // Sokuon handling: the same consonant repeated (except nn).
            if buffer.len() >= 2 {
                let chars: Vec<char> = buffer.chars().collect();
                if chars[0] == chars[1] && !matches!(chars[0], 'a' | 'i' | 'u' | 'e' | 'o' | 'n') {
                    buffer = buffer[1..].to_string();
                    continue;
                }
            }

            // Check whether a match is still possible (prefix match).
            let has_prefix_match = conv
                .rules
                .iter()
                .any(|rule| rule.input.starts_with(buffer.as_str()));
            if has_prefix_match {
                break;
            }

            // try_convert would pass the character through here; the strict check returns false.
            return false;
        }
    }

    buffer.is_empty() || buffer == "n"
}

/// Builds the full romaji rule table
fn build_romaji_rules() -> Vec<RomajiRule> {
    let mut rules = Vec::new();

    /// Helper macro that adds one rule
    macro_rules! rule {
        ($input:expr, $output:expr) => {
            rules.push(RomajiRule {
                input: $input.to_string(),
                output: $output.to_string(),
                remaining: String::new(),
            });
        };
        ($input:expr, $output:expr, $remaining:expr) => {
            rules.push(RomajiRule {
                input: $input.to_string(),
                output: $output.to_string(),
                remaining: $remaining.to_string(),
            });
        };
    }

    // === Vowels ===
    rule!("a", "あ");
    rule!("i", "い");
    rule!("u", "う");
    rule!("e", "え");
    rule!("o", "お");

    // === KA row ===
    rule!("ka", "か");
    rule!("ki", "き");
    rule!("ku", "く");
    rule!("ke", "け");
    rule!("ko", "こ");

    // === SA row ===
    rule!("sa", "さ");
    rule!("si", "し");
    rule!("shi", "し");
    rule!("su", "す");
    rule!("se", "せ");
    rule!("so", "そ");

    // === TA row ===
    rule!("ta", "た");
    rule!("ti", "ち");
    rule!("chi", "ち");
    rule!("tu", "つ");
    rule!("tsu", "つ");
    rule!("te", "て");
    rule!("to", "と");

    // === NA row ===
    rule!("na", "な");
    rule!("ni", "に");
    rule!("nu", "ぬ");
    rule!("ne", "ね");
    rule!("no", "の");

    // === HA row ===
    rule!("ha", "は");
    rule!("hi", "ひ");
    rule!("hu", "ふ");
    rule!("fu", "ふ");
    rule!("he", "へ");
    rule!("ho", "ほ");

    // === MA row ===
    rule!("ma", "ま");
    rule!("mi", "み");
    rule!("mu", "む");
    rule!("me", "め");
    rule!("mo", "も");

    // === YA row ===
    rule!("ya", "や");
    rule!("yu", "ゆ");
    rule!("yo", "よ");

    // === RA row ===
    rule!("ra", "ら");
    rule!("ri", "り");
    rule!("ru", "る");
    rule!("re", "れ");
    rule!("ro", "ろ");

    // === WA row ===
    rule!("wa", "わ");
    rule!("wi", "ゐ");
    rule!("we", "ゑ");
    rule!("wo", "を");

    // === N ===
    rule!("nn", "ん");

    // === Voiced: GA row ===
    rule!("ga", "が");
    rule!("gi", "ぎ");
    rule!("gu", "ぐ");
    rule!("ge", "げ");
    rule!("go", "ご");

    // === Voiced: ZA row ===
    rule!("za", "ざ");
    rule!("zi", "じ");
    rule!("ji", "じ");
    rule!("zu", "ず");
    rule!("ze", "ぜ");
    rule!("zo", "ぞ");

    // === Voiced: DA row ===
    rule!("da", "だ");
    rule!("di", "ぢ");
    rule!("du", "づ");
    rule!("de", "で");
    rule!("do", "ど");

    // === Voiced: BA row ===
    rule!("ba", "ば");
    rule!("bi", "び");
    rule!("bu", "ぶ");
    rule!("be", "べ");
    rule!("bo", "ぼ");

    // === Semi-voiced: PA row ===
    rule!("pa", "ぱ");
    rule!("pi", "ぴ");
    rule!("pu", "ぷ");
    rule!("pe", "ぺ");
    rule!("po", "ぽ");

    // === Youon: KYA row ===
    rule!("kya", "きゃ");
    rule!("kyu", "きゅ");
    rule!("kyo", "きょ");

    // === Youon: SHA row ===
    rule!("sha", "しゃ");
    rule!("shu", "しゅ");
    rule!("sho", "しょ");
    rule!("sya", "しゃ");
    rule!("syu", "しゅ");
    rule!("syo", "しょ");

    // === Youon: CHA row ===
    rule!("cha", "ちゃ");
    rule!("chu", "ちゅ");
    rule!("cho", "ちょ");
    rule!("tya", "ちゃ");
    rule!("tyu", "ちゅ");
    rule!("tyo", "ちょ");

    // === Youon: JA row ===
    rule!("ja", "じゃ");
    rule!("ju", "じゅ");
    rule!("jo", "じょ");
    rule!("jya", "じゃ");
    rule!("jyu", "じゅ");
    rule!("jyo", "じょ");
    rule!("zya", "じゃ");
    rule!("zyu", "じゅ");
    rule!("zyo", "じょ");

    // === Youon: NYA row ===
    rule!("nya", "にゃ");
    rule!("nyu", "にゅ");
    rule!("nyo", "にょ");

    // === Youon: HYA row ===
    rule!("hya", "ひゃ");
    rule!("hyu", "ひゅ");
    rule!("hyo", "ひょ");

    // === Youon: MYA row ===
    rule!("mya", "みゃ");
    rule!("myu", "みゅ");
    rule!("myo", "みょ");

    // === Youon: RYA row ===
    rule!("rya", "りゃ");
    rule!("ryu", "りゅ");
    rule!("ryo", "りょ");

    // === Youon: GYA row ===
    rule!("gya", "ぎゃ");
    rule!("gyu", "ぎゅ");
    rule!("gyo", "ぎょ");

    // === Youon: BYA row ===
    rule!("bya", "びゃ");
    rule!("byu", "びゅ");
    rule!("byo", "びょ");

    // === Youon: PYA row ===
    rule!("pya", "ぴゃ");
    rule!("pyu", "ぴゅ");
    rule!("pyo", "ぴょ");

    // === V row: U+3046 U+309B, the spacing voiced sound mark after u (D-207) ===
    // Never U+3094 or U+3099: the master dictionary headings use U+3046 U+309B.
    // The outputs are written as escapes because the three forms look alike.
    rule!("vu", "\u{3046}\u{309B}");
    rule!("va", "\u{3046}\u{309B}\u{3041}");
    rule!("vi", "\u{3046}\u{309B}\u{3043}");
    rule!("ve", "\u{3046}\u{309B}\u{3047}");
    rule!("vo", "\u{3046}\u{309B}\u{3049}");

    // === Small kana (x/l prefix) ===
    rule!("xa", "ぁ");
    rule!("xi", "ぃ");
    rule!("xu", "ぅ");
    rule!("xe", "ぇ");
    rule!("xo", "ぉ");
    rule!("la", "ぁ");
    rule!("li", "ぃ");
    rule!("lu", "ぅ");
    rule!("le", "ぇ");
    rule!("lo", "ぉ");
    rule!("xya", "ゃ");
    rule!("xyu", "ゅ");
    rule!("xyo", "ょ");
    rule!("xtu", "っ");
    rule!("ltu", "っ");
    rule!("xtsu", "っ");
    rule!("ltsu", "っ");
    rule!("xwa", "ゎ");

    // === Special combinations ===
    rule!("fa", "ふぁ");
    rule!("fi", "ふぃ");
    rule!("fe", "ふぇ");
    rule!("fo", "ふぉ");

    rule!("dya", "ぢゃ");
    rule!("dyu", "ぢゅ");
    rule!("dyo", "ぢょ");

    rule!("thi", "てぃ");
    rule!("dhi", "でぃ");

    // === Symbols ===
    rule!("-", "ー");

    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that feeds a whole string character by character and joins the result
    fn convert(input: &str) -> String {
        let mut converter = RomajiConverter::new();
        let mut result = String::new();
        for ch in input.chars() {
            for s in converter.feed(ch) {
                result.push_str(&s);
            }
        }
        for s in converter.flush() {
            result.push_str(&s);
        }
        result
    }

    // The 151 rules of the v1.3 table (commit 087c7a0), generated from the source
    // rather than written by hand. For that table convert(input) equals the rule
    // output for every entry, so this is the pre-change convert() snapshot (D-210).
    const LEGACY_RULES: [(&str, &str); 151] = [
        ("a", "あ"),
        ("i", "い"),
        ("u", "う"),
        ("e", "え"),
        ("o", "お"),
        ("ka", "か"),
        ("ki", "き"),
        ("ku", "く"),
        ("ke", "け"),
        ("ko", "こ"),
        ("sa", "さ"),
        ("si", "し"),
        ("shi", "し"),
        ("su", "す"),
        ("se", "せ"),
        ("so", "そ"),
        ("ta", "た"),
        ("ti", "ち"),
        ("chi", "ち"),
        ("tu", "つ"),
        ("tsu", "つ"),
        ("te", "て"),
        ("to", "と"),
        ("na", "な"),
        ("ni", "に"),
        ("nu", "ぬ"),
        ("ne", "ね"),
        ("no", "の"),
        ("ha", "は"),
        ("hi", "ひ"),
        ("hu", "ふ"),
        ("fu", "ふ"),
        ("he", "へ"),
        ("ho", "ほ"),
        ("ma", "ま"),
        ("mi", "み"),
        ("mu", "む"),
        ("me", "め"),
        ("mo", "も"),
        ("ya", "や"),
        ("yu", "ゆ"),
        ("yo", "よ"),
        ("ra", "ら"),
        ("ri", "り"),
        ("ru", "る"),
        ("re", "れ"),
        ("ro", "ろ"),
        ("wa", "わ"),
        ("wi", "ゐ"),
        ("we", "ゑ"),
        ("wo", "を"),
        ("nn", "ん"),
        ("ga", "が"),
        ("gi", "ぎ"),
        ("gu", "ぐ"),
        ("ge", "げ"),
        ("go", "ご"),
        ("za", "ざ"),
        ("zi", "じ"),
        ("ji", "じ"),
        ("zu", "ず"),
        ("ze", "ぜ"),
        ("zo", "ぞ"),
        ("da", "だ"),
        ("di", "ぢ"),
        ("du", "づ"),
        ("de", "で"),
        ("do", "ど"),
        ("ba", "ば"),
        ("bi", "び"),
        ("bu", "ぶ"),
        ("be", "べ"),
        ("bo", "ぼ"),
        ("pa", "ぱ"),
        ("pi", "ぴ"),
        ("pu", "ぷ"),
        ("pe", "ぺ"),
        ("po", "ぽ"),
        ("kya", "きゃ"),
        ("kyu", "きゅ"),
        ("kyo", "きょ"),
        ("sha", "しゃ"),
        ("shu", "しゅ"),
        ("sho", "しょ"),
        ("sya", "しゃ"),
        ("syu", "しゅ"),
        ("syo", "しょ"),
        ("cha", "ちゃ"),
        ("chu", "ちゅ"),
        ("cho", "ちょ"),
        ("tya", "ちゃ"),
        ("tyu", "ちゅ"),
        ("tyo", "ちょ"),
        ("ja", "じゃ"),
        ("ju", "じゅ"),
        ("jo", "じょ"),
        ("jya", "じゃ"),
        ("jyu", "じゅ"),
        ("jyo", "じょ"),
        ("zya", "じゃ"),
        ("zyu", "じゅ"),
        ("zyo", "じょ"),
        ("nya", "にゃ"),
        ("nyu", "にゅ"),
        ("nyo", "にょ"),
        ("hya", "ひゃ"),
        ("hyu", "ひゅ"),
        ("hyo", "ひょ"),
        ("mya", "みゃ"),
        ("myu", "みゅ"),
        ("myo", "みょ"),
        ("rya", "りゃ"),
        ("ryu", "りゅ"),
        ("ryo", "りょ"),
        ("gya", "ぎゃ"),
        ("gyu", "ぎゅ"),
        ("gyo", "ぎょ"),
        ("bya", "びゃ"),
        ("byu", "びゅ"),
        ("byo", "びょ"),
        ("pya", "ぴゃ"),
        ("pyu", "ぴゅ"),
        ("pyo", "ぴょ"),
        ("xa", "ぁ"),
        ("xi", "ぃ"),
        ("xu", "ぅ"),
        ("xe", "ぇ"),
        ("xo", "ぉ"),
        ("la", "ぁ"),
        ("li", "ぃ"),
        ("lu", "ぅ"),
        ("le", "ぇ"),
        ("lo", "ぉ"),
        ("xya", "ゃ"),
        ("xyu", "ゅ"),
        ("xyo", "ょ"),
        ("xtu", "っ"),
        ("ltu", "っ"),
        ("xtsu", "っ"),
        ("ltsu", "っ"),
        ("xwa", "ゎ"),
        ("fa", "ふぁ"),
        ("fi", "ふぃ"),
        ("fe", "ふぇ"),
        ("fo", "ふぉ"),
        ("dya", "ぢゃ"),
        ("dyu", "ぢゅ"),
        ("dyo", "ぢょ"),
        ("thi", "てぃ"),
        ("dhi", "でぃ"),
        ("-", "ー"),
    ];

    // Inputs whose output a later decision changed on purpose. Every input here
    // must also be in LEGACY_RULES.
    const CHANGED_RULES: [(&str, &str); 0] = [];

    #[test]
    fn is_strictly_convertible_boundary_cases() {
        // D-26: the strict decision of whether the whole buffer is convertible
        // through the romaji table. Unlike try_convert (which passes unknown
        // characters through), a single non-matching character returns false.
        for (input, expected) in [
            ("nihongodesu", true),
            ("kanji", true),
            ("kon", true), // a lone trailing "n" is allowed (flush emits "ん")
            ("kka", true), // sokuon
            ("nihongodesu.", false),
            (".", false),
            ("2023", false), // digits are not in the romaji table
        ] {
            assert_eq!(
                is_strictly_convertible(input),
                expected,
                "input={:?}",
                input
            );
        }
    }

    #[test]
    fn convert_turns_a_hyphen_into_the_long_vowel_mark() {
        // 02-01: pins that `-` (part of the character key set of D-20) converts to
        // the long vowel mark `ー` through `rule!("-", "ー")`. `ra` -> `ら`,
        // `-` -> `ー`, `men` -> `めん` (flush emits `ん` for the lone trailing `n`)
        // gives `らーめん`.
        assert_eq!(convert("ra-men"), "らーめん");
    }

    #[test]
    fn test_basic_vowels() {
        // Basic vowel conversion.
        assert_eq!(convert("a"), "あ");
        assert_eq!(convert("i"), "い");
        assert_eq!(convert("u"), "う");
        assert_eq!(convert("e"), "え");
        assert_eq!(convert("o"), "お");
    }

    #[test]
    fn test_basic_hiragana() {
        // Basic hiragana conversion.
        assert_eq!(convert("ka"), "か");
        assert_eq!(convert("ki"), "き");
        assert_eq!(convert("ku"), "く");
        assert_eq!(convert("ke"), "け");
        assert_eq!(convert("ko"), "こ");
    }

    #[test]
    fn test_shi_chi_tsu() {
        // Multiple spellings of shi, chi and tsu.
        assert_eq!(convert("shi"), "し");
        assert_eq!(convert("si"), "し");
        assert_eq!(convert("chi"), "ち");
        assert_eq!(convert("ti"), "ち");
        assert_eq!(convert("tsu"), "つ");
        assert_eq!(convert("tu"), "つ");
    }

    #[test]
    fn test_sokuon() {
        // Sokuon (っ): a repeated consonant.
        assert_eq!(convert("kka"), "っか");
        assert_eq!(convert("tta"), "った");
        assert_eq!(convert("ssa"), "っさ");
        assert_eq!(convert("ppa"), "っぱ");
    }

    #[test]
    fn test_n_handling() {
        // Handling of "ん".
        assert_eq!(convert("nn"), "ん");
        assert_eq!(convert("n'"), "ん");
        assert_eq!(convert("na"), "な");
        assert_eq!(convert("ni"), "に");
    }

    #[test]
    fn test_n_before_consonant() {
        // An "n" before a consonant becomes "ん".
        assert_eq!(convert("nka"), "んか");
        assert_eq!(convert("nta"), "んた");
        assert_eq!(convert("nda"), "んだ");
    }

    #[test]
    fn test_combination_kana() {
        // Youon.
        assert_eq!(convert("kya"), "きゃ");
        assert_eq!(convert("kyu"), "きゅ");
        assert_eq!(convert("kyo"), "きょ");
        assert_eq!(convert("sha"), "しゃ");
        assert_eq!(convert("shu"), "しゅ");
        assert_eq!(convert("sho"), "しょ");
        assert_eq!(convert("cha"), "ちゃ");
        assert_eq!(convert("chu"), "ちゅ");
        assert_eq!(convert("cho"), "ちょ");
        assert_eq!(convert("ja"), "じゃ");
        assert_eq!(convert("ju"), "じゅ");
        assert_eq!(convert("jo"), "じょ");
    }

    #[test]
    fn test_small_kana() {
        // Small kana.
        assert_eq!(convert("xa"), "ぁ");
        assert_eq!(convert("xi"), "ぃ");
        assert_eq!(convert("xu"), "ぅ");
        assert_eq!(convert("xe"), "ぇ");
        assert_eq!(convert("xo"), "ぉ");
        assert_eq!(convert("xtu"), "っ");
        assert_eq!(convert("ltu"), "っ");
        assert_eq!(convert("xya"), "ゃ");
        assert_eq!(convert("xyu"), "ゅ");
        assert_eq!(convert("xyo"), "ょ");
    }

    #[test]
    fn test_voiced_kana() {
        // Voiced kana.
        assert_eq!(convert("ga"), "が");
        assert_eq!(convert("za"), "ざ");
        assert_eq!(convert("da"), "だ");
        assert_eq!(convert("ba"), "ば");
        assert_eq!(convert("pa"), "ぱ");
    }

    #[test]
    fn test_long_string() {
        // Continuous conversion of a long string.
        assert_eq!(convert("konnnichiha"), "こんにちは");
        assert_eq!(convert("toukyou"), "とうきょう");
        assert_eq!(convert("gakkou"), "がっこう");
    }

    #[test]
    fn test_n_flush() {
        // A lone "n" becomes "ん" on flush.
        let mut converter = RomajiConverter::new();
        let fed: Vec<String> = converter.feed('n');
        assert!(fed.is_empty()); // no output yet
        let flushed = converter.flush();
        assert_eq!(flushed, vec!["ん".to_string()]);
    }

    #[test]
    fn test_buffer() {
        // Buffer state.
        let mut converter = RomajiConverter::new();
        assert_eq!(converter.buffer(), "");
        converter.feed('k');
        assert_eq!(converter.buffer(), "k");
        converter.feed('a');
        assert_eq!(converter.buffer(), ""); // "ka" -> "か" was emitted and the buffer cleared
    }

    #[test]
    fn test_reset() {
        // Reset.
        let mut converter = RomajiConverter::new();
        converter.feed('k');
        assert_eq!(converter.buffer(), "k");
        converter.reset();
        assert_eq!(converter.buffer(), "");
    }

    #[test]
    fn test_uppercase_lowered() {
        // Uppercase input is normalized to lowercase.
        assert_eq!(convert("KA"), "か");
        assert_eq!(convert("Ka"), "か");
    }

    #[test]
    fn test_chouon() {
        // The long vowel mark.
        assert_eq!(convert("-"), "ー");
    }

    #[test]
    fn test_nya_nyu_nyo() {
        // The NYA row (checks that "n" + "y" is handled correctly).
        assert_eq!(convert("nya"), "にゃ");
        assert_eq!(convert("nyu"), "にゅ");
        assert_eq!(convert("nyo"), "にょ");
    }

    #[test]
    fn test_fu_hu() {
        // Multiple spellings of fu.
        assert_eq!(convert("fu"), "ふ");
        assert_eq!(convert("hu"), "ふ");
    }

    #[test]
    fn legacy_rules_keep_their_outputs_except_wi_and_we() {
        // D-210: every rule of the v1.3 table still converts the same way, apart
        // from the inputs listed in CHANGED_RULES. The list cannot grow silently:
        // each of its inputs must be a legacy input.
        for (input, _) in CHANGED_RULES {
            assert!(
                LEGACY_RULES
                    .iter()
                    .any(|(legacy_input, _)| *legacy_input == input),
                "CHANGED_RULES input is not a legacy input: {:?}",
                input
            );
        }
        for (input, legacy) in LEGACY_RULES {
            let expected = CHANGED_RULES
                .iter()
                .find(|(changed_input, _)| *changed_input == input)
                .map_or(legacy, |(_, changed)| *changed);
            assert_eq!(convert(input), expected, "input={:?}", input);
            assert!(
                is_strictly_convertible(input),
                "legacy input must stay strictly convertible: input={:?}",
                input
            );
        }
    }

    #[test]
    fn every_rule_input_is_unique_so_rule_map_never_drops_a_rule() {
        let conv = RomajiConverter::new();
        assert_eq!(
            conv.rule_map.len(),
            conv.rules.len(),
            "rule_map is built with HashMap::insert, so a duplicated input would \
             silently drop the earlier rule (D-209)"
        );
        // 151 (v1.3 table) + 5 (V row, D-205)
        assert_eq!(conv.rules.len(), 156);
    }

    #[test]
    fn v_row_outputs_use_u3046_u309b_and_never_u3094_or_u3099() {
        // D-207: the hiragana form is u (U+3046) + the spacing voiced mark (U+309B).
        assert_eq!(convert("vu"), "\u{3046}\u{309B}");
        assert_eq!(convert("va"), "\u{3046}\u{309B}\u{3041}");
        assert_eq!(convert("vi"), "\u{3046}\u{309B}\u{3043}");
        assert_eq!(convert("ve"), "\u{3046}\u{309B}\u{3047}");
        assert_eq!(convert("vo"), "\u{3046}\u{309B}\u{3049}");
        for input in ["vu", "va", "vi", "ve", "vo"] {
            let out = convert(input);
            assert!(
                !out.contains('\u{3094}') && !out.contains('\u{3099}'),
                "input={:?} out={:?}",
                input,
                out
            );
            assert!(
                out.starts_with("\u{3046}\u{309B}"),
                "input={:?} out={:?}",
                input,
                out
            );
        }
        // Uppercase input is lowered by feed, so the same rules apply.
        assert_eq!(convert("Va"), convert("va"));
        assert_eq!(convert("Vu"), convert("vu"));
    }
}
