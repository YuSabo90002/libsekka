// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Dictionary conversion CLI tool
//!
//! Converts SKK-JISYO.L into the immutable master dictionary format (a single
//! file, read via mmap, with the romaji index bundled in).
//!
//! # Usage
//! ```sh
//! sekka-dict-tool convert <input file> --output <output path> [--encoding <auto|utf-8|euc-jp>]
//! ```
//!
//! # Note
//! The encoding of the input file is given with `--encoding` (default `auto`).
//! `auto` uses the bytes as they are when they decode as UTF-8 and otherwise
//! decodes them as EUC-JP. SKK-JISYO.L is normally encoded in EUC-JP, so no
//! manual pre-conversion with an external command is needed any more. When
//! EUC-JP decoding produces replacement characters (`had_errors`) this is treated
//! as an error and the tool exits non-zero.

use sekka::dictionary::dict_format;
use sekka::dictionary::DictEntry;
use std::collections::BTreeMap;
use std::path::Path;
use std::process;

/// Prints the usage and exits
fn print_usage() {
    eprintln!(
        "usage: sekka-dict-tool convert <input file> --output <output path> [--encoding <auto|utf-8|euc-jp>]"
    );
    eprintln!("     : sekka-dict-tool dump <user dictionary path>");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!("  convert    convert from the SKK-JISYO format into the immutable master dictionary format");
    eprintln!("  dump       print the contents of a user dictionary (reading, word, frequency) as tab-separated columns");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --output, -o <path>    path of the dictionary file to write");
    eprintln!("  --encoding <value>     encoding of the input file (default: auto)");
    eprintln!(
        "                         auto:    read as EUC-JP when the bytes are not valid UTF-8"
    );
    eprintln!("                         utf-8:   read strictly as UTF-8 (error on failure)");
    eprintln!("                         euc-jp:  read as EUC-JP");
    eprintln!("  --force, -f            when the output path is an existing directory, remove it recursively and overwrite");
    eprintln!();
    eprintln!("Constraints on dump:");
    eprintln!(
        "  It cannot run while fcitx5 has the same user dictionary open (sled's single-process exclusive lock)."
    );
    eprintln!("  The output contains words the user typed in plain text, so do not paste it anywhere shared.");
}

/// Formats a reading, word and frequency as one line of three tab-separated columns (D-101)
///
/// The `annotation` is not printed. SC2 asks about the reading, the word and the
/// frequency; adding columns would make the E2E-side parsing depend on whether an
/// annotation is present.
fn format_dump_line(reading: &str, entry: &DictEntry) -> String {
    format!("{}\t{}\t{}", reading, entry.word, entry.frequency)
}

/// Opens a user dictionary read-only and prints every entry to stdout as three
/// tab-separated columns (D-101)
///
/// It calls no API other than `UserDict::open` and `Dictionary::prefix_search`
/// (T-04-03-02: dump must not modify the user dictionary).
///
/// When `UserDict::open` fails, it prints the `Display` of `DictError` plus a note
/// that fcitx5 may be using the file and exits on the spot (rather than layering
/// error handling in the caller), because sled's single-process exclusive lock
/// (`fs2::FileExt::try_lock_exclusive`, non-blocking) is inherently hit while
/// fcitx5 has the same user dictionary open. When `prefix_search` fails, the
/// `DictError` is returned to the caller as it is (with no note, since the cause
/// is something other than lock contention).
fn dump(path: &str) -> Result<(), sekka::dictionary::DictError> {
    let dict = match sekka::dictionary::user_dict::UserDict::open(path) {
        Ok(dict) => dict,
        Err(e) => {
            eprintln!("error: cannot open the user dictionary: {}", e);
            eprintln!("(fcitx5 may be using this file; try again after it exits)");
            process::exit(1);
        }
    };

    let results = sekka::dictionary::Dictionary::prefix_search(&dict, "")?;
    for (reading, entries) in results {
        for entry in entries {
            println!("{}", format_dump_line(&reading, &entry));
        }
    }

    Ok(())
}

