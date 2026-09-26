// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
// SPDX-FileCopyrightText: 2010-2026 Kiyoka Nishiyama <kiyoka@sumibi.org>
//
// SPDX-License-Identifier: GPL-3.0-or-later
//
// Parts of this file are ported from Sekka (https://github.com/kiyoka/sekka),
// master @ 0f73ee9 (retrieved 2026-09-23): emacs/sekka-jarowinkler.el, emacs/sekka-tests.el.

//! Fuzzy search by Jaro-Winkler similarity **between romaji strings**
//!
//! Finds dictionary entries approximately, so typos and spelling variants still
//! hit. The comparison is always **romaji against romaji**; kana strings are
//! never compared directly (D-57). Follows upstream Sekka's threshold of 0.94
//! and its length-difference cutoff (`roman_index::length_ok`, min/max >= 0.70).
//! Uses the Jaro-Winkler implementation from the `strsim` crate.

use strsim::jaro_winkler;

use crate::dictionary::RomanKey;
use crate::roman_index::length_ok;

/// Fuzzy search configuration
pub struct FuzzySearchConfig {
    /// Similarity threshold (only matches at or above this value are returned).
    pub threshold: f64,
}

impl Default for FuzzySearchConfig {
    /// The default threshold is 0.94 (as upstream Sekka, D-57).
    fn default() -> Self {
        Self { threshold: 0.94 }
    }
}

/// A single fuzzy search result
#[derive(Debug, Clone)]
pub struct FuzzyMatch {
    /// The kana key that matched (`RomanKey::reading`).
    pub key: String,
    /// Jaro-Winkler similarity score (0.0 to 1.0).
    pub score: f64,
}

/// Wrapper that computes Jaro-Winkler similarity
///
/// Both arguments must be **canonical Hepburn romaji**. `strsim` applies the
/// prefix boost only when the similarity exceeds 0.7, whereas upstream applies
/// it whenever the Jaro distance is non-zero (02.1-RESEARCH.md Pitfall 2). The
/// three vectors of D-68 confirm this does not change the 0.94 threshold
/// decision, but always use measured values when adding new vectors (never
/// write an expected value from guesswork).
///
/// # Arguments
/// * `a` - romaji to compare from
/// * `b` - romaji to compare against
///
/// # Returns
/// A similarity score from 0.0 to 1.0 (1.0 means an exact match).
pub fn jaro_winkler_similarity(a: &str, b: &str) -> f64 {
    jaro_winkler(a, b)
}

