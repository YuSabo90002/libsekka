// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Input context state management
//!
//! `SekkaContext` is the state machine that manages every state transition of the
//! input method, from accepting romaji input through generating, selecting and
//! committing conversion candidates.

use crate::candidate::{
    alphabet_hankaku_candidate, alphabet_zenkaku_candidate, build_candidates, hiragana_candidate,
    katakana_candidate, match_stage, sharp_number, sort_candidates, Candidate, CandidateKind,
};
use crate::conversion::{analyze_input, classify_input_shape, split_okuri, InputShape};
use std::collections::HashMap;
use std::sync::Arc;

use crate::dictionary::{DictError, Dictionary, DictionaryMode, RomanKey};
use crate::fuzzy::{fuzzy_filter_roman, FuzzySearchConfig};
use crate::romaji::RomajiConverter;
use crate::roman_index;
use crate::symspell;

/// The `score` value of a SymSpell tier-1 candidate (D-96, 03.1-01). Since
/// `sort_candidates` never compares scores across different tiers, a tier-1 score
/// only breaks ties within tier 1. The constant 1.0 is chosen because
/// `merge_candidate` folds scores by taking the maximum: any other value would lower
/// the score of exactly those candidates whose display string collides with tier 2
/// (JW<1.0) and break the ordering inside tier 1. No distance-based normalized value
/// is used (03.1-01 Open Questions 2 RESOLVED).
const SYMSPELL_TIER_SCORE: f64 = 1.0;

/// Conversion state - the current state of the input context
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionState {
    /// Input - key input is accumulating in the romaji buffer.
    Input,
    /// Converting - a transient state while conversion runs (never observed; it only occurs inside the immediate-commit path).
    Converting,
    /// Selecting - a second Ctrl-J right after a commit is reselecting among the
    /// candidates of the word just committed (D-06/D-08). The candidate window is
    /// shown only in this state.
    Selecting,
}

/// Derives the dictionary key suffix of the okuri-ari key convention of SKK-JISYO.L
/// from the romaji of the okurigana
///
/// SKK-JISYO.L (and dictionaries derived from it) stores words with okurigana under
/// the key "stem reading + the first character of the okurigana romaji" (e.g.
/// かん+j=かんj, おこな+u=おこなu; verified against real data in
/// .planning/debug/g-01.1-6a-candidate-dedup.md and g-01.1-6b-kanji-ranking.md).
/// This function derives the single leading character to append to the dictionary
/// key. It returns `None` when the first character is non-ASCII or the okurigana is
/// empty (okuri-nashi), because appending a non-ASCII character would build a key
/// that does not exist in the dictionary.
fn okuri_key_suffix(okuri_romaji: &str) -> Option<char> {
    let first = okuri_romaji.chars().next()?;
    if first.is_ascii_alphabetic() {
        Some(first.to_ascii_lowercase())
    } else {
        None
    }
}

/// Merges dictionary-derived candidates by display string (deduplicating on display)
///
/// Keeps the first candidate that appears and folds the remaining fields into it
/// per the rules below (D-147, replacing the old "always take the maximum
/// frequency and score" rule). The same spelling gets generated several times
/// both because the dictionary holds okuri-ari verbs under one key per
/// conjugating consonant (G-01.1-6a) and because the same word exists in the
/// master and user dictionaries, so it is folded at merge time.
///
/// Deduplication is confined to this scope (dictionary-derived candidates). The four
/// fallback candidates `build_candidate_list` appends - hiragana, katakana,
/// full-width alphabet and half-width alphabet - are excluded (spec.md:228-229
/// requires at least four, and the Ctrl-U/I/K/L/E direct switches of D-09 would no
/// longer be able to point at the first candidate of each kind).
///
/// Per-field merge rules:
/// - `frequency`: folded by maximum only among candidates from the same match
///   stage (`match_stage`, D-147). A fuzzy-stage (tier 1/2) duplicate's
///   frequency is never carried into an exact-match (tier 0) duplicate,
///   because learning is recorded per reading (D-34/D-37): a word learned
///   under one reading's fuzzy hit must not push up a different reading's
///   exact match of the same word (RANK-01). When an exact match arrives
///   after a fuzzy duplicate (not reachable through the current push order,
///   see `lookup_dictionary`'s "Order:" paragraph, but not assumed here),
///   the exact match's own frequency replaces the fuzzy one's, and (for the same
///   reason) its `learn_pair` replaces the fuzzy one's too
/// - `tier`: folded by minimum (D-147, Pitfall 7), so this does not silently
///   depend on `lookup_dictionary` always pushing tier 0 before tier 1/2
/// - `score`: take the maximum
/// - `learn_pair`: **first wins within a stage** (whatever became `Some` first among
///   candidates of the same match stage is kept and is never overwritten by the
///   `learn_pair` of a later same-stage candidate). When an exact match arrives after
///   a fuzzy duplicate (the same defensive, currently-unreached branch described
///   above for `frequency`), the exact match's own `learn_pair` takes over instead,
///   for the same reason as `frequency` (D-147)
///
/// Why only `learn_pair` is first-wins (within a stage): `learn_pair` is the
/// **provenance** (D-32 / D-33) of "which reading and word pair to record into the
/// user dictionary when this candidate is committed", and the reading of the lookup
/// that first generated the candidate is the correct thing to record. Because the
/// dictionary holds okuri-ari verbs under one key per conjugating consonant
/// (G-01.1-6a), the same spelling can be generated from several reading keys, but the
/// one to record is the reading that hit first. Changing it to "last wins" or "merge",
/// as `frequency` does, would surface as learning that does not take effect or that is
/// recorded against a different reading. This asymmetry is intentional. The one
/// exception is the cross-stage, exact-after-fuzzy case: learning is recorded per
/// reading (D-34/D-37), so a tier-0 candidate must be recorded under its own exact
/// reading rather than under an unrelated fuzzy reading that merely happened to arrive
/// first (D-147).
fn merge_candidate(
    all: &mut Vec<Candidate>,
    seen: &mut HashMap<String, usize>,
    candidate: Candidate,
) {
    if let Some(&index) = seen.get(&candidate.display) {
        let existing = &mut all[index];
        let existing_stage = match_stage(existing.tier);
        let candidate_stage = match_stage(candidate.tier);
        if candidate_stage == existing_stage {
            // Same stage: fold frequency by maximum, exactly as before D-147.
            if candidate.frequency > existing.frequency {
                existing.frequency = candidate.frequency;
            }
        } else if candidate_stage < existing_stage {
            // An exact match arrived after a fuzzy duplicate. Not reachable
            // through the current push order (tier 0 -> 1 -> 2), but only the
            // exact match's own frequency must survive here (D-147, Pitfall 2).
            existing.frequency = candidate.frequency;
            // For the same reason: learning is recorded per reading (D-34/D-37),
            // so a tier-0 candidate must be recorded under its own exact reading,
            // not the unrelated fuzzy reading that happened to arrive first.
            if candidate.learn_pair.is_some() {
                existing.learn_pair = candidate.learn_pair;
            }
        }
        // candidate_stage > existing_stage: a fuzzy-stage duplicate arrived after
        // an exact match. Its frequency must never be carried into the exact
        // match (D-147, RANK-01) - leave `existing.frequency` untouched.
        if candidate.score > existing.score {
            existing.score = candidate.score;
        }
        if candidate.tier < existing.tier {
            existing.tier = candidate.tier;
        }
    } else {
        seen.insert(candidate.display.clone(), all.len());
        all.push(candidate);
    }
}

/// Extracts the runs of consecutive digits in order of appearance and returns the
/// string with each run replaced by a single `#` (ported from upstream
/// `emacs/sekka-henkan.el:150-155`, branch 4 of D-21)
///
/// `"2023nen"` -> `("#nen".to_string(), vec!["2023".to_string()])`.
/// `"12;34"` -> `("#;#".to_string(), vec!["12".to_string(), "34".to_string()])`.
fn replace_digit_runs_with_hash(raw: &str) -> (String, Vec<String>) {
    let mut result = String::new();
    let mut runs: Vec<String> = Vec::new();
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut run = String::new();
            run.push(c);
            while let Some(&next) = chars.peek() {
                if next.is_ascii_digit() {
                    run.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            runs.push(run);
            result.push('#');
        } else {
            result.push(c);
        }
    }

    (result, runs)
}

/// Collects every marker consisting of a `#` and the single digit right after it, in
/// order of appearance, and substitutes them positionally from `num_runs` (ported from
/// upstream `emacs/sekka-henkan.el:169-182`, branch 4 of D-21)
///
/// Returns `None` - dropping the candidate entirely - when the number of markers does
/// not equal `num_runs.len()` (upstream's skip rule). On a match, the i-th marker from
/// the left is replaced exactly once by the result of
/// `sharp_number(marker string, &num_runs[i])`.
/// **The pairing is by position of appearance, not by the marker's number** - the N in
/// `#N` is the conversion type the dictionary specifies (`#1` full-width / `#2` per-digit
/// kanji numerals / `#3` positional kanji numerals / anything else no conversion), not
/// "which digit run this is". To avoid the accident of inserting the wrong number in a
/// word where the same `#3` appears twice, the result is built by walking the original
/// character positions exactly once rather than by a value-based replacement like
/// `String::replacen`.
fn substitute_sharp_markers(word: &str, num_runs: &[String]) -> Option<String> {
    let chars: Vec<char> = word.chars().collect();

    // First pass: count the markers (used for the early count-mismatch check).
    let mut marker_count = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '#' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            marker_count += 1;
            i += 2;
        } else {
            i += 1;
        }
    }

    if marker_count != num_runs.len() {
        return None;
    }

    // Second pass: substitute while building in order of appearance.
    let mut result = String::new();
    let mut marker_index = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '#' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            let marker = format!("#{}", chars[i + 1]);
            let converted = sharp_number(&marker, &num_runs[marker_index]);
            result.push_str(&converted);
            marker_index += 1;
            i += 2;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    Some(result)
}

/// The unsent committed candidate placed in the preedit in the commit display state
/// (revised D-08)
///
/// Updated on every staging by Ctrl-J (`convert_and_stage`) and on every confirmation
/// during reselection (`confirm`). It has not been sent to the application yet and
/// only reaches `committed_output` once `flush_last_commit` is called (D-10/D-12).
struct LastCommit {
    /// The string placed in the preedit in the commit display state.
    text: String,
    /// The candidate list as of staging (restored on reselection).
    candidates: Vec<Candidate>,
    /// The index that was selected at staging time.
    index: usize,
    /// The romaji this word was converted from, before conversion (D-160, Phase
    /// 9). BackSpace in the commit display state, and during reselection
    /// (D-161), reverts to this instead of committing, deleting its last
    /// character (see `SekkaContext::backspace`). `confirm` carries this value
    /// over from the `last_commit` that was active before reselection began, so
    /// a word confirmed during reselection reverts to the same original romaji
    /// as the word it replaced.
    raw_romaji: String,
}

/// Japanese input context
///
/// The state machine that centrally manages accepting romaji input, kana conversion,
/// dictionary lookup, candidate selection and committed output. Used from input
/// frameworks such as fcitx5.
pub struct SekkaContext {
    /// The unconverted romaji input buffer.
    romaji_buffer: String,
    /// The string to display as the preedit.
    preedit: String,
    /// The current conversion state.
    state: ConversionState,
    /// The current conversion candidate list.
    candidates: Vec<Candidate>,
    /// The index of the selected candidate (-1 = nothing selected).
    candidate_index: i32,
    /// The dictionary list to consult.
    dictionaries: Vec<Arc<dyn Dictionary>>,
    /// Committed output text (taken with poll_output).
    committed_output: Option<String>,
    /// Cache of what was just committed (for reselection; invalidated by any other key input or by a reset, per D-06).
    last_commit: Option<LastCommit>,
}

impl Default for SekkaContext {
    fn default() -> Self {
        Self::new()
    }
}

impl SekkaContext {
    /// Creates a new input context
    pub fn new() -> Self {
        SekkaContext {
            romaji_buffer: String::new(),
            preedit: String::new(),
            state: ConversionState::Input,
            candidates: Vec::new(),
            candidate_index: -1,
            dictionaries: Vec::new(),
            committed_output: None,
            last_commit: None,
        }
    }

    /// Processes key input
    ///
    /// When `is_ctrl_j` is true:
    /// - in the Selecting state, advance to the next candidate (upstream's "next
    ///   candidate" key)
    /// - in the Input state with a non-empty romaji buffer, convert and place the first
    ///   candidate in the preedit (revised D-08; nothing is sent to the application yet)
    /// - in the Input state with an empty romaji buffer, enter reselection mode when
    ///   there is a staged candidate (D-06)
    /// - otherwise, do not consume the key
    ///
    /// When `is_ctrl_j` is false, the character is appended to the romaji buffer, only
    /// in the Input state. Any unsent staged candidate is committed here (D-12).
    ///
    /// # Returns
    /// true when the input was consumed, false when nothing was done.
    pub fn process_key(&mut self, ch: char, is_ctrl_j: bool) -> bool {
        if is_ctrl_j {
            return match self.state {
                ConversionState::Selecting => {
                    self.next_candidate();
                    true
                }
                ConversionState::Input if !self.romaji_buffer.is_empty() => {
                    self.convert_and_stage()
                }
                ConversionState::Input if self.last_commit.is_some() => self.begin_reselect(),
                _ => false,
            };
        }

        // Character input is accepted only in the Input state.
        if self.state != ConversionState::Input {
            return false;
        }

        // Any unsent staged candidate is committed here (D-12).
        // This must happen before pushing into the romaji buffer - in the other order,
        // the preedit.clear() of the flush would wipe out the new romaji.
        self.flush_last_commit();
        // Append to the romaji buffer.
        self.romaji_buffer.push(ch);
        // Update the preedit with the contents of the romaji buffer.
        self.preedit = self.romaji_buffer.clone();
        true
    }

    /// Commits the contents of the romaji buffer as they are, preserving case (D-03)
    ///
    /// In the Input state with a non-empty romaji buffer, emits the contents as committed
    /// output and clears the buffer and the preedit. Does nothing when the buffer is
    /// empty. In either case, any unsent staged candidate is committed here (D-12).
    ///
    /// # Returns
    /// true when something was committed, or when only a flush happened,
    /// false when the buffer was empty, there was no staged candidate and nothing happened
    pub fn commit_raw_romaji(&mut self) -> bool {
        // RESEARCH.md Pitfall 1: when a non-character key arrives in the commit display
        // state with an empty buffer, returning false even though the flush produced
        // output would make capi.rs return consumed=0, and the commit would be
        // overwritten by the next event and silently disappear. Early returns must always
        // use this return value.
        let flushed = self.flush_last_commit();
        if self.state != ConversionState::Input || self.romaji_buffer.is_empty() {
            return flushed;
        }
        let text = std::mem::take(&mut self.romaji_buffer);
        self.committed_output = Some(text);
        self.preedit.clear();
        true
    }

    /// Commits whatever this key event commits and appends `ch` to it (D-158)
    ///
    /// Calls `commit_raw_romaji()` first (its own D-12 flush and romaji-buffer
    /// commit) but ignores its return value: whether to append is decided by
    /// whether `committed_output` ends up `Some`, not by `commit_raw_romaji`'s
    /// return value. This matters because the caller (`dispatch_input` in
    /// capi.rs) already ran the single D-12 flush point for this key event before
    /// calling here; when that flush produced the commit-display word,
    /// `committed_output` already holds it and `commit_raw_romaji`'s own internal
    /// flush finds nothing left to flush and its romaji buffer is empty, so it
    /// returns `false` even though there is something to append to (09-RESEARCH
    /// Pattern 1's sketch corrected this way, Pitfall 2). This relies on
    /// `committed_output` being empty at the start of a key event (the C++ side
    /// polls it via `checkAndCommit()` on every consumed key).
    ///
    /// # Returns
    /// true when something was committed (`ch` was appended to it),
    /// false when there was nothing to commit (the buffer was empty, there was no
    /// staged candidate and the D-12 flush produced nothing)
    pub fn commit_with_trailing_char(&mut self, ch: char) -> bool {
        self.commit_raw_romaji();
        match &mut self.committed_output {
            Some(text) => {
                text.push(ch);
                true
            }
            None => false,
        }
    }

    /// Reverts to the original romaji on BackSpace, or edits the romaji buffer
    /// (D-160/D-161, Phase 9)
    ///
    /// D-13 used to say: in the commit display state with an empty buffer,
    /// BackSpace committed the staged word and the caller (capi.rs) then
    /// forwarded BackSpace to the application as it is. D-160 replaces this: the
    /// commit display state reverts to the `raw_romaji` of `last_commit` instead
    /// of committing it, deleting `raw_romaji`'s last character. D-161 does the
    /// same during reselection, after first closing the candidate window (the
    /// same three resets as `cancel`). Nothing is committed and nothing is
    /// learned in either case (D-12's `flush_last_commit` is never called here).
    /// When there is no staged candidate, the ordinary "delete the last character
    /// of the romaji buffer" behaviour (Pitfall 3) is unchanged.
    ///
    /// # Returns
    /// true when the candidate window was closed and/or a staged candidate was
    /// reverted to romaji, or when the buffer was edited; false when the buffer
    /// was empty, there was no staged candidate and nothing happened
    pub fn backspace(&mut self) -> bool {
        // D-161: closes the candidate window first when reselecting. `last_commit`
        // is still Some afterward (`begin_reselect` clones it but never takes it),
        // so the revert below still applies.
        if self.state == ConversionState::Selecting {
            self.state = ConversionState::Input;
            self.candidates.clear();
            self.candidate_index = -1;
        }

        // D-160: an unsent staged candidate reverts to its original romaji instead
        // of being committed. `last_commit` is taken first so the invariant
        // "last_commit Some => romaji_buffer empty" holds throughout (the buffer
        // is written only after last_commit has become None).
        if let Some(last) = self.last_commit.take() {
            self.romaji_buffer = last.raw_romaji;
            self.romaji_buffer.pop();
            self.preedit = self.romaji_buffer.clone();
            return true;
        }

        if self.state != ConversionState::Input || self.romaji_buffer.is_empty() {
            return false;
        }
        self.romaji_buffer.pop();
        self.preedit = self.romaji_buffer.clone();
        true
    }