/// Parses one line of an SKK dictionary and returns (reading, candidate list)
///
/// # Format
/// `reading /candidate1/candidate2;annotation/candidate3/`
///
/// When a candidate contains `;`, the part before it is the converted text and the
/// part after it is the annotation.
fn parse_skk_line(line: &str) -> Option<(String, Vec<DictEntry>)> {
    // Skip comment lines.
    if line.starts_with(";;") {
        return None;
    }

    // Skip blank lines.
    let line = line.trim();
    if line.is_empty() {
        return None;
    }

    // Split the reading from the candidate part.
    // Format: "reading /candidate1/candidate2/"
    // A space follows the reading, then the candidate list beginning with "/".
    let space_pos = line.find(' ')?;
    let reading = &line[..space_pos];
    let candidates_part = line[space_pos..].trim();

    // A line whose candidate part does not start with "/" is malformed.
    if !candidates_part.starts_with('/') {
        return None;
    }

    // Split on "/" to get the candidates.
    let entries: Vec<DictEntry> = candidates_part
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|candidate| {
            // Separate the annotation when the candidate contains ";".
            if let Some(semicolon_pos) = candidate.find(';') {
                let word = &candidate[..semicolon_pos];
                let annotation = &candidate[semicolon_pos + 1..];
                DictEntry {
                    word: word.to_string(),
                    annotation: if annotation.is_empty() {
                        None
                    } else {
                        Some(annotation.to_string())
                    },
                    frequency: 0,
                }
            } else {
                DictEntry {
                    word: candidate.to_string(),
                    annotation: None,
                    frequency: 0,
                }
            }
        })
        .collect();

    if entries.is_empty() {
        return None;
    }

    Some((reading.to_string(), entries))
}

/// Decodes the input bytes into text according to `encoding`.
///
/// - `"utf-8"`: strict UTF-8 decoding; an error on failure.
/// - `"euc-jp"`: decode with `encoding_rs::EUC_JP`. A true `had_errors` means
///   replacement characters crept in, which is an error (02.1-RESEARCH.md
///   Pitfall 4).
/// - `"auto"`: try UTF-8 first and use it on success; otherwise decode as EUC-JP
///   (an error when `had_errors` is true).
/// - anything else: an error for an unknown encoding name.
fn decode_input(bytes: &[u8], encoding: &str) -> Result<String, String> {
    match encoding {
        "utf-8" => std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|e| format!("the input file is not valid UTF-8: {}", e)),
        "euc-jp" => {
            let (text, _, had_errors) = encoding_rs::EUC_JP.decode(bytes);
            if had_errors {
                Err(
                    "could not decode the input file as EUC-JP (it contains invalid bytes)"
                        .to_string(),
                )
            } else {
                Ok(text.into_owned())
            }
        }
        "auto" => {
            if let Ok(s) = std::str::from_utf8(bytes) {
                Ok(s.to_string())
            } else {
                let (text, _, had_errors) = encoding_rs::EUC_JP.decode(bytes);
                if had_errors {
                    Err("the input file is neither UTF-8 nor EUC-JP".to_string())
                } else {
                    Ok(text.into_owned())
                }
            }
        }
        other => Err(format!(
            "unknown encoding name: {} (specify one of auto/utf-8/euc-jp)",
            other
        )),
    }
}

