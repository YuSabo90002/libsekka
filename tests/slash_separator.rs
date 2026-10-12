// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Phase 15 integration tests for the slash separator (SLASH-01 to SLASH-05)
//!
//! These tests use only the C ABI (`sekka::capi`). They replay what
//! fcitx5-sekka's `keyEvent` does for every key press: hand the key to the
//! library, and when it is consumed drain the committed output until the queue
//! is empty and take the forward-key mark. A key the library does not consume
//! reaches the application untouched, which `Session::press` reproduces for the
//! printable keys and BackSpace.
//!
//! The file lives here, and not in the unit tests of `context.rs` / `capi.rs`,
//! because Phase 15 may only add new test files (the covered_files boundary of
//! ROADMAP v1.5). Every dictionary is written by `dict_format::write_dict`.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};

use sekka::capi::*;
use sekka::dictionary::dict_format;
use sekka::dictionary::DictEntry;

// The same values as fcitx5's `KeyState` and as the private constants in
// `capi.rs` (`MOD_CTRL`, `MOD_ALT`).
const MOD_CTRL: u32 = 0x4;
const MOD_ALT: u32 = 0x8;

const KEY_SLASH: u32 = 0x2F;
const KEY_SPACE: u32 = 0x20;
const KEY_PLUS: u32 = 0x2B;
const KEY_RETURN: u32 = 0xFF0D;
const KEY_ESCAPE: u32 = 0xFF1B;
const KEY_BACKSPACE: u32 = 0xFF08;

/// One input context plus the application it types into.
struct Session {
    ctx: *mut SekkaContextFfi,
    dict: *mut SekkaDictionaryFfi,
    // Dropped after `Drop::drop` ran, i.e. after the mapped dictionary is freed.
    _dir: tempfile::TempDir,
    /// What the application received (commits, plus unconsumed keys).
    app: String,
    /// Every CommitString, one entry per `sekka_context_poll_output` item.
    commits: Vec<String>,
    /// The keysym of every key forwarded to the application.
    forwarded: Vec<u32>,
}