    /// Returns a reference to the preedit string
    pub fn get_preedit(&self) -> &str {
        &self.preedit
    }

    /// Returns a reference to the current conversion candidate list
    pub fn get_candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// Returns the index of the selected candidate (-1 = nothing selected)
    pub fn get_candidate_index(&self) -> i32 {
        self.candidate_index
    }

    /// Takes the committed output text
    ///
    /// Returns Some(String) and clears the internal buffer when there is output, and
    /// None otherwise.
    pub fn poll_output(&mut self) -> Option<String> {
        self.committed_output.take()
    }

    /// The single exit through which an unsent staged candidate first reaches the
    /// application (D-10/D-12)
    ///
    /// When `last_commit` is Some, emits its string into `committed_output`, clears the
    /// preedit and returns `true`. The caller uses this return value to decide whether to
    /// forward the key. When `last_commit` is None, it does nothing and returns `false`.
    ///
    /// Learning is recorded only when the `learn_pair` of the committed candidate is
    /// `Some` (D-34: the recording point sits at the single exit of a real commit). It
    /// records into just the first dictionary whose `mode() == ReadWrite` (D-37; `find`
    /// suffices because `set_dictionaries` has already stably moved ReadWrite
    /// dictionaries to the front, D-39). A recording failure (a sled error, a read-only
    /// filesystem and so on) never blocks the commit (D-35; neither `?` propagation nor
    /// panic unwinding happens).
    pub fn flush_last_commit(&mut self) -> bool {
        if let Some(last) = self.last_commit.take() {
            if let Some(candidate) = last.candidates.get(last.index) {
                if let Some((reading, word)) = &candidate.learn_pair {
                    if let Some(dict) = self
                        .dictionaries
                        .iter()
                        .find(|d| d.mode() == DictionaryMode::ReadWrite)
                    {
                        let _ = dict.record_selection(reading, word);
                    }
                }
            } // <- the immutable borrow of self.dictionaries ends here (finding 2)
            self.committed_output = Some(last.text);
            self.preedit.clear();
            true
        } else {
            false
        }
    }

    /// Resets the input context to its initial state
    pub fn reset(&mut self) {
        self.romaji_buffer.clear();
        self.preedit.clear();
        self.state = ConversionState::Input;
        self.candidates.clear();
        self.candidate_index = -1;
        self.committed_output = None;
        // D-06: a focus change, an input method switch or a reset invalidates the last commit.
        self.last_commit = None;
    }

    /// Returns the current conversion state
    pub fn state(&self) -> ConversionState {
        self.state
    }

    /// Adds a dictionary
    ///
    /// A dictionary can be shared by several contexts.
    pub fn add_dictionary(&mut self, dict: Arc<dyn Dictionary>) {
        self.dictionaries.push(dict);
    }

    /// Replaces the dictionary list
    ///
    /// Stably moves dictionaries whose `mode() == ReadWrite` to the front (D-39). The
    /// sort is stable, so the relative order within one mode stays exactly as the caller
    /// passed it. This guarantees here, rather than leaving it to the caller, the
    /// assumption that "the user dictionary is at the head of the dictionary list" that
    /// branches 4 and 6 (the paths that do not go through `sort_candidates`) depend on
    /// (D-38 Pitfall 3).
    pub fn set_dictionaries(&mut self, mut dicts: Vec<Arc<dyn Dictionary>>) {
        dicts.sort_by_key(|d| d.mode() != DictionaryMode::ReadWrite);
        self.dictionaries = dicts;
    }

