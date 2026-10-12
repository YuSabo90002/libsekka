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
#[allow(dead_code)]
const MOD_ALT: u32 = 0x8;

const KEY_SLASH: u32 = 0x2F;
const KEY_SPACE: u32 = 0x20;
#[allow(dead_code)]
const KEY_PLUS: u32 = 0x2B;
#[allow(dead_code)]
const KEY_RETURN: u32 = 0xFF0D;
#[allow(dead_code)]
const KEY_ESCAPE: u32 = 0xFF1B;
#[allow(dead_code)]
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
    #[allow(dead_code)]
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

    #[allow(dead_code)]
    fn candidate_count(&self) -> i32 {
        unsafe { sekka_context_get_candidate_count(self.ctx) }
    }

    /// Same order as `SekkaState::reset` (D-14 / D-15).
    #[allow(dead_code)]
    fn reset(&mut self, commit_pending: bool) {
        if commit_pending {
            unsafe { sekka_context_finalize_for_reset(self.ctx) };
            self.drain_output();
        }
        unsafe { sekka_context_reset(self.ctx) };
    }

    #[allow(dead_code)]
    fn is_registering(&self) -> bool {
        (unsafe { sekka_context_is_registering(self.ctx) }) == 1
    }

    #[allow(dead_code)]
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