impl Session {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        let mut map: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        map.insert("にほんご".to_string(), vec![DictEntry::new("日本語")]);
        map.insert("えいご".to_string(), vec![DictEntry::new("英語")]);
        let path = dir.path().join("slash.dict");
        // Written once and never rewritten while it is mapped.
        dict_format::write_dict(&path, &map).expect("write the dictionary");
        let c_path = CString::new(path.to_str().expect("utf-8 path")).expect("no NUL");
        let mut dict = unsafe { sekka_file_dict_new(c_path.as_ptr(), std::ptr::null()) };
        assert!(!dict.is_null(), "open the dictionary");
        let ctx = sekka_context_new();
        assert!(!ctx.is_null(), "create the context");
        unsafe { sekka_context_set_dictionaries(ctx, &mut dict, 1) };
        Session {
            ctx,
            dict,
            _dir: dir,
            app: String::new(),
            commits: Vec::new(),
            forwarded: Vec::new(),
        }
    }

    /// Drains the committed output into `commits` and `app`.
    fn drain_output(&mut self) {
        loop {
            let p = unsafe { sekka_context_poll_output(self.ctx) };
            if p.is_null() {
                break;
            }
            let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
            unsafe { sekka_free_string(p) };
            self.app.push_str(&s);
            self.commits.push(s);
        }
    }

    /// What `keyEvent` does after the library consumed a key.
    fn after_consumed(&mut self, keysym: u32) {
        self.drain_output();
        if unsafe { sekka_context_take_forward_key(self.ctx) } == 1 {
            self.forwarded.push(keysym);
            if keysym == 0xFF08 {
                self.app.pop();
            }
        }
    }

    /// What the application does with a key the library did not consume.
    fn pass_through(&mut self, keysym: u32, modifiers: u32) {
        if modifiers != 0 {
            return;
        }
        if keysym == 0xFF08 {
            self.app.pop();
        } else if (0x20..=0x7E).contains(&keysym) {
            self.app.push(char::from(keysym as u8));
        }
    }

    fn press(&mut self, keysym: u32, modifiers: u32) -> bool {
        let consumed =
            unsafe { sekka_context_process_key_event(self.ctx, keysym, modifiers, 0) } == 1;
        if consumed {
            self.after_consumed(keysym);
        } else {
            self.pass_through(keysym, modifiers);
        }
        consumed
    }

    /// Types ASCII text, returns how many keys were consumed.
    fn typ(&mut self, text: &str) -> usize {
        text.chars()
            .filter(|ch| self.press(u32::from(*ch), 0))
            .count()
    }

    /// Ctrl plus a letter (the C++ side passes the uppercase keysym).
    fn ctrl(&mut self, letter: u8) -> bool {
        self.press(u32::from(letter.to_ascii_uppercase()), MOD_CTRL)
    }

    /// The trigger key (Ctrl-J).
    fn trigger(&mut self) -> bool {
        let consumed = unsafe { sekka_context_trigger(self.ctx) } == 1;
        if consumed {
            self.after_consumed(u32::from(b'J'));
        }
        consumed
    }

    fn preedit(&self) -> String {
        let p = unsafe { sekka_context_get_preedit(self.ctx) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { sekka_free_string(p) };
        s
    }

    fn candidate_count(&self) -> i32 {
        unsafe { sekka_context_get_candidate_count(self.ctx) }
    }

    /// Same order as `SekkaState::reset` (D-14 / D-15).
    fn reset(&mut self, commit_pending: bool) {
        if commit_pending {
            unsafe { sekka_context_finalize_for_reset(self.ctx) };
            self.drain_output();
        }
        unsafe { sekka_context_reset(self.ctx) };
    }

    fn is_registering(&self) -> bool {
        (unsafe { sekka_context_is_registering(self.ctx) }) == 1
    }

    fn registration_reading(&self) -> String {
        let p = unsafe { sekka_context_get_registration_reading(self.ctx) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { sekka_free_string(p) };
        s
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            sekka_context_free(self.ctx);
            sekka_free_dictionary(self.dict);
        }
    }
}

#[test]
fn linux_slash_nihongo_ctrl_j_gives_linux_nihongo_in_14_keystrokes() {
    // SLASH-01 / D-220 / D-221 / D-224 row 1: Linux / Nihongo Ctrl-J, 14 keys.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux"), 5);
    assert!(s.press(KEY_SLASH, 0), "the slash is consumed");
    assert_eq!(s.commits, ["Linux"], "the left side is committed as it is");
    assert!(s.forwarded.is_empty());
    assert_eq!(s.preedit(), "/");
    assert_eq!(s.typ("Nihongo"), 7);
    assert_eq!(s.preedit(), "/Nihongo");
    assert_eq!(s.commits, ["Linux"]);
    assert!(s.trigger(), "Ctrl-J is consumed (14th key)");
    assert_eq!(s.preedit(), "日本語");
    assert_eq!(s.commits, ["Linux"], "conversion does not commit");
    assert!(s.press(KEY_SPACE, 0));
    assert_eq!(s.app, "Linux日本語 ");
    assert_eq!(s.commits, ["Linux", "日本語 "]);
    assert!(s.forwarded.is_empty());
}

#[test]
fn ctrl_j_on_a_lone_held_slash_is_consumed_and_changes_nothing() {
    // D-221a: nothing follows the held slash, so Ctrl-J has nothing to convert.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux"), 5);
    assert!(s.press(KEY_SLASH, 0));
    assert!(
        s.trigger(),
        "consumed, so the application never sees Ctrl-J"
    );
    assert_eq!(s.commits, ["Linux"]);
    assert!(s.forwarded.is_empty());
    assert_eq!(s.preedit(), "/");
    assert_eq!(s.typ("Nihongo"), 7);
    assert!(s.trigger());
    assert_eq!(s.preedit(), "日本語");
}