/// Reads an SKK dictionary file and converts it into the immutable master
/// dictionary format
///
/// When `force` is false and the output path is an existing **directory** (an old
/// sled-format dictionary, for instance), it aborts with an error (02.1-REVIEW
/// WR-05: this prevents the accident where a mistyped `-o`/`--output` recursively
/// deletes the home directory or an existing user dictionary directory with
/// virtually no warning - just one line that is easily lost in build logs).
/// Overwriting a single file stays allowed without `--force`, as ordinary CLI
/// behaviour.
fn convert(input_path: &str, output_path: &str, encoding: &str, force: bool) -> Result<(), String> {
    let input = Path::new(input_path);
    if !input.exists() {
        return Err(format!("input file not found: {}", input_path));
    }

    // Warn and remove an output path that already exists. Both a file (the
    // immutable format) and a directory (the old sled format) can be overwritten,
    // but recursively removing a directory requires an explicit --force.
    let output = Path::new(output_path);
    if output.exists() {
        if output.is_dir() {
            if !force {
                return Err(format!(
                    "the output path {} is an existing directory. To remove it and \
                     overwrite, pass --force/-f explicitly; this guards against \
                     accidentally deleting an important directory recursively.",
                    output_path
                ));
            }
            eprintln!(
                "warning: the output path {} is an existing directory. --force was given, so it is removed recursively and overwritten.",
                output_path
            );
            std::fs::remove_dir_all(output)
                .map_err(|e| format!("failed to remove the existing output path: {}", e))?;
        } else {
            eprintln!(
                "warning: the output path {} already exists and is overwritten.",
                output_path
            );
            std::fs::remove_file(output)
                .map_err(|e| format!("failed to remove the existing output path: {}", e))?;
        }
    }

    // Read the input file as bytes and decode it into text with the given encoding.
    let bytes = std::fs::read(input)
        .map_err(|e| format!("cannot open the input file: {}: {}", input_path, e))?;
    let text = decode_input(&bytes, encoding)?;

    // Parse every line into memory first. A BTreeMap doubles as the
    // key-sorted intermediate representation write_dict requires (and also handles
    // the case where one reading is spread over several lines).
    let mut dict_map: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
    let mut line_count: u64 = 0;
    // Decoding is already done, so reading lines cannot raise I/O errors of its
    // own. error_count counts only lines that failed to parse.
    let mut skip_count: u64 = 0;
    let mut error_count: u64 = 0;

    for (line_num, line) in text.lines().enumerate() {
        line_count += 1;

        // Skip comment lines and blank lines.
        if line.starts_with(";;") || line.trim().is_empty() {
            skip_count += 1;
            continue;
        }

        match parse_skk_line(line) {
            Some((reading, entries)) => {
                dict_map.entry(reading).or_default().extend(entries);
            }
            None => {
                // A line that failed to parse (malformed format).
                eprintln!("warning: failed to parse line {}: {}", line_num + 1, line);
                error_count += 1;
            }
        }
    }

    let entry_count = dict_map.len() as u64;
    let candidate_count: u64 = dict_map.values().map(|entries| entries.len() as u64).sum();

    // Write out in the immutable master dictionary format. Building the header,
    // the offset tables and the romaji index is entirely write_dict's job; the
    // format is not reimplemented here.
    dict_format::write_dict(output, &dict_map)
        .map_err(|e| format!("failed to write the dictionary file: {}", e))?;

    // Make the generated dictionary read-only (0444). The master dictionary is
    // mmapped on the fcitx5 side and rewriting it while mapped takes the whole
    // process down with SIGBUS, so the fix for 02.1-REVIEW CR-01 makes
    // `openDictionaries` refuse to load a path the running user can write. Keeping
    // that invariant is the job of the tool that generated the dictionary: left at
    // 0644, a dictionary a user converted themselves into ~/.local/share would be
    // refused and conversion would silently stay in hiragana.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o444)).map_err(|e| {
            format!(
                "could not make the generated dictionary file read-only: {}",
                e
            )
        })?;
    }

    // write_dict does not return the index count, so reopen the written file and
    // read roman_count from the header along with the file size (02.1-03-PLAN.md
    // action item 2).
    let written_bytes = std::fs::read(output)
        .map_err(|e| format!("failed to read back the generated dictionary file: {}", e))?;
    let file_size = written_bytes.len() as u64;
    let roman_count = dict_format::DictView::new(&written_bytes).roman_count() as u64;

    // Print the statistics.
    println!("=== conversion complete ===");
    println!("input file:           {}", input_path);
    println!("output path:          {}", output_path);
    println!("total lines:          {}", line_count);
    println!("skipped lines:        {}", skip_count);
    println!("error lines:          {}", error_count);
    println!("headwords:            {}", entry_count);
    println!("total candidates:     {}", candidate_count);
    println!("dictionary file size: {} bytes", file_size);
    println!("romaji index entries: {}", roman_count);

    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Print the usage when arguments are missing.
    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    match args[1].as_str() {
        "convert" => {
            // Parse the form "convert <input file> --output <output path> [--encoding <value>]".
            if args.len() < 3 {
                eprintln!("error: no input file given.");
                print_usage();
                process::exit(1);
            }

            let input_file = &args[2];
            let mut output_path: Option<&str> = None;
            let mut encoding: &str = "auto";
            let mut force = false;

            // Look for the --output / -o / --encoding / --force / -f options.
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--output" | "-o" => {
                        if i + 1 >= args.len() {
                            eprintln!("error: --output was given no value.");
                            process::exit(1);
                        }
                        output_path = Some(&args[i + 1]);
                        i += 2;
                    }
                    "--encoding" => {
                        if i + 1 >= args.len() {
                            eprintln!("error: --encoding was given no value.");
                            process::exit(1);
                        }
                        let value = args[i + 1].as_str();
                        if !matches!(value, "auto" | "utf-8" | "euc-jp") {
                            eprintln!(
                                "error: --encoding takes one of auto/utf-8/euc-jp: {}",
                                value
                            );
                            process::exit(1);
                        }
                        encoding = value;
                        i += 2;
                    }
                    "--force" | "-f" => {
                        force = true;
                        i += 1;
                    }
                    _ => {
                        eprintln!("error: unknown option: {}", args[i]);
                        print_usage();
                        process::exit(1);
                    }
                }
            }

            let output_path = match output_path {
                Some(p) => p,
                None => {
                    eprintln!("error: --output was not given.");
                    print_usage();
                    process::exit(1);
                }
            };

            // Run the conversion.
            if let Err(e) = convert(input_file, output_path, encoding, force) {
                eprintln!("error: {}", e);
                process::exit(1);
            }
        }
        "dump" => {
            // Parse the form "dump <user dictionary path>".
            if args.len() < 3 {
                eprintln!("error: no user dictionary path given.");
                print_usage();
                process::exit(1);
            }

            // dump itself calls process::exit directly when UserDict::open fails,
            // so the only Err that reaches here comes from prefix_search.
            if let Err(e) = dump(&args[2]) {
                eprintln!("error: {}", e);
                process::exit(1);
            }
        }
        "--help" | "-h" | "help" => {
            print_usage();
        }
        other => {
            eprintln!("error: unknown subcommand: {}", other);
            print_usage();
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skk_line_basic() {
        // Parsing a basic entry.
        let result = parse_skk_line("あい /愛/藍/");
        assert!(result.is_some());
        let (reading, entries) = result.unwrap();
        assert_eq!(reading, "あい");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].word, "愛");
        assert_eq!(entries[0].annotation, None);
        assert_eq!(entries[0].frequency, 0);
        assert_eq!(entries[1].word, "藍");
    }

    #[test]
    fn test_parse_skk_line_with_annotation() {
        // Parsing a candidate with an annotation.
        let result = parse_skk_line("きょう /今日;本日のこと/京/");
        assert!(result.is_some());
        let (reading, entries) = result.unwrap();
        assert_eq!(reading, "きょう");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].word, "今日");
        assert_eq!(entries[0].annotation, Some("本日のこと".to_string()));
        assert_eq!(entries[1].word, "京");
        assert_eq!(entries[1].annotation, None);
    }

    #[test]
    fn test_parse_skk_line_comment() {
        // A comment line returns None.
        let result = parse_skk_line(";; これはコメントです");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_skk_line_empty() {
        // A blank line returns None.
        assert!(parse_skk_line("").is_none());
        assert!(parse_skk_line("   ").is_none());
    }

    #[test]
    fn test_parse_skk_line_single_candidate() {
        // A single candidate.
        let result = parse_skk_line("ひと /人/");
        assert!(result.is_some());
        let (reading, entries) = result.unwrap();
        assert_eq!(reading, "ひと");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].word, "人");
    }

    #[test]
    fn test_parse_skk_line_invalid_format() {
        // A malformed line with no space.
        assert!(parse_skk_line("不正な行").is_none());
    }

    #[test]
    fn test_parse_skk_line_no_slash() {
        // A candidate part that does not start with "/".
        assert!(parse_skk_line("あ 不正").is_none());
    }

    #[test]
    fn test_dict_entry_serialization() {
        // JSON serialization of DictEntry.
        let entry = DictEntry {
            word: "東京".to_string(),
            annotation: Some("地名".to_string()),
            frequency: 0,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: DictEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, restored);
    }

    #[test]
    fn test_parse_skk_line_empty_annotation() {
        // An empty annotation after the semicolon yields None.
        let result = parse_skk_line("あ /亜;/");
        assert!(result.is_some());
        let (_, entries) = result.unwrap();
        assert_eq!(entries[0].word, "亜");
        assert_eq!(entries[0].annotation, None);
    }

    /// Reads the EUC-JP fixture with auto-detection and confirms the candidates of にほんご include 日本語.
    #[test]
    fn eucjp_fixture_is_read_with_auto_detection() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mini-dict-eucjp.skk"
        ))
        .expect("failed to read the EUC-JP fixture");
        let text = decode_input(&bytes, "auto").expect("auto-detected decoding failed");

        let mut found_nihongo = false;
        for line in text.lines() {
            if let Some((reading, entries)) = parse_skk_line(line) {
                if reading == "にほんご" {
                    found_nihongo = entries.iter().any(|e| e.word == "日本語");
                }
            }
        }
        assert!(
            found_nihongo,
            "the candidates of にほんご do not include 日本語"
        );
    }

    /// Confirms the UTF-8 mini dictionary is not damaged when read with auto-detection.
    #[test]
    fn utf8_mini_dictionary_survives_auto_detection() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mini-dict.skk"
        ))
        .expect("failed to read the UTF-8 mini dictionary");
        let text = decode_input(&bytes, "auto").expect("auto-detected decoding failed");

        let mut found_nihongo = false;
        for line in text.lines() {
            if let Some((reading, entries)) = parse_skk_line(line) {
                if reading == "にほんご" {
                    found_nihongo = entries.iter().any(|e| e.word == "日本語");
                }
            }
        }
        assert!(
            found_nihongo,
            "the candidates of にほんご do not include 日本語"
        );
    }

    /// Confirms that bytes which are neither UTF-8 nor EUC-JP yield an error.
    #[test]
    fn bytes_that_are_neither_utf8_nor_eucjp_error_out() {
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00];
        let result = decode_input(bytes, "auto");
        assert!(
            result.is_err(),
            "invalid bytes should not decode successfully"
        );
    }

    /// convert produces a single immutable-format file from an SKK source and
    /// reopening it with ImmutableFileDict returns the original readings,
    /// candidates and order (the main verification). It also checks the
    /// precondition of D-74 (okuri-ari conventional keys are not indexed) and D-75
    /// (every collision between homophonous romaji is kept).
    #[test]
    fn convert_produces_a_single_immutable_format_file_and_round_trips() {
        use sekka::dictionary::immutable_dict::ImmutableFileDict;
        use sekka::dictionary::Dictionary;

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let input_path = tmp.path().join("input.skk");
        std::fs::write(
            &input_path,
            "にほんご /日本語/\nかんじ /漢字/幹事/\nかんい /簡易/\nかに /蟹/\nかんj /感/\nおこなu /行/\n",
        )
        .expect("failed to write the input file");
        let output_path = tmp.path().join("output.dict");

        convert(
            input_path.to_str().unwrap(),
            output_path.to_str().unwrap(),
            "auto",
            false,
        )
        .expect("convert failed");

        assert!(
            output_path.is_file(),
            "the output should be a single file, not a directory"
        );

        let dict = ImmutableFileDict::open(&output_path).expect("failed to open the dictionary");

        let nihongo = dict.lookup("にほんご").expect("lookup failed");
        assert!(nihongo.iter().any(|e| e.word == "日本語"));

        // The order of the SKK source is preserved (漢字 then 幹事).
        let kanji = dict.lookup("かんじ").expect("lookup failed");
        assert_eq!(kanji.len(), 2);
        assert_eq!(kanji[0].word, "漢字");
        assert_eq!(kanji[1].word, "幹事");

        // D-75: かんい and かに both canonicalize to the romaji kani and both are picked up.
        let bucket_ka = dict.roman_bucket("ka").expect("roman_bucket failed");
        let ka_readings: Vec<&str> = bucket_ka.iter().map(|rk| rk.reading.as_str()).collect();
        assert!(ka_readings.contains(&"かんい"), "{:?}", ka_readings);
        assert!(ka_readings.contains(&"かに"), "{:?}", ka_readings);
        // The precondition of D-74: the okuri-ari conventional key (かんj) is not indexed.
        assert!(!ka_readings.contains(&"かんj"), "{:?}", ka_readings);

        let bucket_ok = dict.roman_bucket("ok").expect("roman_bucket failed");
        let ok_readings: Vec<&str> = bucket_ok.iter().map(|rk| rk.reading.as_str()).collect();
        assert!(!ok_readings.contains(&"おこなu"), "{:?}", ok_readings);
    }

    /// 02.1-REVIEW WR-05: when the output path is an existing directory,
    /// `force: false` errors out without deleting it and leaves the directory and
    /// its contents untouched.
    #[test]
    fn an_existing_output_directory_errors_without_force_and_keeps_its_contents() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let input_path = tmp.path().join("input.skk");
        std::fs::write(&input_path, "にほんご /日本語/\n").expect("failed to write the input file");

        let output_dir = tmp.path().join("existing-dir");
        std::fs::create_dir(&output_dir).expect("failed to create the existing directory");
        let sentinel = output_dir.join("do-not-delete.txt");
        std::fs::write(&sentinel, "important file").expect("failed to write the sentinel file");

        let result = convert(
            input_path.to_str().unwrap(),
            output_dir.to_str().unwrap(),
            "auto",
            false,
        );

        assert!(
            result.is_err(),
            "converting into an existing directory without force should fail"
        );
        assert!(
            output_dir.is_dir(),
            "without force the existing directory should not be removed"
        );
        assert!(
            sentinel.exists(),
            "without force the contents of the existing directory should be untouched"
        );
    }

    /// In the same situation, passing `force: true` removes the existing directory
    /// and the conversion succeeds.
    #[test]
    fn an_existing_output_directory_is_removed_and_converted_with_force() {
        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let input_path = tmp.path().join("input.skk");
        std::fs::write(&input_path, "にほんご /日本語/\n").expect("failed to write the input file");

        let output_dir = tmp.path().join("existing-dir");
        std::fs::create_dir(&output_dir).expect("failed to create the existing directory");
        std::fs::write(output_dir.join("stale.txt"), "stale file")
            .expect("failed to write the sentinel file");

        convert(
            input_path.to_str().unwrap(),
            output_dir.to_str().unwrap(),
            "auto",
            true,
        )
        .expect("convert should succeed when force is given");

        assert!(
            output_dir.is_file(),
            "with force the existing directory should be removed and replaced by a single file"
        );
    }

    /// Confirms にほんご can be looked up in the output converted from the EUC-JP
    /// fixture (an end-to-end pass of EUC-JP -> UTF-8 -> parse -> immutable format
    /// write -> mmap read).
    #[test]
    fn a_dictionary_converted_from_the_eucjp_fixture_finds_nihongo() {
        use sekka::dictionary::immutable_dict::ImmutableFileDict;
        use sekka::dictionary::Dictionary;

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let output_path = tmp.path().join("eucjp-output.dict");
        let input_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mini-dict-eucjp.skk"
        );

        convert(input_path, output_path.to_str().unwrap(), "auto", false).expect("convert failed");

        let dict = ImmutableFileDict::open(&output_path).expect("failed to open the dictionary");
        let nihongo = dict.lookup("にほんご").expect("lookup failed");
        assert!(nihongo.iter().any(|e| e.word == "日本語"));
    }

    /// Confirms the generated dictionary is read-only (0444) and not writable by
    /// the running user. `openDictionaries` on the fcitx5 side refuses to load a
    /// writable path (02.1-REVIEW CR-01), so a dictionary generated as 0644 would
    /// be unusable as generated.
    #[cfg(unix)]
    #[test]
    fn the_convert_output_is_read_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let output_path = tmp.path().join("readonly-output.dict");
        let input_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mini-dict-eucjp.skk"
        );

        convert(input_path, output_path.to_str().unwrap(), "auto", false).expect("convert failed");

        let mode = std::fs::metadata(&output_path)
            .expect("failed to get the metadata of the output")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o444,
            "the generated dictionary should be 0444 (measured: {:o})",
            mode
        );

        // Check directly, by trying to open it, that the running user cannot write to it.
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&output_path)
                .is_err(),
            "the generated dictionary must not be writable by the running user (fcitx5 refuses to load it)"
        );
    }

    /// Output formatting of the `dump` subcommand (D-101): the reading, word and
    /// frequency as three tab-separated columns. The expected value is pinned
    /// verbatim.
    #[test]
    fn format_dump_line_lays_out_reading_word_and_frequency_tab_separated() {
        let entry = DictEntry::new("幹事").with_frequency(3);
        let line = format_dump_line("かんじ", &entry);
        assert_eq!(line, "かんじ\t幹事\t3");
    }

    /// Confirms `dump` can walk every entry without modifying the user dictionary
    /// (D-101 / T-04-03-02). It checks directly that the result of
    /// `prefix_search("")` is identical before and after the call.
    #[test]
    fn dump_walks_every_entry_without_writing() {
        use sekka::dictionary::user_dict::UserDict;
        use sekka::dictionary::Dictionary;

        let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
        let db_path = tmp.path().join("user-dict.db");

        {
            let dict = UserDict::open(&db_path).expect("failed to open the user dictionary");
            dict.record_selection("かんじ", "幹事")
                .expect("record_selection failed");
            dict.record_selection("かんじ", "漢字")
                .expect("record_selection failed");
            dict.record_selection("かんj", "感")
                .expect("record_selection failed");
            dict.save().expect("save failed");
        }

        let before = {
            let dict = UserDict::open(&db_path).expect("failed to reopen the user dictionary");
            Dictionary::prefix_search(&dict, "").expect("prefix_search failed")
        };

        let result = dump(db_path.to_str().unwrap());
        assert!(result.is_ok(), "dump should return Ok(()): {:?}", result);

        let after = {
            let dict =
                UserDict::open(&db_path).expect("failed to open the user dictionary a third time");
            Dictionary::prefix_search(&dict, "").expect("prefix_search failed")
        };

        assert_eq!(before, after, "the entries must not change across dump");
    }
}
