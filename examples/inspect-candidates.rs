// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Re-runnable measurement tool that dumps a whole candidate list with tier,
//! score and frequency
//!
//! (a) This tool exists for the practice of D-89 (2) (when the candidate list
//!     changes, record each changed candidate with the reason) and D-89 (3) (stop
//!     and consult a human as soon as any first candidate changes). The E2E
//!     candidate window only reaches one page, so the diff of a candidate list
//!     grown to several hundred entries by the SymSpell layer cannot be checked
//!     that way; we needed a way to see every candidate in order.
//!
//! (b) The measurement that produced the baseline table in
//!     `.planning/phases/03-dictionary-fuzzy-search/03-CONTEXT.md` section I
//!     during Phase 3 (`cargo run --example inspect-dict`) was throwaway and was
//!     never committed, leaving the comment in `libsekka/src/capi.rs` referring
//!     to a name that no longer existed. This tool has a different job (the
//!     candidate list itself, not the raw dictionary keys), so it was placed
//!     under the separate name `inspect-candidates` in a form that reproduces the
//!     same numbers. The comment in `capi.rs` was left untouched.
//!
//! (c) This tool loads exactly one master dictionary into `SekkaContext`. It never
//!     opens a dictionary holding the user's learning history. Everything printed
//!     to stdout is derived mechanically from the given master dictionary and
//!     romaji input.
//!
//! # Usage
//! ```sh
//! cargo run --release --example inspect-candidates -- <dictionary path> <romaji input>...
//! ```
//!
//! It contains no candidate-generation logic of its own. It only drives
//! `SekkaContext` and `ImmutableFileDict` through the same path as production
//! (`process_key` one character at a time -> Ctrl-J to stage the commit display ->
//! Ctrl-J again to reselect -> `get_candidates()`).

use std::process;
use std::sync::Arc;

use sekka::context::SekkaContext;
use sekka::dictionary::immutable_dict::ImmutableFileDict;
use sekka::dictionary::Dictionary;

fn print_usage() {
    eprintln!("usage: inspect-candidates <dictionary path> <romaji input>...");
    eprintln!();
    eprintln!("Loads one master dictionary and, for each romaji input given, prints the");
    eprintln!("commit display (the first candidate) and every candidate with its tier, score and frequency.");
}

/// Feeds romaji input, presses Ctrl-J (staging the commit display in the preedit)
/// and Ctrl-J again (entering reselection), then returns
/// (the commit display string, every candidate).
///
/// Follows the same keystroke sequence as the test helper `commit_then_reselect`
/// in `libsekka/src/context.rs` (logic is not reimplemented here, so that this
/// never measures a path different from production).
fn commit_and_collect_candidates(
    ctx: &mut SekkaContext,
    input: &str,
) -> Result<(String, Vec<sekka::candidate::Candidate>), String> {
    for ch in input.chars() {
        ctx.process_key(ch, false);
    }
    let staged = ctx.process_key('\0', true);
    if !staged {
        return Err(format!(
            "Ctrl-J did not stage the first candidate in the preedit for input {}",
            input
        ));
    }
    let committed = ctx.get_preedit().to_string();
    let entered = ctx.process_key('\0', true);
    if !entered {
        return Err(format!(
            "a second Ctrl-J from the commit display state did not enter reselection for input {}",
            input
        ));
    }
    let candidates = ctx.get_candidates().to_vec();
    Ok((committed, candidates))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        print_usage();
        process::exit(1);
    }

    let dict_path = &args[1];
    let inputs = &args[2..];

    let dict = match ImmutableFileDict::open(dict_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot open the dictionary file: {}: {}", dict_path, e);
            process::exit(1);
        }
    };
    let dict: Arc<dyn Dictionary> = Arc::new(dict);

    for input in inputs {
        let mut ctx = SekkaContext::new();
        ctx.set_dictionaries(vec![Arc::clone(&dict)]);

        match commit_and_collect_candidates(&mut ctx, input) {
            Ok((committed, candidates)) => {
                println!("=== input: {} ===", input);
                println!("commit display: {}", committed);
                println!("candidates: {}", candidates.len());
                for (rank, candidate) in candidates.iter().enumerate() {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        rank,
                        candidate.tier,
                        candidate.score,
                        candidate.frequency,
                        candidate.display
                    );
                }
            }
            Err(e) => {
                eprintln!("measuring input {} failed: {}", input, e);
                process::exit(1);
            }
        }
    }
}