#[test]
fn second_slash_commits_the_held_slash_and_holds_a_new_one() {
    // SLASH-03 / D-224 row 2: Z / / Nihongo Ctrl-J, 11 keys.
    let mut s = Session::new();
    assert_eq!(s.typ("Z"), 1);
    assert!(s.press(KEY_SLASH, 0));
    assert!(s.press(KEY_SLASH, 0));
    assert_eq!(s.commits, ["Z", "/"], "the held slash is committed as text");
    assert_eq!(s.preedit(), "/", "and a new one is held");
    assert_eq!(s.typ("Nihongo"), 7);
    assert!(s.trigger());
    assert_eq!(s.commits, ["Z", "/"]);
    assert_eq!(s.preedit(), "日本語");
    assert_eq!(s.app, "Z/");
}

#[test]
fn https_double_slash_example_com_takes_20_keystrokes() {
    // SLASH-03 / D-224 row 4: https: / / example.com Space, 20 keys.
    let mut s = Session::new();
    let mut consumed = s.typ("https:");
    consumed += usize::from(s.press(KEY_SLASH, 0));
    consumed += usize::from(s.press(KEY_SLASH, 0));
    consumed += s.typ("example.com");
    consumed += usize::from(s.press(KEY_SPACE, 0));
    assert_eq!(consumed, 20);
    assert_eq!(s.commits, ["https:", "/", "/example.com "]);
    assert_eq!(s.app, "https://example.com ");
}

#[test]
fn slash_right_after_a_conversion_is_appended_and_not_held() {
    // SLASH-04 / D-224 row 3: Nihongo Ctrl-J / Eigo Ctrl-J, 14 keys.
    let mut s = Session::new();
    let mut consumed = s.typ("Nihongo");
    consumed += usize::from(s.trigger());
    consumed += usize::from(s.press(KEY_SLASH, 0));
    assert_eq!(
        s.commits,
        ["日本語/"],
        "joined to the shown word, in one commit"
    );
    assert_eq!(s.preedit(), "", "nothing is held");
    assert!(s.forwarded.is_empty());
    consumed += s.typ("Eigo");
    consumed += usize::from(s.trigger());
    assert_eq!(consumed, 14);
    assert_eq!(s.preedit(), "英語");
}

#[test]
fn slashes_at_the_start_of_a_line_pass_through_unconsumed() {
    // SLASH-04 / D-224 row 5: with an empty buffer nothing changes.
    let mut s = Session::new();
    assert!(!s.press(KEY_SLASH, 0));
    assert!(!s.press(KEY_SLASH, 0));
    assert!(s.commits.is_empty());
    assert!(s.forwarded.is_empty());
    assert_eq!(s.app, "//", "the application got both slashes itself");
    assert_eq!(s.preedit(), "");
}

#[test]
fn ctrl_j_drops_only_the_slash_right_before_the_converted_part() {
    // D-221 / D-224 row 6: a / b / c Ctrl-J, 6 keys.
    let mut s = Session::new();
    let mut consumed = s.typ("a/b/c");
    consumed += usize::from(s.trigger());
    assert_eq!(consumed, 6);
    assert_eq!(s.commits, ["a", "/b"]);
    assert_eq!(s.app, "a/b");

    // The converted part is just `c`: the same preedit as in a fresh session.
    let mut alone = Session::new();
    assert_eq!(alone.typ("c"), 1);
    assert!(alone.trigger());
    assert_eq!(s.preedit(), alone.preedit());

    let mut t = Session::new();
    assert_eq!(t.typ("a/b/Nihongo"), 11);
    assert!(t.trigger());
    assert_eq!(t.commits, ["a", "/b"]);
    assert_eq!(t.preedit(), "日本語");
}

