// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Phase 16 integration tests for the real-time guide (GUIDE-02 and GUIDE-03,
//! D-230 / D-231)
//!
//! These tests use only the C ABI (`sekka::capi`). They replay what
//! fcitx5-sekka's `keyEvent` does for every key press: hand the key to the
//! library, and when it is consumed drain the committed output until the queue
//! is empty and take the forward-key mark. A key the library does not consume
//! reaches the application untouched, which `Session::press` reproduces for the
//! printable keys and BackSpace.
//!
//! The boundaries of the 30-second window are checked with an injected clock in
//! the unit tests of `context.rs`. This file only looks at the real-time contract
//! of the C ABI: which keys and events open, extend and close the window, and what
//! `sekka_context_get_guide` / `sekka_context_guide_remaining_ms` return. Every
//! dictionary is written by `dict_format::write_dict`.
//!
//! A window extension is seen as "the remaining time right after the trigger is
//! greater than the remaining time just before it, after a 50 ms wait". The
//! remaining time is rounded up to whole milliseconds, so two readings taken
//! within the same millisecond of a trigger are equal and would prove nothing.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::thread::sleep;
use std::time::Duration;

use sekka::capi::*;
use sekka::dictionary::dict_format;
use sekka::dictionary::DictEntry;

// The same value as fcitx5's `KeyState::Ctrl` and the private constant in `capi.rs`.
const MOD_CTRL: u32 = 0x4;