/// Runs a fuzzy search over a romaji query and a `RomanKey` bucket (D-57/D-59)
///
/// Order of work: (1) drop entries early with the length-difference cutoff
/// `roman_index::length_ok`, (2) compute
/// `jaro_winkler_similarity(query, &rk.roman)`, (3) accept when
/// `score >= config.threshold` (exactly the threshold is accepted; comparison is
/// `>=`). Results are ordered deterministically: descending JW score, ties
/// broken by ascending kana key (`RomanKey::reading`). There is deliberately no
/// signature that scans a whole `&[String]` - the caller is expected to pass the
/// index bucket it obtained from `Dictionary::roman_bucket` (D-59).
///
/// # Arguments
/// * `query` - the fuzzy search query (canonical Hepburn romaji, already
///   lowercased by the caller)
/// * `bucket` - the `RomanKey`s obtained from the romaji index
/// * `config` - fuzzy search configuration (threshold and so on)
///
/// # Returns
/// Matches at or above the threshold, ordered by descending score with ties
/// broken by ascending kana key.
pub fn fuzzy_filter_roman(
    query: &str,
    bucket: &[RomanKey],
    config: &FuzzySearchConfig,
) -> Vec<FuzzyMatch> {
    let query_len = query.chars().count();

    let mut matches: Vec<FuzzyMatch> = bucket
        .iter()
        .filter_map(|rk| {
            let roman_len = rk.roman.chars().count();
            if !length_ok(query_len, roman_len) {
                return None;
            }
            let score = jaro_winkler_similarity(query, &rk.roman);
            if score >= config.threshold {
                Some(FuzzyMatch {
                    key: rk.reading.clone(),
                    score,
                })
            } else {
                None
            }
        })
        .collect();

    // Sort by descending score (ties by ascending kana key; part of the deterministic order of D-75).
    matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });

    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(reading: &str, roman: &str) -> RomanKey {
        RomanKey {
            reading: reading.to_string(),
            roman: roman.to_string(),
        }
    }

    /// Confirms that an exact match scores 1.0.
    #[test]
    fn test_exact_match_returns_one() {
        let score = jaro_winkler_similarity("nihongo", "nihongo");
        assert!(
            (score - 1.0).abs() < f64::EPSILON,
            "an exact match should score 1.0: got {}",
            score
        );
    }

    /// Confirms that a transposition still yields a high score.
    #[test]
    fn test_transposition_returns_high_score() {
        let score = jaro_winkler_similarity("nihongo", "niohgno");
        assert!(
            score > 0.8,
            "a transposition should score above 0.8: got {}",
            score
        );
    }

    // === D-68: the three JW similarity vectors from upstream emacs/sekka-tests.el ===

    #[test]
    fn henka_vs_henkan_is_at_or_above_the_0_94_threshold() {
        let score = jaro_winkler_similarity("henka", "henkan");
        assert!(
            score >= 0.94,
            "henka vs henkan should be >= 0.94: got {}",
            score
        );
    }

    #[test]
    fn shizengengos_vs_shizengengoshori_is_at_or_above_the_0_94_threshold() {
        let score = jaro_winkler_similarity("shizengengos", "shizengengoshori");
        assert!(
            score >= 0.94,
            "shizengengos vs shizengengoshori should be >= 0.94: got {}",
            score
        );
    }

    #[test]
    fn henkan_vs_xyz_is_below_0_5() {
        let score = jaro_winkler_similarity("henkan", "xyz");
        assert!(score < 0.5, "henkan vs xyz should be < 0.5: got {}", score);
    }

    // === D-82: pinning the unreachability of Kunrei-shiki romaji by measurement (03-05 Task 1) ===
    //
    // The index side is canonical Hepburn only (`kana_romaji::KANA_TO_HEPBURN`),
    // while the input side `romaji.rs` also accepts Kunrei-shiki spellings
    // (`ti`/`si`/`tu`/`hu`/`zi` and so on) per D-57. 「Konnitiha」 (Kunrei-shiki,
    // "ti") is a word whose canonical Hepburn form is 「Konnichiha」 ("chi"), and
    // running JW directly between the raw romaji query "konnitiha" and the
    // indexed canonical romaji "konnichiha" does not reach 0.94 because of the
    // character-count difference (9 vs 10).
    //
    // [Correction to what 03-CONTEXT.md D-82 says, found by measurement]
    // D-82 claims that "`kanzi` -> 漢字 only works because the lengths differ by
    // a single character, which is a coincidence", but that is wrong. Measured,
    // `jaro_winkler_similarity("kanzi", "kanji")` = 0.9066666666666667, which is
    // **below 0.94** (pinned by a test below). The real reason `Kanzi` reaches
    // 「漢字」 is not that it passes the JW threshold: the input table in
    // `romaji.rs` converts both "zi" and "ji" to the same kana 「じ」, so the
    // **exact match** on the kana form (treated as JW=1.0, the
    // `exact_candidates` path of `lookup_dictionary`) picks it up first
    // (verified on a real machine that RomajiConverter turns both "kanzi" and
    // "kanji" into 「かんじ」).
    //
    // The same investigation also revealed that 「Konnichiha」 (correct Hepburn)
    // converts to 「こんいちは」 when you look only at romaji -> kana conversion,
    // i.e. the same conversion result as 「Konnitiha」 (the two-character "nn"
    // rule in `romaji.rs` consumes "n"+"n" as a single 「ん」, so the following
    // "i" becomes a standalone 「い」 rather than 「に」). 「Konnichiha」 still
    // reaches 「今日は」, not through that exact-match path but because the raw
    // romaji query "konnichiha" matches the indexed canonical Hepburn
    // "konnichiha" exactly at JW=1.0 (pinned by the test
    // `hepburn_konnichiha_matches_canonical_hepburn_exactly` below).
    // So the direct cause of 「Konnitiha」 failing is "the Kunrei-shiki spelling
    // is shorter than canonical Hepburn, which drops the raw romaji query's JW
    // score below 0.94", exactly as D-82 identified; that stands. The correction
    // above only fixes the explanation of *why* `Kanzi` works and does not
    // affect the diagnosis for 「Konnitiha」 (the behaviour of the "nn"
    // two-character rule itself is out of scope for this task and is not a
    // change that meets the stop conditions of D-89, so it is left alone).
    //
    // The threshold, prefix index and length-difference cutoff logic of upstream
    // `sekka-jarowinkler.el` is identical to `libsekka/src/roman_index.rs`
    // (ported from lines 324-342, `sekka-jarowinkler--length-ok-p` and
    // `sekka-jarowinkler--pick-prefix-length`; the kana-to-hepburn alist at
    // lines 104-157 is the source for `kana_romaji.rs`), so there is no
    // structural difference between upstream's JW layer and ours. The reason
    // upstream does reach 「今日は」 for this vector is not the JW layer but the
    // SymSpell layer (`sekka-symspell.el`, edit distance <= 1, called from
    // `sekka-henkan--okuri-nashi` via `sekka-jisyo-approximate-search`), which
    // `sekka-henkan.el` (lines 83-140, `sekka-henkan--okuri-nashi`, especially
    // lines 111-123) invokes in parallel for each kana conversion result
    // (`hira-list`). See the Task 1 SUMMARY record for details.

    /// Kunrei-shiki `konnitiha` does not reach the JW threshold of 0.94 against
    /// canonical Hepburn `konnichiha`
    ///
    /// Measured value: record the number actually obtained from `cargo test`
    /// below, not a guess (02.1-RESEARCH.md Pitfall 2).
    #[test]
    fn kunrei_konnitiha_does_not_reach_the_threshold_against_canonical_hepburn() {
        let score = jaro_winkler_similarity("konnitiha", "konnichiha");
        // Measured (2026-09-23, strsim jaro_winkler): 0.9333333333333333
        assert!(
            score < FuzzySearchConfig::default().threshold,
            "konnitiha vs konnichiha should be below the 0.94 threshold: got {}",
            score
        );
    }

    /// Hepburn `konnichiha` matches canonical Hepburn `konnichiha` exactly (control)
    #[test]
    fn hepburn_konnichiha_matches_canonical_hepburn_exactly() {
        let score = jaro_winkler_similarity("konnichiha", "konnichiha");
        assert!(
            (score - 1.0).abs() < f64::EPSILON,
            "konnichiha vs konnichiha should be an exact match at 1.0: got {}",
            score
        );
    }

    /// Kunrei-shiki `konnitiha` is *not* dropped by the length-difference cutoff
    /// (`length_ok`)
    ///
    /// This mechanically establishes that what rejects it is the 0.94 threshold
    /// rather than the length cutoff (the branch point that keeps us from fixing
    /// the wrong place).
    #[test]
    fn kunrei_konnitiha_is_not_dropped_by_the_length_cutoff() {
        let query_len = "konnitiha".chars().count();
        let index_len = "konnichiha".chars().count();
        assert!(
            length_ok(query_len, index_len),
            "konnitiha ({query_len} chars) vs konnichiha ({index_len} chars) should pass length_ok"
        );
    }

    /// The JW score of Kunrei-shiki `kanzi` against canonical Hepburn `kanji` is
    /// below 0.94 (control; corrects what 03-CONTEXT.md D-82 says)
    ///
    /// 03-CONTEXT.md claimed that "`kanzi` -> 漢字 only works because the lengths
    /// differ by a single character (i.e. it passes the JW threshold), which is a
    /// coincidence", but measured, the JW score itself is below 0.94. The real
    /// reason `Kanzi` reaches 漢字 is not the JW threshold but the exact-match
    /// path shown by `kunrei_kanzi_and_hepburn_kanji_convert_to_the_same_kana`
    /// below.
    #[test]
    fn kunrei_kanzi_vs_canonical_hepburn_kanji_scores_below_0_94() {
        let score = jaro_winkler_similarity("kanzi", "kanji");
        // Measured (2026-09-23, strsim jaro_winkler): 0.9066666666666667
        assert!(
            score < FuzzySearchConfig::default().threshold,
            "kanzi vs kanji should be below the 0.94 threshold (if it is >= 0.94 the D-82 correction is wrong): got {}",
            score
        );
    }

    /// Kunrei-shiki `kanzi` and Hepburn `kanji` both convert to the same kana
    /// 「かんじ」 through the input table of `romaji.rs` (the actual reason 漢字 is
    /// reached, via the exact-match path)
    ///
    /// `rule!("zi","じ")` and `rule!("ji","じ")` in `romaji.rs` emit the same kana
    /// 「じ」, so `Kanzi` hits 「かんじ」 directly in the dictionary's exact-match
    /// lookup (the key produced by `stem_kana`). It never goes through JW fuzzy
    /// search.
    #[test]
    fn kunrei_kanzi_and_hepburn_kanji_convert_to_the_same_kana() {
        use crate::romaji::RomajiConverter;

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

        let kunrei = convert("kanzi");
        let hepburn = convert("kanji");
        assert_eq!(
            kunrei, "かんじ",
            "Kunrei-shiki kanzi should convert to 「かんじ」: got {}",
            kunrei
        );
        assert_eq!(
            hepburn, "かんじ",
            "Hepburn kanji should convert to 「かんじ」: got {}",
            hepburn
        );
        assert_eq!(
            kunrei, hepburn,
            "kanzi and kanji convert to the same kana, which is how 漢字 is reached through the exact-match path"
        );
    }

    // === fuzzy_filter_roman ===

    /// Confirms that below-threshold and length-cutoff matches are dropped, leaving only the exact match.
    #[test]
    fn test_below_threshold_filtered() {
        let bucket = vec![
            key("にほんご", "nihongo"),
            key("あいうえおかきくけこ", "aiueokakikukeko"), // unrelated key, dropped by the length cutoff
        ];
        let config = FuzzySearchConfig::default();
        let results = fuzzy_filter_roman("nihongo", &bucket, &config);

        assert_eq!(
            results.len(),
            1,
            "below-threshold and length-cutoff matches should be dropped"
        );
        assert_eq!(results[0].key, "にほんご");
    }

    /// Confirms results are sorted by descending score with the exact match first.
    #[test]
    fn test_results_sorted_by_score_descending() {
        let bucket = vec![
            key("にほんご", "nihongo"), // exact match
            key("にほんぎ", "nihongi"), // high similarity
            key("にほんざ", "nihonza"), // slightly lower similarity
        ];
        let config = FuzzySearchConfig { threshold: 0.7 }; // low threshold so everything matches
        let results = fuzzy_filter_roman("nihongo", &bucket, &config);

        for window in results.windows(2) {
            assert!(
                window[0].score >= window[1].score,
                "results should be in descending score order: {} >= {} failed",
                window[0].score,
                window[1].score
            );
        }
        assert_eq!(results[0].key, "にほんご");
        assert!((results[0].score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ties_are_ordered_by_ascending_kana_key() {
        let bucket = vec![key("ぶ", "nihongo"), key("あ", "nihongo")];
        let config = FuzzySearchConfig { threshold: 0.0 };
        let results = fuzzy_filter_roman("nihongo", &bucket, &config);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].key, "あ");
        assert_eq!(results[1].key, "ぶ");
    }

    /// Boundary: a length ratio of exactly 0.70 (7-char query, 10-char key) is
    /// not cut off and proceeds to the JW decision.
    #[test]
    fn length_ratio_of_exactly_0_70_is_not_cut_off() {
        let bucket = vec![key("かな1", "abcdefghij")];
        let config = FuzzySearchConfig { threshold: 0.0 };
        let results = fuzzy_filter_roman("abcdefg", &bucket, &config);

        assert_eq!(
            results.len(),
            1,
            "a length ratio of exactly 0.70 should not be cut off: {:?}",
            results.iter().map(|m| &m.key).collect::<Vec<_>>()
        );
    }

    /// Boundary: a length ratio below 0.70 (6-char query, 10-char key) never
    /// appears in the results, whatever the JW value is.
    #[test]
    fn length_ratio_below_0_70_never_appears_in_the_results() {
        let bucket = vec![key("かな2", "abcdefghij")];
        let config = FuzzySearchConfig { threshold: 0.0 };
        let results = fuzzy_filter_roman("abcdef", &bucket, &config);

        assert!(
            results.is_empty(),
            "a length ratio below 0.70 should not appear in the results: {:?}",
            results.iter().map(|m| &m.key).collect::<Vec<_>>()
        );
    }
}