#[test]
fn space_symbol_enter_and_esc_commit_the_held_slash_as_text() {
    // SLASH-02 / D-224 rows 7 and 8, plus a symbol and Esc.
    let mut s = Session::new();
    let mut consumed = s.typ("and/or");
    consumed += usize::from(s.press(KEY_SPACE, 0));
    assert_eq!(consumed, 7);
    assert_eq!(s.commits, ["and", "/or "]);
    assert!(s.forwarded.is_empty());
    assert_eq!(s.preedit(), "");

    let mut s = Session::new();
    assert_eq!(s.typ("and/or"), 6);
    assert!(s.press(KEY_PLUS, 0));
    assert_eq!(s.commits, ["and", "/or+"]);
    assert!(s.forwarded.is_empty());
    assert_eq!(s.preedit(), "");

    let mut s = Session::new();
    let mut consumed = s.typ("usr/bin");
    consumed += usize::from(s.press(KEY_RETURN, 0));
    assert_eq!(consumed, 8);
    assert_eq!(s.commits, ["usr", "/bin"]);
    assert_eq!(s.forwarded, [KEY_RETURN]);
    assert_eq!(s.preedit(), "");

    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nih"), 9);
    assert!(s.press(KEY_ESCAPE, 0));
    assert_eq!(s.commits, ["Linux", "/nih"]);
    assert_eq!(s.forwarded, [KEY_ESCAPE]);
    assert_eq!(s.preedit(), "");
}

#[test]
fn and_or_z_c_slash_takes_11_keystrokes_and_keeps_the_last_slash_held() {
    // SLASH-02 / D-224 row 9.
    let mut s = Session::new();
    assert_eq!(s.typ("and/or/Z/c/"), 11);
    assert_eq!(s.commits, ["and", "/or", "/Z", "/c"]);
    assert_eq!(s.preedit(), "/");
    assert!(s.forwarded.is_empty());
}

#[test]
fn backspace_after_converting_from_a_held_slash_reverts_to_the_slash() {
    // D-222 / D-161: BackSpace brings the slash back together with the romaji.
    let mut s = Session::new();
    let mut consumed = s.typ("Linux/Nihongi");
    consumed += usize::from(s.trigger());
    assert!(s.press(KEY_BACKSPACE, 0));
    consumed += 1;
    assert_eq!(s.preedit(), "/Nihong");
    assert_eq!(s.commits, ["Linux"]);
    assert!(s.forwarded.is_empty());
    consumed += s.typ("o");
    consumed += usize::from(s.trigger());
    assert_eq!(consumed, 17);
    assert_eq!(s.preedit(), "日本語");
    assert!(s.press(KEY_SPACE, 0));
    assert_eq!(s.app, "Linux日本語 ");

    // Reselecting (Ctrl-J twice): the window closes and the slash comes back.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/Nihongi"), 13);
    assert!(s.trigger());
    assert!(s.trigger());
    assert!(s.candidate_count() > 0);
    assert!(s.press(KEY_BACKSPACE, 0));
    assert_eq!(s.preedit(), "/Nihong");
    assert_eq!(s.candidate_count(), 0);
    assert_eq!(s.commits, ["Linux"]);

    // Only the slash is left after removing the last letter.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/N"), 7);
    assert!(s.trigger());
    assert!(s.press(KEY_BACKSPACE, 0));
    assert_eq!(s.preedit(), "/");
}

#[test]
fn backspace_on_a_lone_held_slash_empties_the_preedit_then_reaches_the_app() {
    // D-222a: the committed text is never taken back from the application.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/"), 6);
    assert!(s.press(KEY_BACKSPACE, 0), "consumed");
    assert_eq!(s.preedit(), "");
    assert_eq!(s.commits, ["Linux"]);
    assert!(s.forwarded.is_empty());
    assert!(!s.press(KEY_BACKSPACE, 0), "the next one is not consumed");
    assert_eq!(s.app, "Linu");

    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nih"), 9);
    assert!(s.press(KEY_BACKSPACE, 0));
    assert_eq!(s.preedit(), "/ni");
}