const KEY_SLASH: u32 = 0x2F;
const KEY_ESCAPE: u32 = 0xFF1B;

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
        let path = dir.path().join("guide.dict");
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

    /// The guide text the input panel would show (empty when nothing is shown).
    fn guide(&self) -> String {
        let p = unsafe { sekka_context_get_guide(self.ctx) };
        assert!(!p.is_null(), "the guide is an empty string, never NULL");
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { sekka_free_string(p) };
        s
    }

    /// Milliseconds left of the guide window (0 when it is closed).
    fn remaining_ms(&self) -> i32 {
        unsafe { sekka_context_guide_remaining_ms(self.ctx) }
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
fn guide_is_empty_with_no_time_left_before_the_first_trigger() {
    // GUIDE-02: nothing is shown until the first Ctrl-J. Keys: Nihongo.
    let mut s = Session::new();
    assert_eq!(s.typ("Nihongo"), 7);
    assert_eq!(s.guide(), "");
    assert_eq!(s.remaining_ms(), 0);
}

#[test]
fn a_trigger_that_passes_through_still_opens_the_window() {
    // D-230 row 3: Ctrl-J with no buffer and no conversion is not consumed, but it
    // still opens the window. Keys: Ctrl-J, Nihongo.
    let mut s = Session::new();
    assert!(!s.trigger(), "the trigger passes through");
    assert!(s.commits.is_empty());
    assert!(s.forwarded.is_empty());
    let left = s.remaining_ms();
    assert!(left > 29_000 && left <= 30_001, "remaining {left} ms");
    s.typ("Nihongo");
    assert_eq!(s.guide(), "[日本語]");
}

#[test]
fn a_converting_trigger_extends_the_window() {
    // D-230 row 1: a Ctrl-J that converts restarts the 30 seconds.
    // Keys: Ctrl-J, Nihongo, (50 ms), Ctrl-J.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    sleep(Duration::from_millis(50));
    let before = s.remaining_ms();
    assert!(s.trigger(), "the second trigger converts");
    assert_eq!(s.preedit(), "日本語");
    assert!(s.remaining_ms() > before, "the window was not extended");
}

#[test]
fn a_reselecting_trigger_extends_the_window() {
    // D-230 row 2: a Ctrl-J that enters reselection restarts the 30 seconds.
    // Keys: Ctrl-J, Nihongo, Ctrl-J, (50 ms), Ctrl-J.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    assert!(s.trigger());
    sleep(Duration::from_millis(50));
    let before = s.remaining_ms();
    assert!(s.trigger(), "the third trigger enters reselection");
    assert!(s.candidate_count() > 0);
    assert!(s.remaining_ms() > before, "the window was not extended");
    assert_eq!(s.guide(), "", "no guide while reselecting");
}

#[test]
fn a_trigger_on_a_lone_held_slash_extends_the_window() {
    // D-230 row 4 / D-221a: Ctrl-J on a lone held `/` is consumed and changes
    // nothing, but restarts the window. Keys: Ctrl-J, Linux, /, (50 ms), Ctrl-J.
    let mut s = Session::new();
    s.trigger();
    s.typ("Linux");
    assert!(s.press(KEY_SLASH, 0));
    sleep(Duration::from_millis(50));
    let before = s.remaining_ms();
    assert!(s.trigger(), "the trigger on a lone slash is consumed");
    assert_eq!(s.preedit(), "/");
    assert_eq!(s.commits, ["Linux"]);
    assert!(s.remaining_ms() > before, "the window was not extended");
}

#[test]
fn a_trigger_inside_word_registration_extends_the_window() {
    // D-230 row 5: Ctrl-J inside word registration restarts the window too.
    // Keys: Ctrl-J, Nihongo, Ctrl-J, Ctrl-R, Eigo, (50 ms), Ctrl-J.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    assert!(s.trigger());
    assert!(s.ctrl(b'r'));
    assert!(s.is_registering());
    s.typ("Eigo");
    sleep(Duration::from_millis(50));
    let before = s.remaining_ms();
    assert!(s.trigger(), "the trigger inside registration is consumed");
    assert!(s.is_registering());
    assert_eq!(s.preedit(), "英語");
    assert!(s.remaining_ms() > before, "the window was not extended");
    assert_eq!(s.guide(), "", "no guide while registering");
}

#[test]
fn ctrl_g_with_romaji_keeps_commit_and_forward_and_closes_the_window() {
    // D-231 / D-03: Ctrl-G with romaji still commits the romaji and forwards the
    // key, and now also closes the window. Keys: Ctrl-J, Nihongo, Ctrl-G, Eigo,
    // Ctrl-J, Nihongo.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    assert_eq!(s.guide(), "[日本語]");
    assert!(s.ctrl(b'g'));
    assert_eq!(s.commits, ["Nihongo"]);
    assert_eq!(s.forwarded, [u32::from(b'G')]);
    assert_eq!(s.remaining_ms(), 0);
    s.typ("Eigo");
    assert_eq!(s.guide(), "", "stays off until the next trigger");
    assert!(s.trigger());
    s.typ("Nihongo");
    assert_eq!(s.guide(), "[日本語]", "back after the next trigger");
}

#[test]
fn ctrl_g_while_reselecting_cancels_and_closes_the_window() {
    // D-231 / D-09: Ctrl-G while reselecting cancels (back to the reading, nothing
    // committed) and closes the window.
    // Keys: Ctrl-J, Nihongo, Ctrl-J, Ctrl-J, Ctrl-G, Eigo.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    assert!(s.trigger());
    assert!(s.trigger());
    assert!(s.candidate_count() > 0);
    assert!(s.ctrl(b'g'));
    assert_eq!(s.candidate_count(), 0);
    assert_eq!(s.preedit(), "日本語");
    assert!(s.commits.is_empty());
    assert_eq!(s.remaining_ms(), 0);
    s.typ("Eigo");
    assert_eq!(s.guide(), "");
}

#[test]
fn ctrl_g_while_registering_cancels_the_step_and_closes_the_window() {
    // D-231 / D-174: Ctrl-G in word registration cancels the step and closes the
    // window. Keys: Ctrl-J, Nihongo, Ctrl-J, Ctrl-R, Ctrl-G, Eigo.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    assert!(s.trigger());
    assert!(s.ctrl(b'r'));
    assert!(s.is_registering());
    assert!(s.ctrl(b'g'));
    assert!(!s.is_registering());
    assert_eq!(s.preedit(), "日本語");
    assert_eq!(s.remaining_ms(), 0);
    s.typ("Eigo");
    assert_eq!(s.guide(), "");
}

#[test]
fn ctrl_g_with_nothing_passes_through_and_closes_the_window() {
    // D-231: Ctrl-G with no buffer is not consumed (nothing committed, nothing
    // forwarded by the library) and closes the window. Keys: Ctrl-J, Ctrl-G, Nihongo.
    let mut s = Session::new();
    assert!(!s.trigger());
    assert!(s.remaining_ms() > 0);
    assert!(!s.ctrl(b'g'));
    assert!(s.commits.is_empty());
    assert!(s.forwarded.is_empty());
    assert_eq!(s.remaining_ms(), 0);
    s.typ("Nihongo");
    assert_eq!(s.guide(), "");
}

#[test]
fn escape_and_q_while_reselecting_keep_the_window_open() {
    // D-231: Esc and `q` cancel reselection but do not stop the guide.
    // Keys: Ctrl-J, Nihongo, Ctrl-J, Ctrl-J, Esc, Eigo, Ctrl-J, Ctrl-J, q, Nihongo.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    assert!(s.trigger());
    assert!(s.trigger());
    assert!(s.press(KEY_ESCAPE, 0));
    assert_eq!(s.candidate_count(), 0);
    assert!(s.remaining_ms() > 0);
    s.typ("Eigo");
    assert_eq!(s.guide(), "[英語]");
    assert!(s.trigger());
    assert!(s.trigger());
    assert!(s.press(u32::from(b'q'), 0));
    assert_eq!(s.candidate_count(), 0);
    assert!(s.remaining_ms() > 0);
    s.typ("Nihongo");
    assert_eq!(s.guide(), "[日本語]");
}

#[test]
fn reset_closes_the_window_and_finalize_for_reset_alone_does_not() {
    // D-231 / D-14 / D-15 (research Pitfall 4): `finalize_for_reset` only commits
    // the pending conversion; the window is closed by `sekka_context_reset`, which
    // `SekkaState::reset` always calls last.
    // Keys: Ctrl-J, Nihongo, Ctrl-J, finalize, reset, Eigo.
    let mut s = Session::new();
    s.trigger();
    s.typ("Nihongo");
    assert!(s.trigger());
    unsafe { sekka_context_finalize_for_reset(s.ctx) };
    assert!(
        s.remaining_ms() > 0,
        "finalize_for_reset alone keeps it open"
    );
    s.reset(false);
    assert_eq!(s.remaining_ms(), 0);
    s.typ("Eigo");
    assert_eq!(s.guide(), "");

    // The IM switch / explicit reset order: finalize, drain, reset.
    let mut t = Session::new();
    t.trigger();
    t.typ("Nihongo");
    t.reset(true);
    assert_eq!(t.remaining_ms(), 0);
}

#[test]
fn guide_functions_are_null_safe() {
    // A NULL context is answered with NULL / 0, never a crash.
    assert!(unsafe { sekka_context_get_guide(std::ptr::null_mut()) }.is_null());
    assert_eq!(
        unsafe { sekka_context_guide_remaining_ms(std::ptr::null_mut()) },
        0
    );
}

#[test]
fn guide_through_the_c_abi_is_the_bracketed_first_candidate() {
    // GUIDE-01 / GUIDE-04 over the C ABI: only the romaji after a held `/` counts,
    // reading the guide changes nothing, and it is gone while the result is shown.
    // Keys: Ctrl-J, Linux, /, Nihongo, Ctrl-J.
    let mut s = Session::new();
    s.trigger();
    s.typ("Linux");
    assert!(s.press(KEY_SLASH, 0));
    assert_eq!(s.guide(), "", "a lone held slash has nothing to convert");
    s.typ("Nihongo");
    assert_eq!(s.guide(), "[日本語]");
    assert_eq!(s.preedit(), "/Nihongo", "reading the guide changes nothing");
    assert!(s.trigger());
    assert_eq!(s.preedit(), "日本語");
    assert_eq!(s.guide(), "");
}
