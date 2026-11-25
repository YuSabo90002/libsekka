// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Integration test for the uppercase rescue (E6): a word chosen from the
//! rescued input `KAnji` is learned under the same reading as `Kanji`.
//!
//! Only the C ABI (`sekka::capi`) is used, with a master dictionary written by
//! `dict_format::write_dict` and a user dictionary in a temporary directory, the
//! same way fcitx5-sekka wires them.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::ptr;

use sekka::capi::*;
use sekka::dictionary::dict_format;
use sekka::dictionary::DictEntry;

struct Session {
    ctx: *mut SekkaContextFfi,
    master: *mut SekkaDictionaryFfi,
    user: *mut SekkaDictionaryFfi,
    // Dropped after `Drop::drop` ran, i.e. after the dictionaries are freed.
    _dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        let mut map: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
        map.insert(
            "かんじ".to_string(),
            vec![DictEntry::new("漢字"), DictEntry::new("幹事")],
        );
        let master_path = dir.path().join("master.dict");
        dict_format::write_dict(&master_path, &map).expect("write the dictionary");
        let user_path = dir.path().join("user.db");
        let master_c = CString::new(master_path.to_str().expect("utf-8 path")).expect("no NUL");
        let user_c = CString::new(user_path.to_str().expect("utf-8 path")).expect("no NUL");
        let master = unsafe { sekka_file_dict_new(master_c.as_ptr(), ptr::null()) };
        assert!(!master.is_null(), "open the master dictionary");
        let user = unsafe { sekka_user_dict_new(user_c.as_ptr(), ptr::null()) };
        assert!(!user.is_null(), "open the user dictionary");
        let ctx = sekka_context_new();
        assert!(!ctx.is_null(), "create the context");
        let mut dicts = [master, user];
        unsafe { sekka_context_set_dictionaries(ctx, dicts.as_mut_ptr(), 2) };
        Session {
            ctx,
            master,
            user,
            _dir: dir,
        }
    }

    fn typ(&mut self, text: &str) {
        for ch in text.chars() {
            let consumed =
                unsafe { sekka_context_process_key_event(self.ctx, u32::from(ch), 0, 0) };
            assert_eq!(consumed, 1, "key {:?} is consumed", ch);
        }
    }

    fn trigger(&mut self) {
        assert_eq!(unsafe { sekka_context_trigger(self.ctx) }, 1, "Ctrl-J");
    }

    fn candidates(&self) -> Vec<String> {
        let count = unsafe { sekka_context_get_candidate_count(self.ctx) };
        if count <= 0 {
            return Vec::new();
        }
        let mut buf: Vec<*mut std::os::raw::c_char> = vec![ptr::null_mut(); count as usize];
        let got = unsafe { sekka_context_get_candidates(self.ctx, buf.as_mut_ptr(), count, 0) };
        let list = buf[..got.max(0) as usize]
            .iter()
            .map(|p| unsafe { CStr::from_ptr(*p) }.to_string_lossy().into_owned())
            .collect();
        unsafe { sekka_free_candidate_list(buf.as_mut_ptr(), got) };
        list
    }

    fn preedit(&self) -> String {
        let p = unsafe { sekka_context_get_preedit(self.ctx) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { sekka_free_string(p) };
        s
    }

    /// Cancels whatever is staged without committing it.
    fn reset(&mut self) {
        unsafe { sekka_context_reset(self.ctx) };
        assert!(self.drain().is_empty(), "reset commits nothing");
    }

    fn drain(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let p = unsafe { sekka_context_poll_output(self.ctx) };
            if p.is_null() {
                break;
            }
            out.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
            unsafe { sekka_free_string(p) };
        }
        out
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            sekka_context_free(self.ctx);
            sekka_free_dictionary(self.master);
            sekka_free_dictionary(self.user);
        }
    }
}

#[test]
fn rescued_input_shares_learning_with_the_lowered_input() {
    let mut s = Session::new();

    // Before any learning, `Kanji` puts the dictionary's first word first.
    s.typ("Kanji");
    s.trigger();
    assert_eq!(s.preedit(), "漢字");
    s.trigger();
    s.reset();

    // `KAnji`: enter reselection and choose the word that is not first.
    s.typ("KAnji");
    s.trigger();
    s.trigger();
    let candidates = s.candidates();
    let index = candidates
        .iter()
        .position(|c| c == "幹事")
        .unwrap_or_else(|| panic!("幹事 is a candidate of KAnji: {:?}", candidates));
    assert_ne!(
        index, 0,
        "幹事 is not the first candidate yet: {:?}",
        candidates
    );
    unsafe {
        sekka_context_select_candidate(s.ctx, index as i32);
        sekka_context_confirm_candidate(s.ctx);
    }
    assert_eq!(s.drain(), vec!["幹事".to_string()]);

    // `Kanji` now puts the word learned through `KAnji` first.
    s.typ("Kanji");
    s.trigger();
    assert_eq!(s.preedit(), "幹事");
    s.trigger();
    assert_eq!(s.candidates().first().map(String::as_str), Some("幹事"));
}