#[test]
fn focus_out_and_im_switch_treat_the_held_slash_like_romaji() {
    // D-225 (D-14 / D-15): focus loss commits nothing, an IM switch commits the
    // converted word but never the un-converted romaji, held slash included.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nih"), 9);
    s.reset(false);
    assert_eq!(s.app, "Linux");
    assert_eq!(s.preedit(), "");
    assert!(!s.press(KEY_SPACE, 0), "no leftover `/nih` to commit");
    assert_eq!(s.app, "Linux ");

    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nih"), 9);
    s.reset(true);
    assert_eq!(s.app, "Linux");
    assert_eq!(s.commits, ["Linux"]);

    let mut s = Session::new();
    assert_eq!(s.typ("Linux/Nihongo"), 13);
    assert!(s.trigger());
    s.reset(false);
    assert_eq!(s.app, "Linux");

    let mut s = Session::new();
    assert_eq!(s.typ("Linux/Nihongo"), 13);
    assert!(s.trigger());
    s.reset(true);
    assert_eq!(s.app, "Linux日本語");
    assert_eq!(s.commits, ["Linux", "日本語"]);
}

#[test]
fn ctrl_r_after_converting_from_a_held_slash_reads_without_the_slash() {
    // D-226: the reading for word registration never contains the slash.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/Nihongo"), 13);
    assert!(s.trigger());
    assert!(s.ctrl(b'r'));
    assert!(s.is_registering());
    assert_eq!(s.registration_reading(), "にほんご");
}

#[test]
fn ctrl_r_while_holding_commits_the_romaji_and_forwards_ctrl_r() {
    // D-226: before converting, Ctrl-R keeps its D-03 / D-162 path.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nih"), 9);
    assert!(s.ctrl(b'r'));
    assert_eq!(s.commits, ["Linux", "/nih"]);
    assert_eq!(s.forwarded, [u32::from(b'R')]);
    assert_eq!(s.preedit(), "");
    assert!(!s.is_registering());
}

#[test]
fn slash_inside_word_registration_registers_tcp_slash_ip() {
    // D-227: a slash typed in the word field is part of the word.
    let mut s = Session::new();
    assert_eq!(s.typ("Nihongo"), 7);
    assert!(s.trigger());
    assert!(s.ctrl(b'r'));
    assert!(s.is_registering());
    assert_eq!(s.typ("TCP/IP"), 6);
    assert_eq!(s.preedit(), "TCP/IP");
    assert!(s.press(KEY_RETURN, 0));
    assert!(!s.is_registering());
    assert_eq!(s.commits, ["TCP/IP"]);
    assert!(s.forwarded.is_empty());
}

#[test]
fn slash_while_reselecting_confirms_and_is_appended() {
    // D-220 (reselecting): the chosen candidate is confirmed with the slash joined.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/Nihongo"), 13);
    assert!(s.trigger());
    assert!(s.trigger());
    assert!(s.candidate_count() > 0);
    assert!(s.press(KEY_SLASH, 0));
    assert_eq!(s.commits, ["Linux", "日本語/"]);
    assert_eq!(s.preedit(), "", "nothing is held");
    assert!(s.forwarded.is_empty());
    assert_eq!(s.typ("Eigo"), 4);
    assert!(s.trigger());
    assert_eq!(s.preedit(), "英語");
}

#[test]
fn alt_slash_and_ctrl_slash_keep_their_existing_paths() {
    // D-220 only takes an unmodified slash: Alt is D-185, Ctrl is D-03 / D-162.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nih"), 9);
    assert!(s.press(KEY_SLASH, MOD_ALT));
    assert_eq!(s.commits, ["Linux", "/nih/"]);
    assert!(s.forwarded.is_empty());

    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nih"), 9);
    assert!(s.press(KEY_SLASH, MOD_CTRL));
    assert_eq!(s.commits, ["Linux", "/nih"]);
    assert_eq!(s.forwarded, [KEY_SLASH]);
}

#[test]
fn lowercase_nihongo_after_a_slash_keeps_the_hiragana_first_rule() {
    // The all-lowercase rule of v1.0 (hiragana is the first candidate) is
    // unchanged by the slash, because the slash is dropped before classifying.
    let mut s = Session::new();
    assert_eq!(s.typ("Linux/nihongo"), 13);
    assert!(s.trigger());
    assert_eq!(s.commits, ["Linux"]);
    assert_eq!(s.preedit(), "にほんご");
}