    /// Tells every dictionary it holds to save (D-102)
    ///
    /// Returns the first error if any of them fails (while still trying to save the
    /// others). Returns `Ok(())` when no dictionary is set (a no-op).
    pub fn save_dictionaries(&self) -> Result<(), DictError> {
        let mut first_err = None;
        for dict in &self.dictionaries {
            if let Err(e) = dict.save() {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Selects the next candidate (Ctrl-J during reselection, and so on)
    pub fn next_candidate(&mut self) {
        if self.state != ConversionState::Selecting || self.candidates.is_empty() {
            return;
        }
        let max_index = self.candidates.len() as i32 - 1;
        if self.candidate_index < max_index {
            self.candidate_index += 1;
        } else {
            // Wrap to the start when going past the end.
            self.candidate_index = 0;
        }
        self.update_preedit_from_candidate();
    }

    /// Selects the previous candidate (Ctrl-P during reselection)
    pub fn prev_candidate(&mut self) {
        if self.state != ConversionState::Selecting || self.candidates.is_empty() {
            return;
        }
        if self.candidate_index > 0 {
            self.candidate_index -= 1;
        } else {
            // Wrap to the end when going past the start.
            self.candidate_index = self.candidates.len() as i32 - 1;
        }
        self.update_preedit_from_candidate();
    }

    /// Moves the candidate selected during reselection into the commit display state
    /// (Enter, revised D-08)
    ///
    /// Nothing is sent to the application yet. It updates `last_commit` with the confirmed
    /// string and the whole candidate list, so a second Ctrl-J from the commit display
    /// state can reselect starting from this choice (D-06).
    pub fn confirm(&mut self) {
        if self.state != ConversionState::Selecting {
            return;
        }

        let index = if self.candidate_index >= 0
            && (self.candidate_index as usize) < self.candidates.len()
        {
            self.candidate_index as usize
        } else {
            // Confirm the first candidate when nothing is selected.
            0
        };

        let output = if !self.candidates.is_empty() {
            self.candidates[index].display.clone()
        } else {
            String::new()
        };

        // D-160: carry over the original romaji from the last_commit that was
        // active before reselection began (`begin_reselect` clones its candidates
        // but never takes `last_commit`, so it is still Some here). A word
        // confirmed during reselection must revert to the same romaji as the
        // word it replaced.
        let raw_romaji = self
            .last_commit
            .take()
            .map(|last| last.raw_romaji)
            .unwrap_or_default();

        self.preedit = output.clone();
        self.last_commit = Some(LastCommit {
            text: output,
            candidates: std::mem::take(&mut self.candidates),
            index,
            raw_romaji,
        });
        debug_assert!(
            self.last_commit.is_none() || self.romaji_buffer.is_empty(),
            "the romaji buffer is empty in the commit display state (revised D-08)"
        );

        self.romaji_buffer.clear();
        self.state = ConversionState::Input;
        self.candidates.clear();
        self.candidate_index = -1;
    }

    /// Cancels reselection and puts the original first candidate back in the preedit
    /// (Esc; the meaning changed in D-19)
    ///
    /// With the deferred commit, nothing has ever been sent to the application, so it is
    /// enough to put the original string of `last_commit` back into the preedit (no
    /// re-commit is needed). `last_commit` itself is left unchanged, so a second Ctrl-J
    /// right after cancelling can reselect again.
    pub fn cancel(&mut self) {
        if self.state != ConversionState::Selecting {
            return;
        }

        if let Some(last) = &self.last_commit {
            self.preedit = last.text.clone();
        }

        self.state = ConversionState::Input;
        self.candidates.clear();
        self.candidate_index = -1;
        debug_assert!(
            self.last_commit.is_none() || self.romaji_buffer.is_empty(),
            "the romaji buffer is empty in the commit display state (revised D-08)"
        );
    }

    /// Commits whatever is currently on screen, right now (D-15)
    ///
    /// In the Selecting state it moves the selected candidate into the commit display
    /// state and then flushes; in the commit display state it flushes as it is. The C++
    /// side uses it to "commit what is on screen" on a focus change, an input method
    /// switch or an explicit reset.
    pub fn finalize_staged(&mut self) {
        self.confirm();
        self.flush_last_commit();
    }

    /// During reselection, moves directly to the first candidate of the given kind (D-09)
    ///
    /// Outside the Selecting state, or when there is no candidate of that kind, it does
    /// nothing and returns false (leaving the selection unchanged, like upstream
    /// sekka-select-by-type).
    fn select_first_of(&mut self, kinds: &[CandidateKind]) -> bool {
        if self.state != ConversionState::Selecting {
            return false;
        }
        let Some(index) = self.candidates.iter().position(|c| kinds.contains(&c.kind)) else {
            return false;
        };
        self.candidate_index = index as i32;
        self.update_preedit_from_candidate();
        true
    }

    /// During reselection, moves directly to the first kanji candidate (with or without okurigana) (Ctrl-A, D-09)
    pub fn select_kanji(&mut self) -> bool {
        self.select_first_of(&[CandidateKind::Kanji, CandidateKind::KanjiWithOkuri])
    }

    /// During reselection, moves directly to the hiragana candidate (Ctrl-U, D-09)
    pub fn select_hiragana(&mut self) -> bool {
        self.select_first_of(&[CandidateKind::Hiragana])
    }

    /// During reselection, moves directly to the katakana candidate (Ctrl-I/Ctrl-K, D-09)
    pub fn select_katakana(&mut self) -> bool {
        self.select_first_of(&[CandidateKind::Katakana])
    }

    /// During reselection, moves directly to the half-width alphabet candidate (Ctrl-L, D-09)
    pub fn select_hankaku(&mut self) -> bool {
        self.select_first_of(&[CandidateKind::AlphabetHankaku])
    }

    /// During reselection, moves directly to the full-width alphabet candidate (Ctrl-E, D-09)
    pub fn select_zenkaku(&mut self) -> bool {
        self.select_first_of(&[CandidateKind::AlphabetZenkaku])
    }

    /// Converts and places the first candidate in the preedit (Ctrl-J with a non-empty
    /// romaji buffer, revised D-08)
    ///
    /// Analyses the contents of the romaji buffer, does the work for the conversion mode,
    /// builds the candidate list and then places the first candidate in the preedit. The
    /// commit is deferred until the next key (revised D-08) and no candidate window is
    /// shown. The first candidate and the whole candidate list are cached in
    /// `last_commit`, so a second Ctrl-J from the commit display state can reselect
    /// (D-06).
    fn convert_and_stage(&mut self) -> bool {
        if self.romaji_buffer.is_empty() {
            return false;
        }

        // Move to the Converting state.
        self.state = ConversionState::Converting;

        // Consult the dictionary in every mode and build the hiragana, katakana and
        // alphabet candidates in upstream's order (D-07).
        self.candidates = self.build_candidate_list();

        // Candidate building has finished reading the romaji buffer, so empty it right
        // here (before setting last_commit). That way the invariant "if last_commit is
        // Some then romaji_buffer is empty" (RESEARCH.md Pitfall 2) holds from the moment
        // of the assignment. Clearing the buffer after assigning last_commit instead makes
        // the invariant check right after the assignment report a genuine bug (actually
        // observed during the RED evaluation of this task).
        let raw_romaji = std::mem::take(&mut self.romaji_buffer);

        if self.candidates.is_empty() {
            // When the candidate list came out empty (a defensive case that D-05 should
            // normally prevent): commit the romaji buffer contents as they are, with no
            // reselection available.
            self.committed_output = Some(raw_romaji);
            self.last_commit = None;
            self.preedit.clear();
        } else {
            let text = self.candidates[0].display.clone();
            self.preedit = text.clone();
            self.last_commit = Some(LastCommit {
                text,
                candidates: std::mem::take(&mut self.candidates),
                index: 0,
                raw_romaji,
            });
            debug_assert!(
                self.last_commit.is_none() || self.romaji_buffer.is_empty(),
                "the romaji buffer is empty in the commit display state (revised D-08)"
            );
        }

        self.candidates.clear();
        self.candidate_index = -1;
        self.state = ConversionState::Input;

        true
    }

    /// Enters reselection mode from the commit display state (Ctrl-J with an empty buffer
    /// and a staged candidate, D-06)
    ///
    /// Restores the whole candidate list of the commit display state, moves to the
    /// Selecting state and selects exactly the index that was selected (like upstream
    /// sekka-rK-trans, entering reselection does not automatically advance to the next
    /// candidate). With the deferred commit the word has not been sent to the application
    /// yet, so no delete request is generated at all (stage 1 of D-18).
    fn begin_reselect(&mut self) -> bool {
        let Some(last) = self.last_commit.as_ref() else {
            return false;
        };

        self.candidates = last.candidates.clone();
        self.candidate_index = last.index as i32;
        self.state = ConversionState::Selecting;
        self.update_preedit_from_candidate();

        true
    }

    /// Branches on the input shape (D-22) and builds the candidate list
    ///
    /// The path is chosen from the result of `classify_input_shape`. `CaseBased` (contains
    /// uppercase, branch 2) and `HiraganaConvertible` (all lowercase, branch 5) run the
    /// existing body unmodified (consult the dictionary in every mode and build the
    /// hiragana, katakana and alphabet candidates in upstream's order, D-07) (D-24).
    /// `Symbol` (symbols and anything else, branch 6) is delegated to
    /// `build_symbol_candidates`.
    fn build_candidate_list(&self) -> Vec<Candidate> {
        match classify_input_shape(&self.romaji_buffer) {
            InputShape::CaseBased | InputShape::HiraganaConvertible => {
                let request = analyze_input(&self.romaji_buffer);
                let (stem_romaji, okuri_romaji) =
                    split_okuri(&self.romaji_buffer, request.okuri_position);

                let stem_kana = self.romaji_to_kana(&stem_romaji);
                let okuri_kana = if okuri_romaji.is_empty() {
                    String::new()
                } else {
                    self.romaji_to_kana(&okuri_romaji)
                };
                let full_kana = format!("{}{}", stem_kana, okuri_kana);

                let mut list =
                    self.lookup_dictionary(&stem_romaji, &stem_kana, &okuri_kana, &okuri_romaji);
                list.push(hiragana_candidate(&full_kana));
                list.push(katakana_candidate(&full_kana));
                list.push(alphabet_zenkaku_candidate(&self.romaji_buffer));
                list.push(alphabet_hankaku_candidate(&self.romaji_buffer));

                sort_candidates(&mut list, request.mode);
                list
            }
            InputShape::NumberOnly => self.build_number_candidates(&self.romaji_buffer),
            InputShape::NumberPrefixed => {
                self.build_number_prefixed_candidates(&self.romaji_buffer)
            }
            InputShape::Symbol => self.build_symbol_candidates(),
        }
    }

    /// Candidate building for branch 3 (digits only) (D-21, upstream
    /// `emacs/sekka-henkan.el:273-279`)
    ///
    /// No dictionary is consulted at all. As upstream, there are exactly 4 candidates, in
    /// the order `#1` (full-width) -> the bare half-width digits -> `#2` (per-digit kanji
    /// numerals) -> `#3` (positional kanji numerals). All four get `CandidateKind::Kanji`
    /// (the same treatment as the existing `classify_dictionary_kind`, which classifies a
    /// word containing neither kanji nor hiragana as `Kanji`). No kana or katakana
    /// candidate is built and `sort_candidates` is not called (D-23/D-24). Full-width and
    /// half-width alphabet candidates are **not** added either - only upstream's branch 3
    /// does not call for alphabet candidates, asymmetrically with branches 4 and 6
    /// (RESEARCH.md Pitfall 6).
    fn build_number_candidates(&self, digits: &str) -> Vec<Candidate> {
        vec![
            Candidate {
                display: sharp_number("#1", digits),
                reading: digits.to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 0,
                // D-32: no dictionary was consulted at all, so nothing is learned (stays None).
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: digits.to_string(),
                reading: digits.to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: sharp_number("#2", digits),
                reading: digits.to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 0,
                tier: 0,
                learn_pair: None,
            },
            Candidate {
                display: sharp_number("#3", digits),
                reading: digits.to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 0,
                tier: 0,
                learn_pair: None,
            },
        ]
    }

    /// Candidate building for branch 4 (digit-leading, `#`-substituted dictionary lookup)
    /// (D-21, upstream `emacs/sekka-henkan.el:147-183`)
    ///
    /// Builds `replaced` (with the digit parts replaced by `#`) and the extracted digit
    /// runs `num_runs` via `replace_digit_runs_with_hash`. `replaced` goes through the
    /// existing `romaji_to_kana` to become a reading (`#` is not in the romaji table, so
    /// the pass-through rule of `try_convert` leaves it as it is). The reading is passed
    /// straight to the existing `lookup_dictionary` (as okuri-nashi, with empty
    /// `okuri_kana` and `okuri_romaji`). Calling `lookup_dictionary` rather than the
    /// exact-match-only `lookup_dictionary_exact` is where the correction of D-23 is
    /// implemented - upstream `sekka-henkan--okuri-nashi-and-number` passes the
    /// `#`-substituted string to `sekka-henkan--okuri-nashi`, and that function does
    /// Jaro-Winkler fuzzy search in addition to exact matching. The `display` of each
    /// candidate obtained goes through `substitute_sharp_markers` and candidates that come
    /// back `None` (a mismatch between the number of `#` markers and the number of digit
    /// runs) are dropped. Finally the full-width and half-width alphabet candidates (D-05)
    /// are appended in that order (branch 4 does call for alphabet candidates upstream;
    /// note the asymmetry with branch 3, RESEARCH.md Pitfall 6). No kana or katakana
    /// candidate is built and the candidates are not reordered (D-23/D-24).
    fn build_number_prefixed_candidates(&self, raw_input: &str) -> Vec<Candidate> {
        let (replaced, num_runs) = replace_digit_runs_with_hash(raw_input);
        let reading = self.romaji_to_kana(&replaced);

        let dict_candidates = self.lookup_dictionary(&replaced, &reading, "", "");

        let mut list: Vec<Candidate> = dict_candidates
            .into_iter()
            .filter_map(|cand| {
                substitute_sharp_markers(&cand.display, &num_runs)
                    .map(|display| Candidate { display, ..cand })
            })
            .collect();

        list.push(alphabet_zenkaku_candidate(raw_input));
        list.push(alphabet_hankaku_candidate(raw_input));
        list
    }

    /// Candidate building for branch 6 (symbols and anything else) (D-23/D-25)
    ///
    /// Places only the exact-match results from `lookup_dictionary_exact` at the front, in
    /// the order the dictionary stores them (D-25; the candidates are not reordered.
    /// Calling `sort_candidates` would sink the dictionary-derived candidates to the end
    /// via `group_rank` and break the "leave it to learning" design of D-25). No hiragana
    /// or katakana candidate is built and no fuzzy search is done (D-23; upstream
    /// `sekka-henkan--non-kanji`, `emacs/sekka-henkan.el:286-293`, is exact-match only).
    /// The full-width and half-width alphabet candidates (D-05) are appended at the end.
    fn build_symbol_candidates(&self) -> Vec<Candidate> {
        let reading = self.romaji_to_kana(&self.romaji_buffer);
        let mut list = self.lookup_dictionary_exact(&reading);
        list.push(alphabet_zenkaku_candidate(&self.romaji_buffer));
        list.push(alphabet_hankaku_candidate(&self.romaji_buffer));
        list
    }

    /// Performs exact-match dictionary lookup only (no fuzzy search)
    ///
    /// A helper carved out of the exact-match part of `lookup_dictionary`. It is called
    /// both from `lookup_dictionary` for branches 2 and 5 (which then continue with fuzzy
    /// search) and from `build_symbol_candidates` for branch 6 (which stops at exact
    /// matching, D-23). Appending okurigana is the caller's job (`lookup_dictionary`
    /// appends `okuri_kana` to the results, but the symbol branch has no notion of
    /// okurigana, so it is not done here). Carving this out must not change the candidate
    /// lists of branches 2 and 5 (the golden test of Task 1 detects that).
    fn lookup_dictionary_exact(&self, exact_key: &str) -> Vec<Candidate> {
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        for dict in &self.dictionaries {
            if let Ok(entries) = dict.lookup(exact_key) {
                if !entries.is_empty() {
                    for c in build_candidates(exact_key, &entries, 1.0, 0) {
                        merge_candidate(&mut candidates, &mut seen, c);
                    }
                }
            }
        }

        candidates
    }

    /// Performs dictionary lookup and fuzzy search with the stem romaji query and the stem
    /// kana (appending okurigana when there is any) (D-57/D-59/D-74)
    ///
    /// Even in all-lowercase mode the dictionary is consulted okuri-nashi (D-07: "the
    /// result of an okuri-nashi search with the lowercase reading").
    ///
    /// **Exact-match part (unchanged):** for okuri-ari conversion (a non-empty
    /// `okuri_romaji`), the exact-match key is built to follow the okuri-ari convention of
    /// SKK-JISYO.L (stem_kana plus the first character of the okurigana romaji; see
    /// `okuri_key_suffix`). In SKK-JISYO.L, かん (okuri-nashi noun, headed by 缶) and かんj
    /// (okuri-ari, 感 only) are separate entries, and failing to distinguish them lets an
    /// unrelated noun candidate steal the first slot at score=1.0 in okurigana mode
    /// (G-01.1-6b).
    ///
    /// **Fuzzy search part (rebuilt in D-57/D-59/D-74):** okuri-ari conversion (a non-empty
    /// `okuri_romaji`) stops at exact matching here (D-74; going further would mean defining
    /// our own canonicalization rules for okuri-ari keys in romaji space, which is out of
    /// scope). Okuri-nashi conversion uses the lowercased `stem_romaji` as the query
    /// (matching `raw-roman = (sekka-downcase keyword)` in upstream
    /// `sekka-henkan--okuri-nashi`; the index side is lowercase canonical Hepburn, so
    /// forgetting to lowercase means `Nihongo` never hits anything), cuts an adaptive prefix
    /// with `roman_index::query_prefix` and reads the `roman_bucket` of each dictionary
    /// exactly once (no full dictionary scan per Ctrl-J, D-59). Keys already handled by
    /// exact matching are removed from the collected buckets, and the rest go through
    /// `fuzzy::fuzzy_filter_roman` (threshold 0.94, with the length-difference cutoff) to
    /// become candidates.
    ///
    /// Dictionary-derived candidates are deduplicated per display string with
    /// `merge_candidate` (G-01.1-6a: the same spelling gets generated several times both
    /// because okuri-ari verbs have one key per conjugating consonant and because the same
    /// word exists in several dictionaries).
    ///
    /// Order: exact matches (tier 0, score 1.0) are pushed first, then SymSpell (tier 1),
    /// then the JW<1.0 fuzzy matches (tier 2). This matches upstream's four-stage structure
    /// "exact match -> JW=1.0 -> SymSpell -> JW<1.0" (lifting the Deferred of D-58,
    /// 03.1-01). `sort_candidates` places the exact-match stage (tier 0) ahead of the
    /// fuzzy stage (tier 1/2), then orders within a stage by frequency and then by tier
    /// (D-144/D-146, replacing D-96's "frequency before tier"). The push order here only
    /// matters for candidates that end up next to each other after that sort, and for
    /// `learn_pair`'s first-wins (D-32/D-33). `merge_candidate` does not depend on this
    /// push order for how it folds frequency and tier (D-147).
    ///
    /// **SymSpell part (tier 1, 03.1-01/03.1-02, all four paths):** it walks all four paths
    /// of upstream `sekka-symspell-search` (lines 181-238): path 2 (a delete variant of
    /// `stem_kana` is itself a dictionary key; distance fixed at 1, no recomputation), path
    /// 3 (`stem_kana` itself hits the index, i.e. the dictionary key is one character
    /// longer) and path 4 (a delete variant of `stem_kana` hits the index, i.e. a
    /// substitution). For index hits (paths 3 and 4) the distance is recomputed with
    /// `strsim::levenshtein` and only `0 < dist <= 1` is accepted (false positives from
    /// hash collisions are dropped here; distance 0 is already handled by exact matching,
    /// matching the `(> dist 0)` of upstream `sekka-henkan--okuri-nashi`). Across all four
    /// paths, deduplication is first-wins over **a single set of kana keys** (matching
    /// upstream's single `seen` hash), and only after every path has finished are they
    /// sorted once, following upstream `sekka-jisyo-approximate-search` (lines 403-434)
    /// (ascending distance -> okuri-ari keys last -> ascending length difference from the
    /// query -> ascending kana key, the last stage being a deterministic tie-break in the
    /// style of fuzzy.rs), before becoming candidates (so the same key is never looked up
    /// twice from several paths, which is what makes the absence of a cap in D-97
    /// workable). The user dictionary index (D-95) is the responsibility of the
    /// `Dictionary::symspell_bucket` implementation (`UserDict`); no distinction is made
    /// here by caller.
    fn lookup_dictionary(
        &self,
        stem_romaji: &str,
        stem_kana: &str,
        okuri_kana: &str,
        okuri_romaji: &str,
    ) -> Vec<Candidate> {
        let mut all_candidates: Vec<Candidate> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        let suffix = okuri_key_suffix(okuri_romaji);
        let exact_key = match suffix {
            Some(c) => format!("{}{}", stem_kana, c),
            None => stem_kana.to_string(),
        };

        // Dictionary lookup (exact match) - unchanged.
        let mut exact_candidates = self.lookup_dictionary_exact(&exact_key);
        // Append the okurigana to the display string when there is any.
        if !okuri_kana.is_empty() {
            for c in &mut exact_candidates {
                c.display = format!("{}{}", c.display, okuri_kana);
            }
        }
        for c in exact_candidates {
            merge_candidate(&mut all_candidates, &mut seen, c);
        }

        // D-74: okuri-ari conversion does no fuzzy search at all and stops at exact matching.
        if !okuri_romaji.is_empty() {
            return all_candidates;
        }

        // tier 1: SymSpell (03.1-02, all four paths). Deduplicated first-wins over a
        // single set of kana keys (symspell_seen); sorted and turned into candidates once,
        // only after every path has finished (so the same key is never looked up twice
        // from several paths).
        let mut symspell_hits: Vec<(String, usize)> = Vec::new(); // (kana key, distance)
        let mut symspell_seen: HashMap<String, ()> = HashMap::new();

        // Path 2: a delete variant of stem_kana is itself a dictionary key (distance fixed
        // at 1; upstream does not recompute here, since a single deletion makes distance 1 obvious).
        for variant in symspell::delete_variants(stem_kana) {
            if variant == exact_key || symspell_seen.contains_key(&variant) {
                continue;
            }
            for dict in &self.dictionaries {
                if let Ok(entries) = dict.lookup(&variant) {
                    if !entries.is_empty() {
                        symspell_seen.insert(variant.clone(), ());
                        symspell_hits.push((variant.clone(), 1));
                        break;
                    }
                }
            }
        }

        // Path 3: stem_kana itself hits the index as "somebody's delete variant"
        // (the case where the dictionary key is one character longer than stem_kana).
        for dict in &self.dictionaries {
            for key in dict.symspell_bucket(stem_kana).unwrap_or_default() {
                if key == exact_key || symspell_seen.contains_key(&key) {
                    continue;
                }
                // False positives from hash collisions are dropped here (D-92). Distance 0
                // is already handled by exact matching (matching the (> dist 0) of upstream
                // sekka-henkan--okuri-nashi).
                let dist = strsim::levenshtein(stem_kana, &key);
                if dist == 0 || dist > 1 {
                    continue;
                }
                symspell_seen.insert(key.clone(), ());
                symspell_hits.push((key, dist));
            }
        }

        // Path 4: a delete variant of stem_kana hits the index (the substitution case;
        // deleting one character from each leaves them sharing the same delete variant).
        for variant in symspell::delete_variants(stem_kana) {
            for dict in &self.dictionaries {
                for key in dict.symspell_bucket(&variant).unwrap_or_default() {
                    if key == exact_key || symspell_seen.contains_key(&key) {
                        continue;
                    }
                    let dist = strsim::levenshtein(stem_kana, &key);
                    if dist == 0 || dist > 1 {
                        continue;
                    }
                    symspell_seen.insert(key.clone(), ());
                    symspell_hits.push((key, dist));
                }
            }
        }

        // The upstream-conformant sort (lines 403-434 of `sekka-jisyo-approximate-search`
        // plus the ascending length difference from the query of lines 222-233 of
        // `sekka-symspell-search`):
        // ascending distance -> okuri-ari keys last (no `query-is-kana` branch is needed
        // here because the stem_kana reaching this function is always on the kana side) ->
        // ascending length difference from the query -> ascending kana key (the last stage
        // does not exist upstream; it is a deterministic tie-break aligned with the "ties by
        // ascending kana key" style fuzzy.rs already uses. CONTEXT.md Claude's Discretion).
        let stem_kana_len = stem_kana.chars().count() as isize;
        symspell_hits.sort_by(|(a_key, a_dist), (b_key, b_dist)| {
            a_dist
                .cmp(b_dist)
                .then_with(|| {
                    roman_index::ends_with_ascii_alpha(a_key)
                        .cmp(&roman_index::ends_with_ascii_alpha(b_key))
                })
                .then_with(|| {
                    let a_len_diff = (a_key.chars().count() as isize - stem_kana_len).abs();
                    let b_len_diff = (b_key.chars().count() as isize - stem_kana_len).abs();
                    a_len_diff.cmp(&b_len_diff)
                })
                .then_with(|| a_key.cmp(b_key))
        });

        // D-97: no cap on the count. Every key after the sort becomes a candidate.
        for (key, _dist) in &symspell_hits {
            for dict in &self.dictionaries {
                if let Ok(entries) = dict.lookup(key) {
                    if !entries.is_empty() {
                        for c in build_candidates(key, &entries, SYMSPELL_TIER_SCORE, 1) {
                            merge_candidate(&mut all_candidates, &mut seen, c);
                        }
                    }
                }
            }
        }

        // D-57/D-59: fuzzy search between romaji strings. The query is the raw romaji
        // lowercased (the raw-roman of upstream sekka-henkan--okuri-nashi).
        let query = stem_romaji.to_lowercase();
        let Some(prefix) = roman_index::query_prefix(&query) else {
            return all_candidates;
        };

        let mut bucket: Vec<RomanKey> = Vec::new();
        for dict in &self.dictionaries {
            if let Ok(results) = dict.roman_bucket(&prefix) {
                for rk in results {
                    // Exclude keys already handled by exact matching.
                    if rk.reading != exact_key {
                        bucket.push(rk);
                    }
                }
            }
        }

        let fuzzy_matches = fuzzy_filter_roman(&query, &bucket, &FuzzySearchConfig::default());

        // okuri_kana is necessarily empty here (we only reach this point when okuri_romaji is empty, D-74).
        for fm in &fuzzy_matches {
            for dict in &self.dictionaries {
                if let Ok(entries) = dict.lookup(&fm.key) {
                    if !entries.is_empty() {
                        let candidates = build_candidates(&fm.key, &entries, fm.score, 2);
                        for c in candidates {
                            merge_candidate(&mut all_candidates, &mut seen, c);
                        }
                    }
                }
            }
        }

        all_candidates
    }

    /// Helper that converts a romaji string into kana
    fn romaji_to_kana(&self, romaji: &str) -> String {
        let mut converter = RomajiConverter::new();
        let mut result = String::new();
        for ch in romaji.chars() {
            for s in converter.feed(ch) {
                result.push_str(&s);
            }
        }
        for s in converter.flush() {
            result.push_str(&s);
        }
        result
    }

    /// Updates the preedit with the currently selected candidate
    fn update_preedit_from_candidate(&mut self) {
        if self.candidate_index >= 0 && (self.candidate_index as usize) < self.candidates.len() {
            self.preedit = self.candidates[self.candidate_index as usize]
                .display
                .clone();
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)] // so that test names can contain state names (Input and so on)
mod tests {
    use super::*;
    use crate::dictionary::dict_format;
    use crate::dictionary::immutable_dict::ImmutableFileDict;
    use crate::dictionary::user_dict::UserDict;
    use crate::dictionary::{DictEntry, DictError, Dictionary, DictionaryMode, RomanKey};
    use crate::kana_romaji::kana_to_hepburn;
    use crate::roman_index::MIN_INDEX_ROMAN_LEN;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock dictionary for tests
    ///
    /// A test-only dictionary that keeps a reading -> entries mapping in memory.
    struct MockDictionary {
        entries: Vec<(String, Vec<DictEntry>)>,
        path: PathBuf,
        /// How many times `save()` was called (for the delegation test of `save_dictionaries`).
        save_calls: AtomicUsize,
        /// When `true`, `save()` returns an error (for the error propagation test of `save_dictionaries`).
        save_should_fail: bool,
    }

    impl MockDictionary {
        /// Creates an empty mock dictionary
        fn new() -> Self {
            MockDictionary {
                entries: Vec::new(),
                path: PathBuf::from("/tmp/mock_dict"),
                save_calls: AtomicUsize::new(0),
                save_should_fail: false,
            }
        }

        /// Makes the mock return a failure every time `save()` is called
        fn with_save_failure(mut self) -> Self {
            self.save_should_fail = true;
            self
        }

        /// Returns how many times `save()` was called
        fn save_call_count(&self) -> usize {
            self.save_calls.load(Ordering::SeqCst)
        }

        /// Adds a reading/entry pair
        fn add_entry(&mut self, reading: &str, word: &str) {
            if let Some((_, entries)) = self.entries.iter_mut().find(|(k, _)| k == reading) {
                entries.push(DictEntry::new(word));
            } else {
                self.entries
                    .push((reading.to_string(), vec![DictEntry::new(word)]));
            }
        }

        /// Adds a reading/entry pair with a frequency
        fn add_entry_with_frequency(&mut self, reading: &str, word: &str, freq: u32) {
            if let Some((_, entries)) = self.entries.iter_mut().find(|(k, _)| k == reading) {
                entries.push(DictEntry::new(word).with_frequency(freq));
            } else {
                self.entries.push((
                    reading.to_string(),
                    vec![DictEntry::new(word).with_frequency(freq)],
                ));
            }
        }
    }

    impl Dictionary for MockDictionary {
        fn lookup(&self, reading: &str) -> Result<Vec<DictEntry>, DictError> {
            Ok(self
                .entries
                .iter()
                .find(|(k, _)| k == reading)
                .map(|(_, v)| v.clone())
                .unwrap_or_default())
        }

        fn prefix_search(&self, prefix: &str) -> Result<Vec<(String, Vec<DictEntry>)>, DictError> {
            Ok(self
                .entries
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .cloned()
                .collect())
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn mode(&self) -> DictionaryMode {
            DictionaryMode::ReadOnly
        }

        /// Records how many times `save()` was called (for tests).
        /// Always returns an error when `save_should_fail` is `true`.
        fn save(&self) -> Result<(), DictError> {
            self.save_calls.fetch_add(1, Ordering::SeqCst);
            if self.save_should_fail {
                Err(DictError::SerializationError(
                    "mock save failure".to_string(),
                ))
            } else {
                Ok(())
            }
        }

        /// Reads a romaji index bucket (for tests).
        ///
        /// The exclusion rules are kept identical to `is_roman_index_eligible` in
        /// `dictionary::dict_format::write_dict` (02.1-01): (a) the canonical romaji length
        /// is at least `MIN_INDEX_ROMAN_LEN`, and (b) the last character of the kana key is
        /// not an ASCII letter (excluding the okuri-ari conventional keys of SKK-JISYO.L,
        /// `かんj` / `おこなu`). If these two conditions drift apart, the context tests would
        /// pin behaviour that differs from the real dictionary.
        fn roman_bucket(&self, roman_prefix: &str) -> Result<Vec<RomanKey>, DictError> {
            let mut result = Vec::new();
            for (reading, _) in &self.entries {
                let roman = kana_to_hepburn(reading);
                let roman_len_ok = roman.chars().count() >= MIN_INDEX_ROMAN_LEN;
                let last_char_is_ascii_alpha = reading
                    .chars()
                    .next_back()
                    .map(|c| c.is_ascii_alphabetic())
                    .unwrap_or(false);
                if roman_len_ok && !last_char_is_ascii_alpha && roman.starts_with(roman_prefix) {
                    result.push(RomanKey {
                        reading: reading.clone(),
                        roman,
                    });
                }
            }
            Ok(result)
        }
    }

    /// Feeds romaji input, presses Ctrl-J (staging it in the preedit) and Ctrl-J again
    /// (reselection), then returns the string of the first candidate placed in the preedit.
    /// The caller can assert further on the return value and the state (revised D-08:
    /// Ctrl-J only stages into the preedit without committing, so this also confirms the
    /// precondition that poll_output stays None).
    fn commit_then_reselect(ctx: &mut SekkaContext, input: &str) -> String {
        for ch in input.chars() {
            ctx.process_key(ch, false);
        }
        let staged = ctx.process_key('\0', true);
        assert!(
            staged,
            "Ctrl-J should place the first candidate in the preedit"
        );
        assert!(
            ctx.poll_output().is_none(),
            "with the deferred commit, poll_output should be None right after Ctrl-J"
        );
        let committed = ctx.get_preedit().to_string();
        let entered = ctx.process_key('\0', true);
        assert!(
            entered,
            "a second Ctrl-J in the commit display state should enter reselection"
        );
        committed
    }

    // === State transition tests ===

    #[test]
    fn the_initial_state_is_input() {
        let ctx = SekkaContext::new();
        assert_eq!(ctx.state(), ConversionState::Input);
        assert_eq!(ctx.get_candidate_index(), -1);
        assert!(ctx.get_candidates().is_empty());
        assert_eq!(ctx.get_preedit(), "");
    }

    #[test]
    fn ctrl_j_stages_the_first_candidate_without_committing() {
        let mut ctx = SekkaContext::new();
        // Romaji input.
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        assert_eq!(ctx.state(), ConversionState::Input);

        // Ctrl-J places the first candidate in the preedit (no candidate window and no
        // committed_output either; revised D-08).
        let consumed = ctx.process_key('\0', true);
        assert!(consumed);
        assert_eq!(ctx.state(), ConversionState::Input);
        assert!(ctx.get_candidates().is_empty());
        assert_eq!(ctx.get_candidate_index(), -1);
        assert_eq!(ctx.get_preedit(), "か");

        let output = ctx.poll_output();
        assert!(output.is_none());
    }

    #[test]
    fn confirming_moves_from_selecting_to_input() {
        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.confirm();
        assert_eq!(ctx.state(), ConversionState::Input);
    }

    #[test]
    fn cancelling_moves_from_selecting_to_input() {
        let mut ctx = SekkaContext::new();
        let committed = commit_then_reselect(&mut ctx, "ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.cancel();
        assert_eq!(ctx.state(), ConversionState::Input);
        // Cancelling does not commit; it only puts the original first candidate back in the preedit (revised D-08).
        assert_eq!(ctx.get_preedit(), committed);
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn the_full_state_transition_cycle() {
        // Input -> (staged in the preedit) -> Input -> reselection (Selecting) -> confirm -> Input
        let mut ctx = SekkaContext::new();
        assert_eq!(ctx.state(), ConversionState::Input);

        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert_eq!(ctx.state(), ConversionState::Input);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "あ");

        ctx.process_key('\0', true);
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.confirm();
        assert_eq!(ctx.state(), ConversionState::Input);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "あ");
    }

    // === Romaji buffer and preedit tests ===

    #[test]
    fn romaji_input_updates_the_preedit() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        assert_eq!(ctx.get_preedit(), "k");

        ctx.process_key('a', false);
        assert_eq!(ctx.get_preedit(), "ka");

        ctx.process_key('n', false);
        assert_eq!(ctx.get_preedit(), "kan");

        ctx.process_key('j', false);
        assert_eq!(ctx.get_preedit(), "kanj");

        ctx.process_key('i', false);
        assert_eq!(ctx.get_preedit(), "kanji");
    }

    #[test]
    fn reset_clears_all_state() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);

        ctx.reset();
        assert_eq!(ctx.state(), ConversionState::Input);
        assert_eq!(ctx.get_preedit(), "");
        assert!(ctx.get_candidates().is_empty());
        assert_eq!(ctx.get_candidate_index(), -1);
        assert!(ctx.poll_output().is_none());
    }

    // === Empty input handling tests ===

    #[test]
    fn empty_input_does_not_convert() {
        let mut ctx = SekkaContext::new();
        let consumed = ctx.process_key('\0', true);
        assert!(!consumed);
        assert_eq!(ctx.state(), ConversionState::Input);
    }

    // === Hiragana-only mode conversion tests ===

    #[test]
    fn all_lowercase_input_stages_hiragana_as_the_first_candidate() {
        let mut ctx = SekkaContext::new();
        for ch in "kanji".chars() {
            ctx.process_key(ch, false);
        }
        let consumed = ctx.process_key('\0', true);
        assert!(consumed);
        assert_eq!(ctx.state(), ConversionState::Input);

        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "かんじ");
    }

    // === Uppercase pattern conversion tests ===

    #[test]
    fn an_uppercase_head_gives_kanji_conversion_mode() {
        // "Kanji" -> KanjiConvert mode
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "感じ");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        // Check the candidate list of the Selecting state entered through reselection.
        let candidates = ctx.get_candidates();
        // Dictionary candidates + hiragana + katakana
        assert!(candidates.len() >= 3);
        // The dictionary candidates are present.
        assert!(candidates.iter().any(|c| c.display == "漢字"));
        assert!(candidates.iter().any(|c| c.display == "感じ"));
        // The fallback candidates are present.
        assert!(candidates.iter().any(|c| c.display == "かんじ"));
        assert!(candidates.iter().any(|c| c.display == "カンジ"));
    }

