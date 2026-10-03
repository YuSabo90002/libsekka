// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture builder that writes a v1.3-shaped user dictionary
//!
//! (a) The values are exactly what v1.3 stored: the serialization of v1.3's
//!     `DictEntry` (field order `word, annotation, frequency`, where `frequency` was
//!     the number of times the word was selected). v1.4 reads such a value once, in
//!     `UserDict::open`, and rewrites it with last-selected sequence numbers
//!     (D-195). This tool exists so that the E2E and the hands-on UAT of that
//!     migration have a v1.3-shaped dictionary to start from.
//!
//! (b) The contents are fixed:
//!     - `かんじ` -> `[{"word":"漢字","annotation":null,"frequency":1},{"word":"幹事","annotation":null,"frequency":5}]`
//!     - `かんj`  -> `[{"word":"感","annotation":null,"frequency":2}]`
//!
//! (c) It never writes to a path that already exists (exit code 2), so pointing it
//!     at a real user dictionary by mistake cannot damage that dictionary. Give it
//!     a new path, for example one under an isolated `HOME`.
//!
//! # Usage
//! ```sh
//! cargo run --release --example make-legacy-user-dict -- <path of a new user dictionary>
//! ```

use std::path::Path;
use std::process;

/// The v1.3 values to write: `(reading, JSON of Vec<DictEntry> as v1.3 wrote it)`.
const LEGACY_ROWS: [(&str, &str); 2] = [
    (
        "かんじ",
        r#"[{"word":"漢字","annotation":null,"frequency":1},{"word":"幹事","annotation":null,"frequency":5}]"#,
    ),
    (
        "かんj",
        r#"[{"word":"感","annotation":null,"frequency":2}]"#,
    ),
];

fn print_usage() {
    eprintln!("usage: make-legacy-user-dict <path of a new user dictionary>");
    eprintln!();
    eprintln!("Writes a v1.3-shaped user dictionary (selection counts, no last-selected");
    eprintln!("numbers) at the given path. It refuses to write to a path that already exists.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 || args[1] == "--help" || args[1] == "-h" {
        print_usage();
        process::exit(if args.len() == 2 { 0 } else { 2 });
    }

    let path = Path::new(&args[1]);
    if path.exists() {
        eprintln!(
            "error: {} already exists; refusing to write a v1.3-shaped dictionary over it",
            path.display()
        );
        process::exit(2);
    }

    let db = match sled::open(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot create the dictionary: {e}");
            process::exit(1);
        }
    };
    for (reading, json) in LEGACY_ROWS {
        if let Err(e) = db.insert(reading.as_bytes(), json.as_bytes()) {
            eprintln!("error: cannot write an entry: {e}");
            process::exit(1);
        }
    }
    if let Err(e) = db.flush() {
        eprintln!("error: cannot flush the dictionary: {e}");
        process::exit(1);
    }
}