    #[test]
    fn lowercase_head_with_uppercase_inside_gives_conversion_with_okurigana() {
        // "kanJi" -> KanjiWithOkuri mode
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんj", "感");
        dict.add_entry("かん", "缶");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "kanJi");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        let candidates = ctx.get_candidates();
        // The candidate derived from the okuri-ari key (かんj) is shown with its okurigana.
        assert!(candidates.iter().any(|c| c.display == "感じ"));
        // No candidate derived from the unrelated okuri-nashi reading key (かん) creeps in (G-01.1-6b).
        assert!(!candidates.iter().any(|c| c.display == "缶じ"));
    }

    #[test]
    fn uppercase_head_and_tail_gives_the_okurigana_marker() {
        // "OkonaU" -> OkuriMarker mode
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("おこなu", "行");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "OkonaU");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        let candidates = ctx.get_candidates();
        // The dictionary candidate is shown with its okurigana.
        assert!(candidates.iter().any(|c| c.display == "行う"));
    }

    #[test]
    fn kanji_conversion_mode_without_a_dictionary_yields_only_fallback_candidates() {
        // "Kanji", but the dictionary is empty.
        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        let candidates = ctx.get_candidates();
        // Only the hiragana and katakana fallback candidates.
        assert!(candidates.len() >= 2);
        assert!(candidates.iter().any(|c| c.display == "かんじ"));
        assert!(candidates.iter().any(|c| c.display == "カンジ"));
    }

    // === Candidate selection tests ===

    #[test]
    fn next_and_previous_candidate_change_the_selection() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "感じ");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        // The initial selection of reselection is the candidate just committed (the first one).
        assert_eq!(ctx.get_candidate_index(), 0);

        // Next candidate.
        ctx.next_candidate();
        assert_eq!(ctx.get_candidate_index(), 1);

        // Previous candidate.
        ctx.prev_candidate();
        assert_eq!(ctx.get_candidate_index(), 0);
    }

    #[test]
    fn candidate_selection_wraps_around() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("か", "蚊");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        let count = ctx.get_candidates().len() as i32;
        // Advance to the end.
        for _ in 0..count - 1 {
            ctx.next_candidate();
        }
        assert_eq!(ctx.get_candidate_index(), count - 1);

        // One more wraps back to the start.
        ctx.next_candidate();
        assert_eq!(ctx.get_candidate_index(), 0);

        // Going back from the start moves to the end.
        ctx.prev_candidate();
        assert_eq!(ctx.get_candidate_index(), count - 1);
    }

    // === Commit and output tests ===

    #[test]
    fn confirming_stages_the_candidate_in_the_preedit_without_committing() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry_with_frequency("かんじ", "漢字", 10);
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        // Move the candidate selected during reselection into the commit display state (revised D-08; nothing is sent to the application yet).
        ctx.confirm();

        assert!(ctx.poll_output().is_none());
        assert!(!ctx.get_preedit().is_empty());
    }

    #[test]
    fn it_is_staged_in_the_preedit_in_hiragana_only_mode() {
        let mut ctx = SekkaContext::new();
        for ch in "kanji".chars() {
            ctx.process_key(ch, false);
        }
        ctx.process_key('\0', true);

        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "かんじ");
    }

    // === Input refusal tests for the Selecting state ===

    #[test]
    fn character_input_is_refused_in_the_selecting_state() {
        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "a");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        // Character input is not processed in the Selecting state.
        let consumed = ctx.process_key('b', false);
        assert!(!consumed);
    }

    // === Repeated conversion cycle tests ===

    #[test]
    fn input_and_conversion_work_again_after_a_commit() {
        let mut ctx = SekkaContext::new();

        // First cycle: only staged in the preedit, not committed.
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "あ");

        // The next character input flushes it and new romaji input begins (D-12).
        ctx.process_key('i', false);
        let out1 = ctx.poll_output();
        assert_eq!(out1, Some("あ".to_string()));
        assert_eq!(ctx.get_preedit(), "i");

        // Second cycle.
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "い");
    }

    #[test]
    fn conversion_works_again_after_cancelling() {
        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.cancel();
        assert_eq!(ctx.state(), ConversionState::Input);

        // After cancelling, typing something new flushes the word that cancelling had put
        // back into the preedit (D-12) and conversion proceeds as usual.
        ctx.process_key('i', false);
        assert!(ctx.poll_output().is_some());

        let consumed = ctx.process_key('\0', true);
        assert!(consumed);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "い");
    }

    // === Preedit update tests ===

    #[test]
    fn selecting_a_candidate_updates_the_preedit() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("か", "蚊");
        dict.add_entry("か", "火");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        // The initial preedit is the first candidate.
        let initial_preedit = ctx.get_preedit().to_string();
        assert!(!initial_preedit.is_empty());

        // The next candidate changes the preedit.
        ctx.next_candidate();
        let next_preedit = ctx.get_preedit().to_string();
        // With several candidates the preedit should change
        // (what ends up first depends on the dictionary, through the sort order).
        assert!(!next_preedit.is_empty());
    }

    // === Immediate commit, reselection and cancel tests (D-04/D-06/D-08) ===

    #[test]
    fn a_second_ctrl_j_right_after_a_commit_enters_reselection_mode() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('K', false);
        ctx.process_key('a', false);
        let consumed = ctx.process_key('\0', true);
        assert!(consumed);
        assert!(ctx.poll_output().is_none());

        let entered = ctx.process_key('\0', true);
        assert!(entered);
        assert_eq!(ctx.state(), ConversionState::Selecting);
        assert_eq!(ctx.get_candidate_index(), 0);
        assert_eq!(ctx.get_preedit(), "か");
        // Entering reselection causes no commit at all (stage 1 of D-18).
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn a_multi_character_committed_word_is_not_committed_when_entering_reselection() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        ctx.add_dictionary(Arc::new(dict));

        for ch in "Kanji".chars() {
            ctx.process_key(ch, false);
        }
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "漢字");

        let entered = ctx.process_key('\0', true);
        assert!(entered);
        assert_eq!(ctx.state(), ConversionState::Selecting);
        // Even for a multi-character committed word such as 「漢字」, entering reselection causes no commit.
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn ctrl_j_during_reselection_advances_to_the_next_candidate() {
        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "Ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);
        assert_eq!(ctx.get_candidate_index(), 0);

        let consumed = ctx.process_key('\0', true);
        assert!(consumed);
        assert_eq!(ctx.state(), ConversionState::Selecting);
        assert_eq!(ctx.get_candidate_index(), 1);
    }

    #[test]
    fn cancelling_returns_to_the_preedit_of_the_originally_committed_word() {
        let mut ctx = SekkaContext::new();
        let committed = commit_then_reselect(&mut ctx, "Ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        // Even after advancing to the next candidate, cancelling returns to the originally committed word.
        ctx.next_candidate();
        ctx.cancel();
        assert_eq!(ctx.state(), ConversionState::Input);
        assert_eq!(ctx.get_preedit(), committed);
        assert!(ctx.poll_output().is_none());

        // A Ctrl-J right after cancelling can reselect again.
        let entered = ctx.process_key('\0', true);
        assert!(entered);
        assert_eq!(ctx.state(), ConversionState::Selecting);
    }

    #[test]
    fn confirming_updates_the_last_commit_to_the_selected_candidate() {
        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "Ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.next_candidate();
        ctx.confirm();
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "カ");

        let entered = ctx.process_key('\0', true);
        assert!(entered);
        assert_eq!(ctx.state(), ConversionState::Selecting);
        assert_eq!(ctx.get_candidate_index(), 1);
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn character_input_invalidates_the_last_commit() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        ctx.poll_output();
        assert!(ctx.last_commit.is_some());

        ctx.process_key('i', false);
        assert!(ctx.last_commit.is_none());
    }

    #[test]
    fn reset_invalidates_the_last_commit() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        ctx.poll_output();
        assert!(ctx.last_commit.is_some());

        ctx.reset();
        // The romaji buffer is empty too, so a Ctrl-J with an invalidated last commit is not consumed.
        let consumed = ctx.process_key('\0', true);
        assert!(!consumed);
    }

    // === Candidate composition and order (D-07/D-05) ===

    #[test]
    fn all_lowercase_input_starts_with_hiragana_and_ends_with_kanji() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        ctx.add_dictionary(Arc::new(dict));

        let committed = commit_then_reselect(&mut ctx, "kanji");
        assert_eq!(committed, "かんじ");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec!["かんじ", "カンジ", "ｋａｎｊｉ", "kanji", "漢字"]
        );
    }

    #[test]
    fn with_uppercase_the_candidates_start_with_kanji_and_end_with_the_alphabet() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "感じ");
        ctx.add_dictionary(Arc::new(dict));

        let committed = commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(committed, "漢字");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec!["漢字", "感じ", "かんじ", "カンジ", "Ｋａｎｊｉ", "Kanji"]
        );
    }

    #[test]
    fn the_alphabet_candidate_for_inner_uppercase_keeps_the_typed_case() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんj", "感");
        ctx.add_dictionary(Arc::new(dict));

        let committed = commit_then_reselect(&mut ctx, "kanJi");
        assert_eq!(committed, "感じ");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec!["感じ", "かんじ", "カンジ", "ｋａｎＪｉ", "kanJi"]
        );
    }

    #[test]
    fn the_candidate_composition_of_the_okurigana_marker() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("おこなu", "行");
        ctx.add_dictionary(Arc::new(dict));

        let committed = commit_then_reselect(&mut ctx, "OkonaU");
        assert_eq!(committed, "行う");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(displays[0], "行う");
        let len = displays.len();
        assert_eq!(&displays[len - 2..], ["ＯｋｏｎａＵ", "OkonaU"]);
    }

    // === D-57/D-59: regression for fuzzy search between romaji strings (measured in the ROADMAP Background) ===

    /// RED target (02.1-02 Task 2): a regression test that the typo `Nihogno` reaches the
    /// `日本語` entry of `にほんご`. It prevents a recurrence of what the ROADMAP Background
    /// measured: "the typo Nihogno -> 二歩 (no way back to the intended 日本語)".
    ///
    /// Until `MockDictionary` overrides `Dictionary::roman_bucket`, the default
    /// implementation (returning nothing) applies, the fuzzy search path silently verifies
    /// nothing and this test fails (exactly as the key_links warning of 02.1-01 says).
    #[test]
    fn the_typo_nihogno_reaches_nihongo() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("にほんご", "日本語");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Nihogno");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert!(
            displays.contains(&"日本語"),
            "the candidate list for the typo Nihogno should contain 日本語: {:?}",
            displays
        );
    }

    // === Regression tests for a real SKK-JISYO.L setup (G-01.1-6a / G-01.1-6b) ===

    /// Mock dictionary reproducing the real data layout of SKK-JISYO.L
    ///
    /// Okuri-ari verbs have a separate key per conjugating consonant and are separate
    /// entries from the okuri-nashi reading key. Built by inspecting the real SKK-JISYO.L
    /// directly (.planning/debug/g-01.1-6a-candidate-dedup.md,
    /// .planning/debug/g-01.1-6b-kanji-ranking.md).
    fn realistic_mock_dictionary() -> MockDictionary {
        let mut dict = MockDictionary::new();
        // 「行う」 has 8 keys (one per conjugating consonant). Only w/u/i also carry 「行な」.
        dict.add_entry("おこなw", "行");
        dict.add_entry("おこなw", "行な");
        dict.add_entry("おこなu", "行");
        dict.add_entry("おこなu", "行な");
        dict.add_entry("おこなi", "行");
        dict.add_entry("おこなi", "行な");
        dict.add_entry("おこなt", "行");
        dict.add_entry("おこなo", "行");
        dict.add_entry("おこなh", "行");
        dict.add_entry("おこなe", "行");
        dict.add_entry("おこなc", "行");
        // 「おこない」 is the okuri-nashi reading key (a separate entry).
        dict.add_entry("おこない", "行い");
        dict.add_entry("おこない", "行ない");
        // The okuri-ari key 「かんj」 (感 only) and the unrelated okuri-nashi noun key
        // 「かん」 (headed by 缶; this reproduces かん /缶/勘;山勘/間/感/観/.../ from the real
        // SKK-JISYO.L, including the fact that 感 is the fourth entry).
        dict.add_entry("かんj", "感");
        for word in ["缶", "勘", "間", "感", "観"] {
            dict.add_entry("かん", word);
        }
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "幹事");
        dict
    }

    #[test]
    fn okuri_ari_conversion_puts_the_okuri_ari_key_candidate_first() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(realistic_mock_dictionary()));

        let committed = commit_then_reselect(&mut ctx, "kanJi");
        assert_eq!(committed, "感じ");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec!["感じ", "かんじ", "カンジ", "ｋａｎＪｉ", "kanJi"]
        );
    }

    #[test]
    fn okuri_ari_conversion_does_not_mix_in_candidates_of_the_okuri_nashi_reading_key() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(realistic_mock_dictionary()));

        commit_then_reselect(&mut ctx, "kanJi");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        for forbidden in ["缶じ", "勘じ", "間じ", "観じ", "漢字じ", "幹事じ"] {
            assert!(
                !displays.contains(&forbidden),
                "{} crept in: {:?}",
                forbidden,
                displays
            );
        }
    }

    /// D-74: okuri-ari conversion (`KanJi`) does no fuzzy search at all, so not one
    /// candidate derived from かん (缶/勘/間/感/観) appears. Where the existing
    /// `okuri_ari_conversion_does_not_mix_in_candidates_of_the_okuri_nashi_reading_key`
    /// checks only the spellings after 「じ」 is appended, this test confirms - using the
    /// bare spellings of かん - that the early return of D-74 keeps the bucket search from
    /// ever running.
    #[test]
    fn okuri_ari_conversion_shows_no_candidate_derived_from_kan() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(realistic_mock_dictionary()));

        commit_then_reselect(&mut ctx, "KanJi");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        for forbidden in ["缶", "勘", "間", "感", "観"] {
            assert!(
                !displays.contains(&forbidden),
                "{} crept in (a D-74 violation): {:?}",
                forbidden,
                displays
            );
        }
    }

    #[test]
    fn okuri_nashi_conversion_does_not_mix_in_okuri_ari_key_candidates() {
        // D-77: 「行い」 (おこない, canonical romaji okonai) was among the candidates in the
        // old version of this test through kana-to-kana fuzzy search at threshold 0.8, but
        // with romaji-to-romaji search at threshold 0.94 (as upstream) it disappears,
        // because jaro_winkler("okonau", "okonai") = 0.9333... (measured) is below 0.94. The
        // core of this test - that okuri-ari keys never creep in (おこなw/u/i/t/o/h/e/c end
        // in an ASCII letter and are excluded from the index, so they can never become
        // candidates) - is unchanged, so only that assertion is kept.
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(realistic_mock_dictionary()));

        commit_then_reselect(&mut ctx, "Okonau");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert!(!displays.contains(&"行"));
        assert!(!displays.contains(&"行な"));
    }

    #[test]
    fn candidate_spellings_do_not_duplicate_in_a_realistic_setup() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(realistic_mock_dictionary()));

        let committed = commit_then_reselect(&mut ctx, "OkonaU");
        assert_eq!(committed, "行う");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec![
                "行う",
                "行なう",
                "おこなう",
                "オコナウ",
                "ＯｋｏｎａＵ",
                "OkonaU"
            ]
        );
    }

    #[test]
    fn the_same_spelling_in_several_dictionaries_yields_one_candidate() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(realistic_mock_dictionary()));
        // In addition to the master dictionary, a second dictionary standing in for a user dictionary with the same spelling 「行」.
        let mut dict2 = MockDictionary::new();
        dict2.add_entry("おこなu", "行");
        ctx.add_dictionary(Arc::new(dict2));

        let committed = commit_then_reselect(&mut ctx, "OkonaU");
        assert_eq!(committed, "行う");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec![
                "行う",
                "行なう",
                "おこなう",
                "オコナウ",
                "ＯｋｏｎａＵ",
                "OkonaU"
            ]
        );
    }

    #[test]
    fn deduplication_keeps_the_higher_frequency_and_score() {
        let mut ctx = SekkaContext::new();
        let mut dict1 = MockDictionary::new();
        dict1.add_entry("かんj", "感");
        ctx.add_dictionary(Arc::new(dict1));
        let mut dict2 = MockDictionary::new();
        dict2.add_entry_with_frequency("かんj", "感", 7);
        ctx.add_dictionary(Arc::new(dict2));

        commit_then_reselect(&mut ctx, "kanJi");

        let matches: Vec<&Candidate> = ctx
            .get_candidates()
            .iter()
            .filter(|c| c.display == "感じ")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].frequency, 7);
        assert_eq!(matches[0].score, 1.0);
    }

    #[test]
    fn merge_candidate_does_not_carry_a_fuzzy_frequency_into_an_exact_match() {
        // D-147: a fuzzy-stage duplicate (tier 1/2) of an exact match (tier 0) must
        // not lift the exact match's frequency. Calls `merge_candidate` directly
        // (through `use super::*`) rather than going through a dictionary lookup,
        // to pin the merge rule itself regardless of push order.
        let mut all: Vec<Candidate> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "角".to_string(),
                reading: "かく".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 0,
                tier: 0,
                learn_pair: Some(("かく".to_string(), "角".to_string())),
            },
        );
        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "角".to_string(),
                reading: "かど".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 5,
                tier: 1,
                learn_pair: Some(("かど".to_string(), "角".to_string())),
            },
        );

        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0].frequency, 0,
            "a fuzzy-stage frequency (learned under a different reading) must not be carried into the exact match"
        );
        assert_eq!(all[0].tier, 0);
        assert_eq!(
            all[0].learn_pair,
            Some(("かく".to_string(), "角".to_string())),
            "learn_pair stays first-wins even across the stage boundary"
        );
    }

    #[test]
    fn merge_candidate_folds_frequency_by_maximum_within_the_same_stage() {
        // D-147/D-148: duplicates that share the same match stage still fold their
        // frequency by maximum, exactly as before D-147. Tier 1 and tier 2 share
        // the fuzzy stage, so a tier-1/tier-2 duplicate also folds by maximum.
        let mut all: Vec<Candidate> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 0,
                tier: 0,
                learn_pair: None,
            },
        );
        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "漢字".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 7,
                tier: 0,
                learn_pair: None,
            },
        );
        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "幹事".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 2,
                tier: 1,
                learn_pair: None,
            },
        );
        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "幹事".to_string(),
                reading: "かんじ".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.95,
                frequency: 4,
                tier: 2,
                learn_pair: None,
            },
        );

        let kanji = all.iter().find(|c| c.display == "漢字").unwrap();
        assert_eq!(kanji.frequency, 7);
        assert_eq!(kanji.tier, 0);

        let kanji_ = all.iter().find(|c| c.display == "幹事").unwrap();
        assert_eq!(
            kanji_.frequency, 4,
            "tier 1 and tier 2 share the fuzzy stage, so they still fold by maximum"
        );
        assert_eq!(
            kanji_.tier, 1,
            "tier folds to the minimum of the involved candidates (D-147, Pitfall 7)"
        );
    }

    #[test]
    fn merge_candidate_takes_the_exact_match_frequency_and_tier_when_an_exact_match_arrives_after_a_fuzzy_one(
    ) {
        // D-147/D-148 (Pitfall 2/7): the reverse push order (fuzzy, then exact) is
        // not reachable through the current `lookup_dictionary` call order (tier 0
        // -> 1 -> 2, Open Items Resolved 4 of 08-RESEARCH.md), but `merge_candidate`
        // must not depend on that order to produce the correct result: only the
        // exact match's frequency and tier survive, and score still folds by
        // maximum.
        let mut all: Vec<Candidate> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();

        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "角".to_string(),
                reading: "かど".to_string(),
                kind: CandidateKind::Kanji,
                score: 0.95,
                frequency: 5,
                tier: 2,
                learn_pair: Some(("かど".to_string(), "角".to_string())),
            },
        );
        merge_candidate(
            &mut all,
            &mut seen,
            Candidate {
                display: "角".to_string(),
                reading: "かく".to_string(),
                kind: CandidateKind::Kanji,
                score: 1.0,
                frequency: 1,
                tier: 0,
                learn_pair: Some(("かく".to_string(), "角".to_string())),
            },
        );

        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0].tier, 0,
            "tier folds to the minimum (the exact match's tier), Pitfall 7"
        );
        assert_eq!(
            all[0].frequency, 1,
            "only the exact match's own frequency survives when it arrives after a fuzzy duplicate"
        );
        assert_eq!(all[0].score, 1.0, "score still folds by maximum");
        assert_eq!(
            all[0].learn_pair,
            Some(("かく".to_string(), "角".to_string())),
            "learn_pair also takes the exact match's own reading when it arrives after a fuzzy duplicate (D-147)"
        );
    }

    #[test]
    fn the_4_mandatory_fallbacks_stay_within_the_first_10_in_a_realistic_setup() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(realistic_mock_dictionary()));

        commit_then_reselect(&mut ctx, "OkonaU");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .take(10)
            .map(|c| c.display.as_str())
            .collect();
        for fallback in ["おこなう", "オコナウ", "ＯｋｏｎａＵ", "OkonaU"] {
            assert!(
                displays[..10.min(displays.len())].contains(&fallback),
                "{} is not among the first 10: {:?}",
                fallback,
                displays
            );
        }
    }

    #[test]
    fn pins_the_candidate_lists_of_branches_2_and_5_in_a_realistic_setup() {
        // Pins, in order, the candidate lists of branch 2 (uppercase head, okurigana
        // marker) and branch 5 (all lowercase) as they were before the dispatcher was
        // introduced, in a realistic setup (several dictionary keys, fuzzy search results
        // included) - a characterization test guaranteeing that not one entry changes
        // across the introduction of the dispatcher (D-24).
        //
        // D-77 (measured in 02.1-02 Task 2): 「缶/勘/間/感/観」 (かん, canonical romaji
        // "kan", 3 characters) were among the candidates in the old version through
        // kana-to-kana fuzzy search (threshold 0.8), but with romaji-to-romaji search and
        // the length-difference cutoff (min/max >= 0.70) they are dropped by
        // roman_index::length_ok(5, 3) = (100*3=300) < (70*5=350) (the query "kanji" is 5
        // characters, the romaji of 「かん」 is 3).
        //
        // 03.1-02 (this plan; the measurement record of D-89 (2)): through SymSpell path 2
        // (one delete variant of `symspell::delete_variants("かんじ")`, 「かん」, hits a real
        // dictionary key at distance 1), 「缶/勘/間/感/観」 came back as tier-1 candidates.
        // The same words D-77 dropped from romaji-to-romaji JW (tier 2) are picked up, as
        // upstream does, by SymSpell (tier 1), which measures edit distance between kana -
        // the result of MockDictionary faithfully exercising path 2 of upstream
        // `sekka-symspell-search` (exactly as RESEARCH.md Pitfall 1 predicted). The first
        // candidate (the leading element; kanji -> 「かんじ」 / Kanji -> 「漢字」) is unchanged
        // in both cases, so this is handled by updating the expected values only (D-89 (3):
        // consult the user only when the first candidate changes; not the case here). The
        // expected values are taken straight from the actual `cargo test` output (never
        // written from guesswork).

        // Branch 5: all lowercase (かんじ has 漢字/幹事 plus the 5 かん entries from SymSpell path 2).
        let mut ctx_kanji = SekkaContext::new();
        ctx_kanji.add_dictionary(Arc::new(realistic_mock_dictionary()));
        commit_then_reselect(&mut ctx_kanji, "kanji");
        let displays_kanji: Vec<&str> = ctx_kanji
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays_kanji,
            vec![
                "かんじ",
                "カンジ",
                "ｋａｎｊｉ",
                "kanji",
                "漢字",
                "幹事",
                "缶",
                "勘",
                "間",
                "感",
                "観"
            ]
        );

        // Branch 2: uppercase head (the same 5 かん entries from SymSpell path 2 land inside
        // the dictionary group, right after the tier-0 漢字/幹事 and before the fallbacks).
        let mut ctx_kanji_upper = SekkaContext::new();
        ctx_kanji_upper.add_dictionary(Arc::new(realistic_mock_dictionary()));
        commit_then_reselect(&mut ctx_kanji_upper, "Kanji");
        let displays_kanji_upper: Vec<&str> = ctx_kanji_upper
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays_kanji_upper,
            vec![
                "漢字",
                "幹事",
                "缶",
                "勘",
                "間",
                "感",
                "観",
                "かんじ",
                "カンジ",
                "Ｋａｎｊｉ",
                "Kanji"
            ]
        );

        // Branch 2: okurigana marker (the longest list, where the several おこなw/u keys and
        // fuzzy search are all involved).
        let mut ctx_okona = SekkaContext::new();
        ctx_okona.add_dictionary(Arc::new(realistic_mock_dictionary()));
        commit_then_reselect(&mut ctx_okona, "OkonaU");
        let displays_okona: Vec<&str> = ctx_okona
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays_okona,
            vec![
                "行う",
                "行なう",
                "おこなう",
                "オコナウ",
                "ＯｋｏｎａＵ",
                "OkonaU"
            ]
        );
    }

    // === SymSpell tier 1 (03.1-01, path 4 only) ===

    /// The single path that removes the root cause of D-82: write a real-format dictionary
    /// file with `write_dict`, mmap it with `ImmutableFileDict::open`, and typing
    /// `Konnitiha` (whose kana conversion is 「こんいちは」) through `SekkaContext` finds
    /// 「こんにちは」 via SymSpell path 4 (they share the delete variant 「こんちは」), pushes
    /// it as a tier-1 candidate and reaches the first candidate 「今日は」 (the test name is
    /// chosen so it can be run alone with `cargo test --lib kunrei_konnitiha`).
    #[test]
    fn kunrei_konnitiha_becomes_kyouha_through_the_real_dictionary_format() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("こんにちは".to_string(), vec![DictEntry::new("今日は")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let committed = commit_then_reselect(&mut ctx, "Konnitiha");
        assert_eq!(committed, "今日は");
    }

    /// D-74: the SymSpell layer does not run at all for okuri-ari conversion (a non-empty
    /// `okuri_romaji`), because it is inserted after the existing early return. Calling
    /// `lookup_dictionary` directly with a non-empty `okuri_romaji` against the same
    /// 「こんにちは」 dictionary confirms that neither exact matching nor SymSpell runs and the
    /// candidates come back empty.
    #[test]
    fn the_symspell_layer_does_not_run_for_okuri_ari_conversion() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("こんにちは".to_string(), vec![DictEntry::new("今日は")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let candidates = ctx.lookup_dictionary("konnitiha", "こんいちは", "", "a");
        assert!(
            candidates.is_empty(),
            "for okuri-ari conversion SymSpell should not run and candidates should be empty: {:?}",
            candidates.iter().map(|c| &c.display).collect::<Vec<_>>()
        );
    }

    // === The four SymSpell paths and the upstream-conformant sort (03.1-02) ===

    // The romaji query is always "zzz". `BTreeMap::range("zz"..)` sorts after every
    // canonical Hepburn romaji in the dictionary index (which all start with a lowercase
    // letter), so the romaji fuzzy search via `roman_index::query_prefix` (tier 2) always
    // returns an empty bucket and SymSpell (tier 1) can be verified without any
    // interference from the romaji layer.

    /// Path 2: a delete variant of `stem_kana` is itself a dictionary key (distance fixed
    /// at 1). The delete variant 「かん」 of the query 「かんじ」 is itself a dictionary key, so
    /// it is picked up at distance 1 without going through index-hit recomputation.
    #[test]
    fn symspell_path_2_a_query_delete_variant_that_is_a_dictionary_key_is_picked_up_at_distance_1()
    {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かん".to_string(), vec![DictEntry::new("缶")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let candidates = ctx.lookup_dictionary("zzz", "かんじ", "", "");
        let displays: Vec<&str> = candidates.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["缶"],
            "path 2 (a delete variant that is itself a dictionary key) should pick it up at distance 1: {:?}",
            displays
        );
    }

    /// Path 3: `stem_kana` itself hits the index as "somebody's delete variant" (the case
    /// where the dictionary key is one character longer than the query). For the query
    /// 「とうきょ」, the dictionary key 「とうきょう」 is picked up through the index.
    #[test]
    fn symspell_path_3_a_dictionary_key_one_character_longer_is_picked_up_through_the_index() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("とうきょう".to_string(), vec![DictEntry::new("東京")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let candidates = ctx.lookup_dictionary("zzz", "とうきょ", "", "");
        let displays: Vec<&str> = candidates.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["東京"],
            "path 3 (the query hits the index) should pick it up: {:?}",
            displays
        );
    }

    /// Path 4: a delete variant of `stem_kana` hits the index (the substitution case). The
    /// query 「かんじ」 and the dictionary key 「かんj」 share the delete variant 「かん」.
    #[test]
    fn symspell_path_4_a_single_character_substitution_is_picked_up_through_the_index() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かんj".to_string(), vec![DictEntry::new("感")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let candidates = ctx.lookup_dictionary("zzz", "かんじ", "", "");
        let displays: Vec<&str> = candidates.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["感"],
            "path 4 (a delete variant hits the index; substitution) should pick it up: {:?}",
            displays
        );
    }

    /// A transposition hits the index but is not accepted because its edit distance is 2.
    /// The query 「あいう」 and the dictionary key 「あうい」 share the delete variants 「あい」 and
    /// 「あう」 and do hit the index, but the recomputation with `strsim::levenshtein` finds
    /// distance 2 and drops them.
    #[test]
    fn keys_at_symspell_distance_2_are_not_picked_up() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("あうい".to_string(), vec![DictEntry::new("試")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let candidates = ctx.lookup_dictionary("zzz", "あいう", "", "");
        assert!(
            candidates.is_empty(),
            "a key at distance 2 should be dropped by the recomputation even when it hits the index: {:?}",
            candidates.iter().map(|c| &c.display).collect::<Vec<_>>()
        );
    }

    /// D-94: within the same distance (1), okuri-ari conventional keys (ending in an ASCII
    /// letter) come last. For the query 「かんじ」, 「かん」 (not okuri-ari), picked up by path 2,
    /// comes before 「かんj」 (okuri-ari), picked up by path 4.
    #[test]
    fn symspell_sorts_okuri_ari_keys_last_within_the_same_distance() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かん".to_string(), vec![DictEntry::new("缶")]);
        entries.insert("かんj".to_string(), vec![DictEntry::new("感")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let candidates = ctx.lookup_dictionary("zzz", "かんじ", "", "");
        let displays: Vec<&str> = candidates.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(
            displays,
            vec!["缶", "感"],
            "candidates from a non-okuri-ari key should come before those from an okuri-ari key: {:?}",
            displays
        );
    }

    /// D-97: no cap is placed on the SymSpell layer. With a dictionary holding 10 keys at
    /// distance 1, all 10 become candidates. The 10 are picked up from the query 「あい」 at
    /// distance 1 through path 4 (they share the delete variant 「い」).
    #[test]
    fn no_cap_is_placed_on_the_symspell_count() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        let first_chars = ["い", "う", "え", "お", "か", "き", "く", "け", "こ", "さ"];
        for c in first_chars.iter() {
            let key = format!("{}い", c);
            let display = format!("{}い_candidate", c);
            entries.insert(key, vec![DictEntry::new(display)]);
        }
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let candidates = ctx.lookup_dictionary("zzz", "あい", "", "");
        assert_eq!(
            candidates.len(),
            10,
            "all 10 keys at distance 1 should become candidates, with no cap: {:?}",
            candidates.iter().map(|c| &c.display).collect::<Vec<_>>()
        );
    }

    /// Determinism: calling `lookup_dictionary` twice with the same dictionary and the same
    /// input returns the SymSpell-derived candidates in exactly the same order.
    #[test]
    fn the_symspell_order_is_deterministic() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let mut entries: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        entries.insert("かん".to_string(), vec![DictEntry::new("缶")]);
        entries.insert("かんj".to_string(), vec![DictEntry::new("感")]);
        entries.insert("かんじゃ".to_string(), vec![DictEntry::new("患者")]);
        let path = tmp.path().join("test.dict");
        dict_format::write_dict(&path, &entries).expect("write_dict failed");
        let dict = ImmutableFileDict::open(&path).expect("failed to open the dictionary");

        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        let displays_of = |ctx: &SekkaContext| -> Vec<String> {
            ctx.lookup_dictionary("zzz", "かんじ", "", "")
                .iter()
                .map(|c| c.display.clone())
                .collect()
        };

        let first = displays_of(&ctx);
        let second = displays_of(&ctx);
        assert_eq!(
            first, second,
            "looking up the same input twice should return exactly the same order: {:?} vs {:?}",
            first, second
        );
    }

    // === The symbol branch (branch 6, D-22 to D-25) ===

    #[test]
    fn the_dot_symbol_becomes_the_dictionary_symbol_candidate_list_on_ctrl_j() {
        // Tracer: typing 「.」 and pressing Ctrl-J looks up the dictionary key 「.」 and
        // yields the same 6 candidates in the same order as upstream
        // emacs/sekka-tests.el:686, walking the whole path end to end - key acceptance
        // (is_romaji_char) -> the dispatcher (classify_input_shape) -> exact-match
        // dictionary lookup (build_symbol_candidates/lookup_dictionary_exact).
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry(".", "．");
        dict.add_entry(".", "・");
        dict.add_entry(".", "。");
        dict.add_entry(".", "…");
        ctx.add_dictionary(Arc::new(dict));

        let committed = commit_then_reselect(&mut ctx, ".");
        assert_eq!(committed, "．");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(displays, vec!["．", "・", "。", "…", "．", "."]);
    }

    #[test]
    fn the_comma_symbol_becomes_the_dictionary_symbol_candidate_list_on_ctrl_j() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry(",", "，");
        dict.add_entry(",", "、");
        ctx.add_dictionary(Arc::new(dict));

        let committed = commit_then_reselect(&mut ctx, ",");
        assert_eq!(committed, "，");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(displays, vec!["，", "、", "，", ","]);
    }

    #[test]
    fn a_mixed_buffer_produces_no_hiragana_candidate_and_falls_to_the_symbol_branch() {
        // D-26: "nihongodesu." makes is_strictly_convertible return false, so it falls to
        // branch 6 (symbols); the exact-match lookup misses and only the full-width and
        // half-width alphabet candidates remain. No hiragana candidate 「にほんごです.」 is
        // produced (nothing is registered in the dictionary, so this can be verified even
        // though the key itself does not exist).
        let mut ctx = SekkaContext::new();

        let committed = commit_then_reselect(&mut ctx, "nihongodesu.");
        assert_eq!(committed, "ｎｉｈｏｎｇｏｄｅｓｕ．");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(displays, vec!["ｎｉｈｏｎｇｏｄｅｓｕ．", "nihongodesu."]);
    }

    #[test]
    fn the_symbol_branch_runs_no_fuzzy_search() {
        // D-23: branch 6 (symbols) is exact-match only. Even with another key that could
        // fuzzy-match (「!」, one character away, both accepted symbols of D-20) in the
        // dictionary, its value does not creep into the candidates.
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry(".", "．");
        dict.add_entry(".", "・");
        dict.add_entry(".", "。");
        dict.add_entry(".", "…");
        dict.add_entry("!", "。"); // another key that could fuzzy-match
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, ".");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec!["．", "・", "。", "…", "．", "."],
            "there should be exactly 6: the 4 exact matches of the key . plus the 2 alphabet candidates: {:?}",
            displays
        );
    }

    // === The digits-only branch (branch 3, D-21) ===

    #[test]
    fn digits_only_yields_exactly_4_candidates_on_ctrl_j() {
        // D-21/Pitfall 6: no dictionary is consulted at all. There are exactly 4 candidates,
        // #1 (full-width) -> the bare half-width digits -> #2 (per-digit kanji numerals) ->
        // #3 (positional kanji numerals) (no full-width/half-width alphabet candidates are
        // added - the same asymmetry as upstream emacs/sekka-henkan.el:273-279).
        let mut ctx = SekkaContext::new();
        let committed = commit_then_reselect(&mut ctx, "2023");
        assert_eq!(committed, "２０２３");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec!["２０２３", "2023", "二〇二三", "二千二十三"],
            "the count should be exactly 4 (no alphabet candidates mixed in): {:?}",
            displays
        );
    }

    #[test]
    fn digit_leading_input_containing_uppercase_takes_branch_2() {
        // RESEARCH.md Pitfall 1: "2023Nen" contains uppercase, so it takes CaseBased
        // (branch 2) and never enters the number branches. It gets the usual composition of
        // dictionary lookup plus kana candidates plus alphabet candidates.
        assert_eq!(
            classify_input_shape("2023Nen"),
            InputShape::CaseBased,
            "digit-leading input containing uppercase should be CaseBased"
        );

        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "2023Nen");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert!(
            displays.contains(&"2023ねん"),
            "branch 2 should produce the hiragana candidate (the reading as it is) when the dictionary has no match: {:?}",
            displays
        );
        assert!(
            displays.contains(&"2023Nen"),
            "branch 2 should produce the half-width alphabet candidate (the case as typed): {:?}",
            displays
        );
    }

    // === The digit-leading branch (branch 4, D-21) ===

    #[test]
    fn replace_digit_runs_with_hash_extracts_digit_runs_in_order_and_substitutes_hashes() {
        assert_eq!(
            replace_digit_runs_with_hash("2023nen"),
            ("#nen".to_string(), vec!["2023".to_string()])
        );
        assert_eq!(
            replace_digit_runs_with_hash("12;34"),
            ("#;#".to_string(), vec!["12".to_string(), "34".to_string()])
        );
    }

    #[test]
    fn substitute_sharp_markers_returns_none_on_a_count_mismatch() {
        // "#3年" has one # and num_runs also has one, so they match.
        assert_eq!(
            substitute_sharp_markers("#3年", &["2023".to_string()]),
            Some("二千二十三年".to_string())
        );
        // "#3年#3月" has two # but num_runs has one, so the count mismatch yields None (dropping the candidate).
        assert_eq!(
            substitute_sharp_markers("#3年#3月", &["2023".to_string()]),
            None
        );
        // Pairing by order of appearance: #1 and #2 map to different digit runs (by position, not by type value).
        assert_eq!(
            substitute_sharp_markers("#1年#2月", &["12".to_string(), "34".to_string()]),
            Some("１２年三四月".to_string())
        );
    }

    #[test]
    fn digit_leading_words_convert_through_hash_substituted_lookup_including_fuzzy_search() {
        // D-23 [correction]: branch 4 also flows through fuzzy search in addition to exact
        // matching, because upstream sekka-henkan--okuri-nashi-and-number reuses
        // sekka-henkan--okuri-nashi. This pins it with a dictionary layout imitating one
        // where #0/#1/#2/#3 all really exist under a reading key containing #ねん.
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("#ねん", "#3年");
        dict.add_entry("#ねん", "#1年");
        // A word containing two #: it must not appear in the candidate list for input with one digit run (2023nen).
        dict.add_entry("#ねん", "#3年#3月");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "2023nen");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays,
            vec!["二千二十三年", "２０２３年", "２０２３ｎｅｎ", "2023nen"],
            "the first should be 二千二十三年, the second ２０２３年 and the last two the full-width/half-width alphabet: {:?}",
            displays
        );
    }

    #[test]
    fn with_two_digit_runs_a_two_marker_word_also_stays_a_candidate() {
        // Checking the positional pairing: "12;34" has two num_runs (["12","34"]), so a
        // dictionary word containing two # ("#1年#2月") matches on count and stays a candidate.
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("#;#", "#1年#2月");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "12;34");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert!(
            displays.contains(&"１２年三四月"),
            "#1年#2月 should map # 1 -> 12 and # 2 -> 34 positionally: {:?}",
            displays
        );
    }

    #[test]
    fn uppercase_input_with_no_dictionary_match_puts_hiragana_first() {
        let mut ctx = SekkaContext::new();
        let committed = commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(committed, "かんじ");

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(displays, vec!["かんじ", "カンジ", "Ｋａｎｊｉ", "Kanji"]);
    }

    #[test]
    fn a_dictionary_katakana_word_comes_before_the_generated_hiragana() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "カンジ");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Kanji");

        let candidates = ctx.get_candidates();
        assert_eq!(candidates[0].display, "カンジ");
        assert_eq!(candidates[0].kind, CandidateKind::Kanji);
        assert_eq!(candidates[1].display, "かんじ");
        assert_eq!(candidates[1].kind, CandidateKind::Hiragana);
    }

    // === Direct switching during reselection (D-09) ===

    #[test]
    fn a_direct_switch_moves_to_the_first_candidate_of_each_kind() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "感じ");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(ctx.get_candidate_index(), 0);

        // Candidate order: 漢字(0), 感じ(1), かんじ(2), カンジ(3), Ｋａｎｊｉ(4), Kanji(5)
        assert!(ctx.select_hankaku());
        assert_eq!(ctx.get_candidate_index(), 5);
        assert_eq!(ctx.get_preedit(), "Kanji");

        assert!(ctx.select_zenkaku());
        assert_eq!(ctx.get_candidate_index(), 4);
        assert_eq!(ctx.get_preedit(), "Ｋａｎｊｉ");

        assert!(ctx.select_katakana());
        assert_eq!(ctx.get_candidate_index(), 3);
        assert_eq!(ctx.get_preedit(), "カンジ");

        assert!(ctx.select_hiragana());
        assert_eq!(ctx.get_candidate_index(), 2);
        assert_eq!(ctx.get_preedit(), "かんじ");

        assert!(ctx.select_kanji());
        assert_eq!(ctx.get_candidate_index(), 0);
        assert_eq!(ctx.get_preedit(), "漢字");
    }

    #[test]
    fn select_kanji_moves_to_the_trailing_kanji_candidate_in_all_lowercase() {
        let mut ctx = SekkaContext::new();
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "kanji");

        // Candidate order: かんじ(0), カンジ(1), ｋａｎｊｉ(2), kanji(3), 漢字(4)
        assert!(ctx.select_kanji());
        assert_eq!(ctx.get_candidate_index(), 4);
        assert_eq!(ctx.get_preedit(), "漢字");
    }

    #[test]
    fn direct_switch_changes_nothing_when_the_kind_is_absent() {
        let mut ctx = SekkaContext::new();
        commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(ctx.get_candidate_index(), 0);

        // With no dictionary there is no Kanji/KanjiWithOkuri candidate.
        let selected = ctx.select_kanji();
        assert!(!selected);
        assert_eq!(ctx.get_candidate_index(), 0);
    }

    #[test]
    fn direct_switch_does_nothing_in_the_input_state() {
        let mut ctx = SekkaContext::new();
        assert_eq!(ctx.state(), ConversionState::Input);

        assert!(!ctx.select_kanji());
        assert!(!ctx.select_hiragana());
        assert!(!ctx.select_katakana());
        assert!(!ctx.select_hankaku());
        assert!(!ctx.select_zenkaku());
    }

    // === Committing romaji as it is, and BackSpace (D-03) ===

    #[test]
    fn committing_romaji_as_is_flushes_the_unsent_staged_candidate_first() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());
        assert!(ctx.last_commit.is_some());

        // Character input flushes the unsent staged candidate (D-12).
        ctx.process_key('k', false);
        assert_eq!(ctx.poll_output(), Some("あ".to_string()));
        assert!(ctx.last_commit.is_none());

        let committed = ctx.commit_raw_romaji();
        assert!(committed);
        assert_eq!(ctx.poll_output(), Some("k".to_string()));
        assert_eq!(ctx.get_preedit(), "");
    }

    #[test]
    fn commit_with_trailing_char_appends_to_whatever_this_key_event_committed() {
        // D-158. Case (b) fixes the double-flush trap (Pitfall 2): the caller
        // (dispatch_input in capi.rs) already ran the single D-12 flush point for
        // this key event before calling here, so by the time
        // commit_with_trailing_char runs, `committed_output` may already hold the
        // commit-display word and `commit_raw_romaji`'s own internal flush finds
        // nothing left. Deciding whether to append by `committed_output` (not by
        // `commit_raw_romaji`'s return value) is what makes this case correct.

        // (a) A plain romaji buffer ("Ctrl") gets the character appended.
        let mut ctx = SekkaContext::new();
        for ch in "Ctrl".chars() {
            ctx.process_key(ch, false);
        }
        assert!(ctx.commit_with_trailing_char('+'));
        assert_eq!(ctx.poll_output(), Some("Ctrl+".to_string()));
        assert_eq!(ctx.get_preedit(), "");

        // (b) The commit-display word ("あ"), already flushed by the caller before
        // commit_with_trailing_char runs, gets the character appended too.
        let mut ctx = SekkaContext::new();
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.flush_last_commit());
        assert!(ctx.commit_with_trailing_char('('));
        assert_eq!(ctx.poll_output(), Some("あ(".to_string()));
        assert!(ctx.poll_output().is_none());

        // (c) Nothing to commit: returns false and produces no output.
        let mut ctx = SekkaContext::new();
        assert!(!ctx.commit_with_trailing_char('+'));
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn backspace_returns_false_with_an_empty_buffer_and_no_staged_candidate() {
        let mut ctx = SekkaContext::new();
        assert!(!ctx.backspace());
    }

    #[test]
    fn backspace_in_the_commit_display_state_reverts_to_the_raw_romaji_without_committing() {
        // D-13 used to say BackSpace in the commit display state commits first and
        // is then forwarded. D-160 (Phase 9) replaces that: it reverts to the
        // original romaji ("a" minus its last character) instead, committing
        // nothing.
        let mut ctx = SekkaContext::new();
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());
        assert!(ctx.last_commit.is_some());

        let reverted = ctx.backspace();
        assert!(reverted);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "");
        assert!(ctx.last_commit.is_none());

        // The romaji is now exhausted, so a further BackSpace is unconsumed.
        assert!(!ctx.backspace());
        assert!(ctx.poll_output().is_none());
    }

    // === Invariants and transitions of the commit display state (VALIDATION.md Wave 0 Gaps 1 and 2) ===

    /// Shared helper dictionary for producing input with two or more candidates
    fn two_candidate_dictionary() -> MockDictionary {
        let mut dict = MockDictionary::new();
        dict.add_entry("か", "蚊");
        dict.add_entry("か", "火");
        dict
    }

    #[test]
    fn the_romaji_buffer_is_empty_in_the_commit_display_state() {
        // Through convert_and_stage (Ctrl-J).
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.last_commit.is_some());
        assert!(ctx.romaji_buffer.is_empty());

        // Through confirm (Selecting -> Enter).
        let mut ctx2 = SekkaContext::new();
        ctx2.add_dictionary(Arc::new(two_candidate_dictionary()));
        commit_then_reselect(&mut ctx2, "ka");
        assert_eq!(ctx2.state(), ConversionState::Selecting);
        ctx2.confirm();
        assert!(ctx2.last_commit.is_some());
        assert!(ctx2.romaji_buffer.is_empty());

        // Through cancel (Selecting -> Esc).
        let mut ctx3 = SekkaContext::new();
        ctx3.add_dictionary(Arc::new(two_candidate_dictionary()));
        commit_then_reselect(&mut ctx3, "ka");
        assert_eq!(ctx3.state(), ConversionState::Selecting);
        ctx3.cancel();
        assert!(ctx3.last_commit.is_some());
        assert!(ctx3.romaji_buffer.is_empty());
    }

    #[test]
    fn the_key_after_ctrl_j_commits_the_word() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "か");

        let consumed = ctx.process_key('d', false);
        assert!(consumed);
        assert_eq!(ctx.poll_output(), Some("か".to_string()));
        // The second poll_output is None (no double commit).
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "d");
        assert!(ctx.last_commit.is_none());
    }

    #[test]
    fn committing_romaji_in_the_commit_display_state_returns_true_even_with_an_empty_buffer() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());

        // Directly preventing a recurrence of Pitfall 1: it returns true even with an empty buffer.
        let committed = ctx.commit_raw_romaji();
        assert!(committed);
        assert_eq!(ctx.poll_output(), Some("か".to_string()));
    }

    #[test]
    fn backspace_returns_true_in_the_commit_display_state_even_with_an_empty_buffer() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());

        // Pitfall 1 and D-160 (replaces D-13): it returns true even with an empty
        // romaji buffer, reverting to the original romaji ("ka" minus its last
        // character) instead of committing.
        let reverted = ctx.backspace();
        assert!(reverted);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "k");
    }

    #[test]
    fn backspace_does_not_commit_while_there_is_romaji() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);

        let flushed = ctx.backspace();
        assert!(flushed);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), "k");
    }

    #[test]
    fn repeated_backspace_after_conversion_empties_the_buffer_and_then_returns_false() {
        // D-160: once the reverted romaji itself has been fully deleted by
        // repeated BackSpace, the next BackSpace is unconsumed (an ordinary
        // BackSpace reaches the application from there).
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());

        assert!(ctx.backspace());
        assert_eq!(ctx.get_preedit(), "k");
        assert!(ctx.poll_output().is_none());

        assert!(ctx.backspace());
        assert_eq!(ctx.get_preedit(), "");
        assert!(ctx.poll_output().is_none());

        assert!(!ctx.backspace());
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn backspace_after_confirming_a_reselected_word_reverts_to_the_same_raw_romaji() {
        // D-160/D-161: confirming a different candidate during reselection
        // (`confirm`) carries over the original romaji from before reselection
        // began, so BackSpace afterward reverts to that same romaji, not to the
        // newly confirmed word's own (nonexistent) romaji.
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "幹事");
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        // "Kanji" (capitalized) requests kanji-first ordering (D-07), so the Kanji
        // group leads and the first candidate is 漢字.
        let first = commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(first, "漢字");
        ctx.next_candidate();
        assert_eq!(
            ctx.get_candidates()[ctx.get_candidate_index() as usize].display,
            "幹事"
        );
        ctx.confirm();

        assert!(ctx.backspace());
        assert_eq!(ctx.get_preedit(), "Kanj");
        assert!(ctx.poll_output().is_none());

        ctx.process_key('i', false);
        let staged = ctx.process_key('\0', true);
        assert!(staged);
        assert_eq!(ctx.get_preedit(), "漢字");
    }

    #[test]
    fn backspace_after_cancelling_reselection_reverts_to_the_raw_romaji() {
        // D-160/D-161: cancelling reselection (Esc/q/Ctrl-G) puts the originally
        // committed word back into the commit display state without touching
        // last_commit, so BackSpace afterward still reverts to the same original
        // romaji.
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "幹事");
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Kanji");
        ctx.next_candidate();
        ctx.cancel();
        assert_eq!(ctx.get_preedit(), "漢字");

        assert!(ctx.backspace());
        assert_eq!(ctx.get_preedit(), "Kanj");
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn backspace_during_reselection_closes_the_window_and_reverts_to_the_raw_romaji() {
        // D-161: BackSpace during reselection closes the candidate window (the
        // same three resets as `cancel`) and reverts to the original romaji, like
        // D-160 does in the commit display state.
        let mut dict = MockDictionary::new();
        dict.add_entry("かんじ", "漢字");
        dict.add_entry("かんじ", "幹事");
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(dict));

        commit_then_reselect(&mut ctx, "Kanji");
        ctx.next_candidate();
        assert_eq!(ctx.state(), ConversionState::Selecting);

        assert!(ctx.backspace());
        assert_eq!(ctx.state(), ConversionState::Input);
        assert!(ctx.get_candidates().is_empty());
        assert_eq!(ctx.get_candidate_index(), -1);
        assert_eq!(ctx.get_preedit(), "Kanj");
        assert!(ctx.poll_output().is_none());
    }

    #[test]
    fn backspace_to_the_raw_romaji_records_no_learning() {
        // D-160: reverting to the romaji via BackSpace never calls
        // flush_last_commit, so the candidate that was on screen is never
        // recorded as learned. Part (2) below is the contrast: the same sequence
        // but flushing instead of backspacing does learn, confirming the harness
        // would actually catch it if BackSpace had learned too.
        let mut master = MockDictionary::new();
        master.add_entry("かんじ", "漢字");
        master.add_entry("かんじ", "幹事");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        // (1) Confirm 幹事 during reselection, then BackSpace to the romaji
        // instead of flushing: nothing is learned.
        commit_then_reselect(&mut ctx, "Kanji");
        ctx.next_candidate();
        ctx.confirm();
        assert!(ctx.backspace());
        ctx.process_key('i', false);
        ctx.process_key('\0', true);
        assert_eq!(
            ctx.get_preedit(),
            "漢字",
            "backspace-to-romaji must not record learning for 幹事"
        );
        ctx.reset();

        // (2) Contrast: the same sequence but flushing (instead of BackSpace-ing)
        // does learn, promoting 幹事 to the front of the Kanji-leading group (and
        // therefore to the overall first candidate, since "Kanji" is capitalized).
        commit_then_reselect(&mut ctx, "Kanji");
        ctx.next_candidate();
        ctx.confirm();
        assert!(ctx.flush_last_commit());
        ctx.poll_output();
        let recommitted = commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(
            recommitted, "幹事",
            "learning should have promoted 幹事 to first, unlike part (1)"
        );
    }

    #[test]
    fn entering_reselection_causes_no_commit() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());

        let entered = ctx.process_key('\0', true);
        assert!(entered);
        assert_eq!(ctx.state(), ConversionState::Selecting);
        assert!(ctx.poll_output().is_none());
        assert!(!ctx.get_candidates().is_empty());
    }

    #[test]
    fn confirming_a_reselection_only_returns_to_the_commit_display_state_without_committing() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(two_candidate_dictionary()));
        commit_then_reselect(&mut ctx, "ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.next_candidate();
        let second = ctx.get_preedit().to_string();
        ctx.confirm();
        assert_eq!(ctx.state(), ConversionState::Input);
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), second);
    }

    #[test]
    fn cancelling_returns_to_the_preedit_of_the_commit_display_state() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(two_candidate_dictionary()));
        let committed = commit_then_reselect(&mut ctx, "ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.next_candidate();
        ctx.cancel();
        assert!(ctx.poll_output().is_none());
        assert_eq!(ctx.get_preedit(), committed);
    }

    #[test]
    fn reselection_works_again_right_after_cancelling() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(two_candidate_dictionary()));
        commit_then_reselect(&mut ctx, "ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.cancel();
        assert_eq!(ctx.state(), ConversionState::Input);

        let entered = ctx.process_key('\0', true);
        assert!(entered);
        assert_eq!(ctx.state(), ConversionState::Selecting);
    }

    #[test]
    fn finalize_staged_commits_the_word_in_the_commit_display_state() {
        let mut ctx = SekkaContext::new();
        ctx.process_key('k', false);
        ctx.process_key('a', false);
        ctx.process_key('\0', true);
        assert!(ctx.poll_output().is_none());

        // We are not in the Selecting state, so confirm() does nothing and the staged word
        // is flushed as it is (D-15).
        ctx.finalize_staged();
        assert_eq!(ctx.poll_output(), Some("か".to_string()));
    }

    #[test]
    fn finalize_staged_during_reselection_commits_the_selected_candidate() {
        let mut ctx = SekkaContext::new();
        ctx.add_dictionary(Arc::new(two_candidate_dictionary()));
        commit_then_reselect(&mut ctx, "ka");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.next_candidate();
        let second = ctx.get_preedit().to_string();
        ctx.finalize_staged();
        assert_eq!(ctx.poll_output(), Some(second));
    }

    // === Learning wiring (D-32 to D-39; the tracer of Task 1) ===

    #[test]
    fn committing_the_dot_symbol_puts_it_first_next_time() {
        // Tracer: typing 「.」 then Ctrl-J -> reselecting and confirming 「。」 -> committing on
        // the next key -> that word being recorded into the user dictionary -> typing 「.」 +
        // Ctrl-J again in the same session putting 「。」 first. It walks this single path
        // through all four layers: Candidate -> the Dictionary trait -> UserDict ->
        // SekkaContext (the recording point of D-34, the stable reordering of D-39, and the
        // provenance decision of D-32/D-33).
        let mut master = MockDictionary::new();
        master.add_entry(".", "．");
        master.add_entry(".", "・");
        master.add_entry(".", "。");
        master.add_entry(".", "…");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = UserDict::open(tmp.path().join("user_dict"))
            .expect("failed to open the user dictionary");
        // The same handle is used for the post-recording verification (avoiding a sled reopen).
        let user_dict = Arc::new(user_dict);

        let mut ctx = SekkaContext::new();
        // An order that fails unless the reordering of D-39 works: master first, user dictionary second.
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        // First round: commit ".", enter reselection and call next_candidate twice to select
        // 「。」 (the third entry of the candidate list).
        let committed = commit_then_reselect(&mut ctx, ".");
        assert_eq!(committed, "．");
        assert_eq!(ctx.state(), ConversionState::Selecting);

        ctx.next_candidate();
        ctx.next_candidate();
        assert_eq!(
            ctx.get_candidates()[ctx.get_candidate_index() as usize].display,
            "。"
        );

        ctx.confirm();
        assert!(ctx.poll_output().is_none());

        // D-34: a single call to flush_last_commit runs the recording.
        assert!(ctx.flush_last_commit());
        assert_eq!(ctx.poll_output(), Some("。".to_string()));

        // Second round: "." + Ctrl-J again on the same context -> enter reselection and
        // confirm that the head of the display list from get_candidates() is 「。」
        // (convert_and_stage empties self.candidates right after staging, so seeing the
        // candidate list requires going as far as reselection, i.e. the second Ctrl-J).
        let recommitted = commit_then_reselect(&mut ctx, ".");
        assert_eq!(
            recommitted, "。",
            "the learned word should be staged in the preedit as the first candidate"
        );

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays.first(),
            Some(&"。"),
            "the learned word should come first: {:?}",
            displays
        );

        // UserDict::lookup(".") returns 「。」 with a frequency of at least 1.
        let entries = user_dict.lookup(".").expect("lookup failed");
        assert!(
            entries.iter().any(|e| e.word == "。" && e.frequency >= 1),
            "UserDict::lookup(\".\") should contain 「。」 with a frequency of at least 1: {:?}",
            entries
        );
    }

    // === Pinning the two mechanisms of D-38 and the boundaries of D-32/D-33/D-35/D-37/D-39 (Task 2) ===

    // This section changes no production code and touches only `#[cfg(test)]`. The tracer of
    // Task 1 exercised just one mechanism, branch 6 (dictionary list order), so the other
    // mechanism D-38 decided to keep alongside it (branches 2 and 5, through
    // sort_candidates) and the boundaries of D-32/D-33/D-35/D-37 are each pinned by their
    // own independent test.

    /// Test dictionary that reports `mode() -> ReadWrite` but whose `record_selection`
    /// always returns `Err` (used to pin D-35; a minimal implementation that exists only to
    /// confirm a recording failure does not block the commit)
    struct AlwaysFailingDictionary {
        path: PathBuf,
    }

    impl AlwaysFailingDictionary {
        fn new() -> Self {
            AlwaysFailingDictionary {
                path: PathBuf::from("/tmp/always_failing_dict"),
            }
        }
    }

    impl Dictionary for AlwaysFailingDictionary {
        fn lookup(&self, _reading: &str) -> Result<Vec<DictEntry>, DictError> {
            Ok(Vec::new())
        }

        fn prefix_search(&self, _prefix: &str) -> Result<Vec<(String, Vec<DictEntry>)>, DictError> {
            Ok(Vec::new())
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn mode(&self) -> DictionaryMode {
            DictionaryMode::ReadWrite
        }

        fn record_selection(&self, _reading: &str, _word: &str) -> Result<(), DictError> {
            Err(DictError::ReadOnlyViolation)
        }
    }

    #[test]
    fn branches_2_and_5_let_learning_lead_within_a_group_through_sort_candidates_without_crossing_groups(
    ) {
        // The upper mechanism of D-38: through sort_candidates, which orders by group_rank
        // and then by descending frequency. With all-lowercase input (branch 5) the Kanji
        // group comes last, so this simultaneously shows that learning changes only the
        // order inside the Kanji group and leaves the leading group (Hiragana) untouched
        // (preserving D-07).
        let mut master = MockDictionary::new();
        master.add_entry("かんじ", "漢字");
        master.add_entry("かんじ", "幹事");
        master.add_entry("かんじ", "監事");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        // First round: select and confirm 「幹事」, the second entry of the Kanji group.
        commit_then_reselect(&mut ctx, "kanji");
        assert!(
            ctx.select_kanji(),
            "it should be possible to switch directly to the first candidate of the Kanji kind"
        );
        assert_eq!(
            ctx.get_candidates()[ctx.get_candidate_index() as usize].display,
            "漢字"
        );
        ctx.next_candidate();
        assert_eq!(
            ctx.get_candidates()[ctx.get_candidate_index() as usize].display,
            "幹事"
        );
        ctx.confirm();
        assert!(ctx.poll_output().is_none());
        assert!(ctx.flush_last_commit());
        ctx.poll_output();

        // Second round: reconverting the same input puts 「幹事」 first inside the Kanji group.
        commit_then_reselect(&mut ctx, "kanji");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(
            displays[0], "かんじ",
            "the leading group stays Hiragana (preserving D-07): {:?}",
            displays
        );
        assert_eq!(
            displays[4], "幹事",
            "the learned word should come first inside the Kanji group: {:?}",
            displays
        );
        assert_eq!(displays.len(), 7);
    }

    #[test]
    fn branch_6_lets_learning_lead_through_the_dictionary_list_order_without_changing_the_candidate_set(
    ) {
        // The middle mechanism of D-38: the branch whose order is decided by the dictionary
        // list order (lookup_dictionary_exact plus the first-appearance order preserved by
        // merge_candidate) rather than by sort_candidates. It also shows that learning does
        // not change the number or the contents (the set) of the candidates.
        let mut master = MockDictionary::new();
        master.add_entry(",", "，");
        master.add_entry(",", "、");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        let committed = commit_then_reselect(&mut ctx, ",");
        assert_eq!(committed, "，");
        let before: Vec<String> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.clone())
            .collect();

        ctx.next_candidate();
        assert_eq!(
            ctx.get_candidates()[ctx.get_candidate_index() as usize].display,
            "、"
        );
        ctx.confirm();
        assert!(ctx.flush_last_commit());
        ctx.poll_output();

        let recommitted = commit_then_reselect(&mut ctx, ",");
        assert_eq!(recommitted, "、", "the learned word should come first");
        let after: Vec<String> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.clone())
            .collect();

        let mut before_sorted = before.clone();
        before_sorted.sort();
        let mut after_sorted = after.clone();
        after_sorted.sort();
        assert_eq!(
            before_sorted, after_sorted,
            "learning must not change the candidate set (count or contents): before={:?} after={:?}",
            before, after
        );
    }

    #[test]
    fn a_fuzzy_candidate_learned_under_another_reading_never_outranks_exact_matches() {
        // RANK-01 / D-144 / D-148: a fuzzy candidate learned under one reading
        // must never let a different reading's exact match fall behind it.
        // `MockDictionary` does not implement `symspell_bucket` (it keeps the
        // default, empty implementation), so before any learning happens the
        // only source of a SymSpell hit between あと and もと is the UserDict's
        // index built by `record_selection` (`もと` gets indexed only after it
        // is confirmed once). Before that, Ato never sees 基/元 as candidates.
        let mut master = MockDictionary::new();
        master.add_entry("あと", "後");
        master.add_entry("あと", "跡");
        master.add_entry("もと", "基");
        master.add_entry("もと", "元");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        // First round: learn 元 under the もと reading (RANK-02: exact match
        // learning is unaffected by D-144).
        let committed = commit_then_reselect(&mut ctx, "Moto");
        assert_eq!(committed, "基");
        ctx.next_candidate();
        assert_eq!(
            ctx.get_candidates()[ctx.get_candidate_index() as usize].display,
            "元"
        );
        ctx.confirm();
        assert!(ctx.poll_output().is_none());
        assert!(ctx.flush_last_commit());
        ctx.poll_output();

        // Second round: read Ato. The learned 元 must show up (not a fluke of
        // an empty candidate list) with tier 1 and frequency >= 1, coming from
        // the UserDict's SymSpell index that record_selection just populated.
        let first = commit_then_reselect(&mut ctx, "Ato");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        let moto_candidate = ctx
            .get_candidates()
            .iter()
            .find(|c| c.display == "元")
            .unwrap_or_else(|| panic!("元 is missing from the Ato candidate list: {:?}", displays));
        assert_eq!(
            moto_candidate.tier, 1,
            "元 should be a SymSpell (tier 1) candidate for Ato: {:?}",
            displays
        );
        assert!(
            moto_candidate.frequency >= 1,
            "元 should carry the learned frequency: {:?}",
            displays
        );
        assert_eq!(
            first, "後",
            "an unlearned exact match (後) must not be outranked by a fuzzy candidate learned under another reading (RANK-01): {:?}",
            displays
        );
        assert_eq!(
            &displays[..4],
            &["後", "跡", "元", "基"],
            "the learned fuzzy candidate (元) should lead the fuzzy stage, behind every exact match (D-145): {:?}",
            displays
        );

        // reset() discards the read-only Ato conversion without learning it (D-06).
        ctx.reset();

        // Third round: Moto should still commit 元 first (RANK-02 unaffected).
        let recommitted = commit_then_reselect(&mut ctx, "Moto");
        assert_eq!(recommitted, "元");
    }

    #[test]
    fn learning_a_word_under_a_neighbouring_reading_does_not_lift_it_among_exact_matches() {
        // D-147 / RANK-02: recording a selection under one reading (かど) must not
        // let the fuzzy-stage frequency it creates carry into a different reading's
        // (かく) exact match of the same word (角). `MockDictionary` has no
        // `symspell_bucket` of its own (same setup as
        // `a_fuzzy_candidate_learned_under_another_reading_never_outranks_exact_matches`
        // above), so before any learning happens the only source of a SymSpell hit
        // between かく and かど is the UserDict's index, which `record_selection`
        // builds only once かど has actually been confirmed. 門 (also under かど,
        // never selected) is the witness that the かど SymSpell bucket really is
        // feeding かく's fuzzy stage: if it is missing, the merge never crossed the
        // stage boundary D-147 guards, and the test would pass for the wrong reason.
        let mut master = MockDictionary::new();
        master.add_entry("かく", "核");
        master.add_entry("かく", "格");
        master.add_entry("かく", "角");
        master.add_entry("かど", "角");
        master.add_entry("かど", "門");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        // First round: learn 角 under the かど reading.
        let committed = commit_then_reselect(&mut ctx, "Kado");
        assert_eq!(committed, "角");
        ctx.confirm();
        assert!(ctx.poll_output().is_none());
        assert!(ctx.flush_last_commit());
        ctx.poll_output();

        // Second round: read かく. 角 there is an exact match, unlearned under かく
        // itself; its frequency must stay 0 even though かど's SymSpell bucket now
        // really does feed かく's fuzzy stage (witnessed by 門, tier 1).
        let first = commit_then_reselect(&mut ctx, "Kaku");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        let mon_candidate = ctx.get_candidates().iter().find(|c| c.display == "門");
        assert_eq!(
            mon_candidate.map(|c| c.tier),
            Some(1),
            "門 (only reachable from かど) must appear as a tier-1 candidate for Kaku, proving the かど bucket fed かく: {:?}",
            displays
        );
        let kaku_matches: Vec<&Candidate> = ctx
            .get_candidates()
            .iter()
            .filter(|c| c.display == "角")
            .collect();
        assert_eq!(
            kaku_matches.len(),
            1,
            "角 must be deduplicated to a single candidate: {:?}",
            displays
        );
        assert_eq!(
            kaku_matches[0].tier, 0,
            "角 must stay an exact match (tier 0) for Kaku: {:?}",
            displays
        );
        assert_eq!(
            kaku_matches[0].frequency, 0,
            "角 must not carry over the frequency learned under かど (D-147, RANK-01): {:?}",
            displays
        );
        assert_eq!(
            first, "核",
            "an unlearned exact match (核) must not be outranked by 角's borrowed frequency: {:?}",
            displays
        );
        assert_eq!(
            &displays[..4],
            &["核", "格", "角", "門"],
            "the exact matches must stay ahead of the fuzzy stage: {:?}",
            displays
        );

        // Learn 角 under its own reading (かく) too - same-stage learning (RANK-02)
        // must keep working after D-147.
        ctx.next_candidate();
        ctx.next_candidate();
        assert_eq!(
            ctx.get_candidates()[ctx.get_candidate_index() as usize].display,
            "角"
        );
        ctx.confirm();
        assert!(ctx.poll_output().is_none());
        assert!(ctx.flush_last_commit());
        ctx.poll_output();

        // Third round: かく's own learning must now take effect.
        let recommitted = commit_then_reselect(&mut ctx, "Kaku");
        assert_eq!(
            recommitted, "角",
            "learning under the matching reading (かく) must promote 角 (RANK-02)"
        );
    }

    #[test]
    fn okuri_ari_learning_records_the_raw_word_before_okurigana_is_appended_and_never_doubles_it() {
        // Row 1 of the D-33 table: pins that the raw word 「感」 held by learn_pair is recorded
        // rather than the display (「感じ」, after the okurigana was appended), and that
        // reconverting does not produce a doubled okurigana such as 「感じじ」.
        let mut master = MockDictionary::new();
        master.add_entry("かんj", "感");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        let committed = commit_then_reselect(&mut ctx, "kanJi");
        assert_eq!(committed, "感じ");
        ctx.confirm();
        assert!(ctx.flush_last_commit());
        ctx.poll_output();

        let entries = user_dict.lookup("かんj").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].word, "感",
            "the recorded word should be the raw word before the okurigana was appended (D-33)"
        );
        assert!(entries[0].frequency >= 1);

        // Reconverting the same input does not produce a word with doubled okurigana.
        let recommitted = commit_then_reselect(&mut ctx, "kanJi");
        assert_eq!(recommitted, "感じ");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert!(
            !displays.iter().any(|d| d.contains("感じじ")),
            "a word with doubled okurigana crept in: {:?}",
            displays
        );
    }

    #[test]
    fn learning_in_branch_4_records_the_raw_word_before_substitution_and_leaves_no_garbage() {
        // Row 2 of the D-33 table: pins that the raw word 「#3年」 held by learn_pair is
        // recorded rather than the display (「二千二十三年」, after the # substitution), and that
        // converting the next digit run (2024nen) with the recorded word still yields a
        // candidate whose # substitution succeeded (leftover garbage would make the
        // substitution fail and drop the candidate).
        let mut master = MockDictionary::new();
        master.add_entry("#ねん", "#3年");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master), user_dict.clone()]);

        let committed = commit_then_reselect(&mut ctx, "2023nen");
        assert_eq!(committed, "二千二十三年");
        ctx.confirm();
        assert!(ctx.flush_last_commit());
        ctx.poll_output();

        let entries = user_dict.lookup("#ねん").expect("lookup failed");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].word, "#3年",
            "the recorded word should be the raw word before the # substitution (D-33)"
        );
        assert!(
            !entries.iter().any(|e| e.word == "二千二十三年"),
            "the display string with the # already substituted must not be recorded: {:?}",
            entries
        );

        // Converting 2024nen next yields a candidate whose # substitution succeeded.
        commit_then_reselect(&mut ctx, "2024nen");
        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert!(
            displays.contains(&"二千二十四年"),
            "a candidate with a successful # substitution should appear (leftover garbage would make the substitution fail and drop it): {:?}",
            displays
        );
    }

    #[test]
    fn generated_candidates_are_not_recorded() {
        // D-32: committing and flushing a generated candidate that consulted no dictionary
        // at all (here the hiragana fallback) leaves the user dictionary empty.
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![user_dict.clone()]);

        let committed = commit_then_reselect(&mut ctx, "Kanji");
        assert_eq!(
            committed, "かんじ",
            "with no dictionary the generated hiragana candidate should be first"
        );
        ctx.confirm();
        assert!(ctx.flush_last_commit());

        let results = user_dict.prefix_search("").expect("prefix_search failed");
        assert!(
            results.is_empty(),
            "generated candidates must not be recorded into the user dictionary (D-32): {:?}",
            results
        );
    }

    #[test]
    fn candidates_from_the_digits_only_branch_are_not_recorded() {
        // D-32/Pitfall 1: the 4 candidates of branch 3 (digits only) are classified as
        // CandidateKind::Kanji, but no dictionary was consulted at all, so learn_pair stays
        // None. The single case that fails only "an implementation that decides provenance
        // from the candidate kind".
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = Arc::new(
            UserDict::open(tmp.path().join("user_dict"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![user_dict.clone()]);

        let committed = commit_then_reselect(&mut ctx, "2023");
        assert_eq!(committed, "２０２３");
        ctx.confirm();
        assert!(ctx.flush_last_commit());

        let results = user_dict.prefix_search("").expect("prefix_search failed");
        assert!(
            results.is_empty(),
            "candidates of branch 3 (digits only) consulted no dictionary and must not be recorded (D-32): {:?}",
            results
        );
    }

    #[test]
    fn committing_succeeds_with_zero_writable_dictionaries() {
        // D-37: even committing with only the ReadOnly master dictionary passed to
        // set_dictionaries (zero writable dictionaries), flush_last_commit returns true and
        // the commit always succeeds.
        let mut master = MockDictionary::new();
        master.add_entry("かんじ", "漢字");

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::new(master)]);

        commit_then_reselect(&mut ctx, "kanji");
        ctx.confirm();
        assert!(ctx.poll_output().is_none());

        assert!(
            ctx.flush_last_commit(),
            "the flush should succeed even with zero writable dictionaries (D-37)"
        );
        assert!(ctx.poll_output().is_some());
    }

    #[test]
    fn only_the_first_writable_dictionary_records_and_a_recording_failure_still_commits() {
        // (a) D-37: with several writable dictionaries, only the first one is recorded into.
        let mut master = MockDictionary::new();
        master.add_entry("かんじ", "漢字");
        let tmp1 = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict1 = Arc::new(
            UserDict::open(tmp1.path().join("user_dict1"))
                .expect("failed to open the user dictionary"),
        );
        let tmp2 = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict2 = Arc::new(
            UserDict::open(tmp2.path().join("user_dict2"))
                .expect("failed to open the user dictionary"),
        );

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![
            Arc::new(master),
            user_dict1.clone(),
            user_dict2.clone(),
        ]);

        commit_then_reselect(&mut ctx, "kanji");
        assert!(ctx.select_kanji());
        ctx.confirm();
        assert!(ctx.flush_last_commit());

        let entries1 = user_dict1.lookup("かんじ").expect("lookup failed");
        assert_eq!(
            entries1.len(),
            1,
            "only the first ReadWrite dictionary should be recorded into (D-37)"
        );
        assert_eq!(entries1[0].word, "漢字");

        let entries2 = user_dict2.lookup("かんじ").expect("lookup failed");
        assert!(
            entries2.is_empty(),
            "the second ReadWrite dictionary should not be recorded into (D-37): {:?}",
            entries2
        );

        // (b) D-35: even when record_selection of the first ReadWrite dictionary returns Err,
        // the commit always succeeds.
        let mut ctx2 = SekkaContext::new();
        let mut master2 = MockDictionary::new();
        master2.add_entry("かんじ", "漢字");
        ctx2.set_dictionaries(vec![
            Arc::new(AlwaysFailingDictionary::new()),
            Arc::new(master2),
        ]);

        commit_then_reselect(&mut ctx2, "kanji");
        assert!(ctx2.select_kanji());
        ctx2.confirm();
        assert!(
            ctx2.flush_last_commit(),
            "the flush should succeed even when record_selection returns Err (D-35)"
        );
        assert!(ctx2.poll_output().is_some());
    }

    #[test]
    fn set_dictionaries_stably_moves_readwrite_dictionaries_first_keeping_relative_order() {
        // D-39: even passing three dictionaries as ReadOnly -> ReadWrite -> ReadOnly, the
        // ReadWrite one moves to the front while the relative order of the two ReadOnly ones
        // stays as passed (a stable sort). Both are observed at once in the candidate display
        // list of branch 6.
        let mut master1 = MockDictionary::new();
        master1.add_entry("!", "one");
        let mut master2 = MockDictionary::new();
        master2.add_entry("!", "two");

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let user_dict = UserDict::open(tmp.path().join("user_dict"))
            .expect("failed to open the user dictionary");
        user_dict
            .record_selection("!", "user_word")
            .expect("failed to record the selection");

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![
            Arc::new(master1),
            Arc::new(user_dict),
            Arc::new(master2),
        ]);

        let committed = commit_then_reselect(&mut ctx, "!");
        assert_eq!(
            committed, "user_word",
            "the ReadWrite dictionary should have been reordered to the front (D-39)"
        );

        let displays: Vec<&str> = ctx
            .get_candidates()
            .iter()
            .map(|c| c.display.as_str())
            .collect();
        assert_eq!(displays[0], "user_word");
        let pos_one = displays
            .iter()
            .position(|d| *d == "one")
            .expect("one not found");
        let pos_two = displays
            .iter()
            .position(|d| *d == "two")
            .expect("two not found");
        assert!(
            pos_one < pos_two,
            "the relative order of the two ReadOnly dictionaries stays as passed (D-39): {:?}",
            displays
        );
    }

    // === D-102: save_dictionaries ===

    #[test]
    fn save_dictionaries_returns_ok_without_dictionaries() {
        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![]);
        assert!(ctx.save_dictionaries().is_ok());
    }

    #[test]
    fn save_dictionaries_calls_save_on_every_dictionary() {
        let dict1 = Arc::new(MockDictionary::new());
        let dict2 = Arc::new(MockDictionary::new());

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![dict1.clone(), dict2.clone()]);

        assert!(ctx.save_dictionaries().is_ok());
        assert_eq!(dict1.save_call_count(), 1);
        assert_eq!(dict2.save_call_count(), 1);
    }

    #[test]
    fn save_dictionaries_returns_the_first_error_while_still_trying_the_rest() {
        let dict1 = Arc::new(MockDictionary::new().with_save_failure());
        let dict2 = Arc::new(MockDictionary::new());

        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![dict1.clone(), dict2.clone()]);

        assert!(ctx.save_dictionaries().is_err());
        assert_eq!(dict1.save_call_count(), 1);
        assert_eq!(
            dict2.save_call_count(),
            1,
            "the save of the second should still be attempted even when the first fails"
        );
    }
}
