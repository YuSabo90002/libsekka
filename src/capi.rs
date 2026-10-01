// SPDX-FileCopyrightText: 2026 yuta <yusabo90002@gmail.com>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! C ABI bindings
//!
//! Exposes libsekka's functionality as a C ABI. `unsafe` blocks are confined to
//! this module.
//!
//! Every FFI function catches panics with `catch_unwind` so that no panic
//! propagates across the FFI boundary. A NULL pointer is handled safely as a
//! no-op.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::catch_unwind;
use std::ptr;
use std::slice;
use std::sync::Arc;

use crate::context::{ConversionState, SekkaContext};
use crate::dictionary::immutable_dict::ImmutableFileDict;
use crate::dictionary::user_dict::UserDict;
use crate::dictionary::{DictError, Dictionary};

/// Helper that recognizes a Ctrl + letter key
///
/// fcitx5 normalizes Ctrl + letter to the uppercase keysym, so both the lowercase
/// and uppercase keysyms must be accepted (D-09, RESEARCH Pitfall 4).
fn ctrl_letter(keysym: u32, lower: u8) -> bool {
    let upper = lower.to_ascii_uppercase();
    keysym == lower as u32 || keysym == upper as u32
}

/// Ctrl modifier bit (the same value as fcitx5's `KeyState::Ctrl`)
const MOD_CTRL: u32 = 0x4;
/// Alt modifier bit (the same value as fcitx5's `KeyState::Alt`)
const MOD_ALT: u32 = 0x8;
/// Super modifier bit (the same value as fcitx5's `KeyState::Super`)
const MOD_SUPER: u32 = 0x40;

/// Decides whether a key press is a lone modifier key (D-03)
///
/// Covers Shift_L to Hyper_R (0xFFE1-0xFFEE), ISO_Lock to ISO_Level5_Lock
/// (0xFE01-0xFE13, including ISO_Level3_Shift 0xFE03), Mode_switch (0xFF7E) and
/// Num_Lock (0xFF7F). These change no state, no buffer and no last commit, and
/// count as unconsumed.
fn is_modifier_keysym(keysym: u32) -> bool {
    (0xFFE1..=0xFFEE).contains(&keysym)
        || (0xFE01..=0xFE13).contains(&keysym)
        || keysym == 0xFF7E
        || keysym == 0xFF7F
}

/// Decides whether a key is a character key to accumulate in the romaji buffer
/// (D-03/D-20/D-21)
///
/// Romaji characters (D-20/D-21) are ASCII `a`-`z`, `A`-`Z`, `0`-`9`, the long
/// vowel mark `-` (which becomes `ー` by the rules in romaji.rs), plus the symbols
/// `.` `,` `@` `:` `!` `[` `]` `?` `;` `'` - that is, upstream `sekka-skip-chars`
/// (`emacs/sekka.el:122`) minus `=`, `` ` `` and `+`. Upstream buffers those three
/// during input and only interprets them at conversion time, as a phrase-search
/// marker (`=`) or okurigana markers (`` ` ``/`+`). Sekka does not accept them as
/// romaji characters: buffering them would leave Ctrl-J's behavior undefined, and
/// would not rescue other printable symbols such as `(` either (rejected for
/// D-158). Since D-158 (Phase 9), printable keys outside `is_romaji_char`
/// (including whitespace) are instead appended to the commit string by
/// `dispatch_input`. Digits are accepted together with upstream's number
/// conversion branch (D-21, `InputShape::NumberOnly` / `NumberPrefixed`). Every
/// other symbol and whitespace is a non-character key.
fn is_romaji_char(keysym: u32) -> bool {
    matches!(
        keysym,
        0x30..=0x39 // 0-9
            | 0x41..=0x5A
            | 0x61..=0x7A
            | 0x2E // .
            | 0x2C // ,
            | 0x40 // @
            | 0x3A // :
            | 0x21 // !
            | 0x5B // [
            | 0x5D // ]
            | 0x3F // ?
            | 0x3B // ;
            | 0x27 // '
    ) || keysym == b'-' as u32
}

/// Decides whether a keysym is a printable ASCII character (D-158)
///
/// For printable ASCII, the X11 keysym matches the code point (the same
/// assumption `is_romaji_char` makes for `keysym as u8 as char`). This range
/// (0x20..=0x7E) includes space and every symbol not in `is_romaji_char`
/// (`+`, `` ` ``, `=` and so on). BackSpace (0xFF08), Enter (0xFF0D), Tab
/// (0xFF09), Escape (0xFF1B) and the arrow keys (0xFF51..=0xFF54) are outside
/// this range, so they keep the commit-and-forward behavior of D-03 (D-162).
/// Non-ASCII printable keysyms (e.g. Latin-1 Supplement) are out of scope for
/// D-158 (RESEARCH Open Question 1).
///
/// D-185 (Phase 11): the same range decides which Alt-held keys are committed as
/// text with Alt removed, so Latin-1 and the like stay out of range there too
/// (D-187 keeps them on the D-03/D-162 path).
fn is_printable_ascii(keysym: u32) -> bool {
    (0x20..=0x7E).contains(&keysym)
}

/// Key dispatch during reselection (the Selecting state) (D-09)
///
/// Returns false without deciding anything when Alt or Super is set (treated as an
/// other key). Every explicit key in the table of D-09 is decided first and only
/// then does a non-match return false (RESEARCH Pitfall 5: including where `q` is
/// decided, the structure decides the whole key table before falling through to
/// "other key"). The "other key" handling for a false return (confirm, then feed
/// the same key to dispatch_input) is done by the caller
/// (`sekka_context_process_key_event`). Since D-161 (Phase 9), BackSpace has its
/// own entry in this table (closes the candidate window and reverts to the
/// original romaji, D-160) instead of falling through to "other key" like it used
/// to. Since D-167 (Phase 10), Ctrl-R also has its own entry: it enters word
/// registration from inside the candidate window, exactly like from the commit
/// display state (`dispatch_input`'s own Ctrl-R branch); a shape refused by
/// `begin_registration` (D-169) is still consumed here and changes nothing else.
fn handle_selecting_key(ctx: &mut SekkaContext, keysym: u32, modifiers: u32) -> bool {
    let ctrl = (modifiers & MOD_CTRL) != 0;
    let other = (modifiers & (MOD_ALT | MOD_SUPER)) != 0;
    if other {
        return false;
    }

    if !ctrl && keysym == 0xFF08 {
        // D-161: closes the candidate window and reverts to the original romaji
        // (minus its last character), matching D-160 in the commit display state.
        // Nothing is committed and nothing is forwarded. `last_commit` is always
        // Some while Selecting (`begin_reselect` never enters without one), so
        // this is always consumed.
        let consumed = ctx.backspace();
        debug_assert!(
            consumed,
            "BackSpace during reselection must always be consumed (last_commit is Some)"
        );
        return true;
    }

    if ctrl {
        if ctrl_letter(keysym, b'j') || ctrl_letter(keysym, b'n') {
            ctx.next_candidate();
            return true;
        }
        if ctrl_letter(keysym, b'p') {
            ctx.prev_candidate();
            return true;
        }
        if ctrl_letter(keysym, b'm') {
            ctx.confirm();
            return true;
        }
        if ctrl_letter(keysym, b'g') {
            ctx.cancel();
            return true;
        }
        if ctrl_letter(keysym, b'a') {
            ctx.select_kanji();
            return true;
        }
        if ctrl_letter(keysym, b'u') {
            ctx.select_hiragana();
            return true;
        }
        if ctrl_letter(keysym, b'i') || ctrl_letter(keysym, b'k') {
            ctx.select_katakana();
            return true;
        }
        if ctrl_letter(keysym, b'l') {
            ctx.select_hankaku();
            return true;
        }
        if ctrl_letter(keysym, b'e') {
            ctx.select_zenkaku();
            return true;
        }
        if ctrl_letter(keysym, b'r') {
            // D-167: Ctrl-R also enters registration from the reselection
            // window; a refused shape (D-169) is consumed and changes nothing.
            ctx.begin_registration();
            return true;
        }
        return false;
    }

    if keysym == 0x20 {
        ctx.next_candidate();
        return true;
    }
    if keysym == 0xFF0D || keysym == 0xFF8D {
        ctx.confirm();
        return true;
    }
    if keysym == 0xFF1B || keysym == b'q' as u32 {
        ctx.cancel();
        return true;
    }

    false
}

/// Mirrors the key table `handle_selecting_key` consumes (D-177)
///
/// A copy of the D-09 table above (including D-161's BackSpace and D-167's
/// Ctrl-R) as a pure predicate with no `SekkaContext` side effects, so
/// `would_forward_outside_registration` can ask "is this one of the keys the
/// candidate window itself handles" without calling into the context. Kept
/// in step with `handle_selecting_key` by the exhaustive agreement test
/// `selecting_table_predicate_agrees_with_handle_selecting_key`.
fn is_selecting_table_key(keysym: u32, modifiers: u32) -> bool {
    let ctrl = (modifiers & MOD_CTRL) != 0;
    let other = (modifiers & (MOD_ALT | MOD_SUPER)) != 0;
    if other {
        return false;
    }
    if ctrl {
        return ctrl_letter(keysym, b'j')
            || ctrl_letter(keysym, b'n')
            || ctrl_letter(keysym, b'p')
            || ctrl_letter(keysym, b'm')
            || ctrl_letter(keysym, b'g')
            || ctrl_letter(keysym, b'a')
            || ctrl_letter(keysym, b'u')
            || ctrl_letter(keysym, b'i')
            || ctrl_letter(keysym, b'k')
            || ctrl_letter(keysym, b'l')
            || ctrl_letter(keysym, b'e')
            || ctrl_letter(keysym, b'r');
    }
    matches!(keysym, 0xFF08 | 0x20 | 0xFF0D | 0xFF8D | 0xFF1B) || keysym == b'q' as u32
}

/// Key dispatch during input (the Input state) (D-03/D-12/D-13; D-160 replaces
/// D-13's BackSpace rule, Phase 9)
///
/// The trigger (start conversion / start reselection / next candidate) is split
/// out into `sekka_context_trigger()` (D-110) and this function no longer handles
/// it. Ctrl-J used to be hardcoded here and delegated to
/// `process_key('\0', true)`, but since D-108 (the C++ side decides the trigger
/// key with `Key::checkKeyList`, routes matching keys to
/// `sekka_context_trigger()` and everything else to this function), "is this the
/// trigger key" is decided solely by the C++-side configuration
/// (`SekkaConfig::triggerKey`). By the time a key reaches this function it is
/// certain not to be the trigger (even the default Ctrl-J never arrives here,
/// because a match on the C++ side routes it to `sekka_context_trigger()`).
/// Therefore **even a literal Ctrl-J** gets no special treatment and falls into
/// the "anything else" branch below (commit the romaji as it is and forward,
/// D-03), like any other Ctrl + letter key. This is what makes conversion stop
/// happening on the old Ctrl-J right after the TriggerKey is changed (pinned by
/// the E2E tests of D-106/D-108/D-111).
///
/// Everything below is non-trigger key handling. D-13 used to say: BackSpace is
/// forwarded as it is when the D-12 flush produced output, and edits the preedit
/// otherwise. D-160 (Phase 9) replaces this: BackSpace is judged *before* the
/// D-12 flush (RESEARCH Pattern 2 — judging it after would commit the staged
/// candidate first) and, in the commit display state, reverts to the original
/// romaji instead of committing (see `SekkaContext::backspace`). For every other
/// key, `flush_last_commit` commits any unsent staged candidate exactly once
/// (D-12: the single rule that "every key other than the trigger and BackSpace
/// commits and forwards that key to the application"); romaji character keys
/// accumulate in the buffer; any other non-character key either commits the
/// romaji as it is or forwards the flush result. Since D-158 (Phase 9), a
/// printable ASCII key without Ctrl/Alt/Super that is not a romaji character
/// (space and every symbol not accepted by `is_romaji_char`) is instead appended
/// to whatever this key event commits and is not forwarded (D-162 keeps the
/// Ctrl/Alt/Super and non-printable case as-is).
///
/// D-167 (Phase 10): Ctrl-R in the commit display state (a staged candidate is
/// present) enters word registration instead. It is judged first, before even
/// the BackSpace check, and in particular before the single D-12 flush point
/// below - flushing first would commit `last_commit` and lose the typed
/// reading it carries (the same reason BackSpace is judged early, D-160). A
/// shape refused by `begin_registration` (D-169, a later plan) is still
/// consumed here and changes nothing else. Outside the commit display state
/// (nothing staged, or still typing romaji) Ctrl-R falls through unchanged and
/// keeps D-03's plain Ctrl+letter handling.
///
/// D-185 (Phase 11): a printable ASCII key held with Alt (but neither Ctrl nor
/// Super) is committed as that character with Alt removed, appended to whatever
/// this key event commits (the romaji buffer, or the word the D-12 flush above
/// produced) or alone when there is nothing, in a single CommitString. Unlike
/// D-158 it is consumed even when empty, and it is never forwarded (D-155). D-186:
/// Shift is already reflected in the keysym, so Alt+Shift+d arrives as `D`.
/// D-187/D-188: an Alt key that is not a printable character, and any key with
/// Ctrl or Super (with or without Alt), stay on the D-03/D-162 commit-and-forward
/// path.
fn dispatch_input(
    ctx: &mut SekkaContext,
    forward_key: &mut bool,
    keysym: u32,
    modifiers: u32,
) -> c_int {
    let ctrl = (modifiers & MOD_CTRL) != 0;
    let other = (modifiers & (MOD_ALT | MOD_SUPER)) != 0;
    let alt_only = (modifiers & MOD_ALT) != 0 && !ctrl && (modifiers & MOD_SUPER) == 0;

    if ctrl && !other && ctrl_letter(keysym, b'r') && ctx.has_staged_candidate() {
        ctx.begin_registration();
        return 1;
    }

    // D-160: BackSpace is judged before the single D-12 flush point below
    // (placing it after would commit the staged candidate first — RESEARCH
    // Pattern 2). SekkaContext::backspace reverts a staged candidate to its
    // original romaji instead of committing it; it commits nothing and forwards
    // nothing. Once the romaji buffer is exhausted, a further BackSpace is
    // unconsumed and reaches the application untouched, as an ordinary BackSpace.
    if !ctrl && !other && keysym == 0xFF08 {
        return if ctx.backspace() { 1 } else { 0 };
    }

    // The single flush point of D-12, for every key other than BackSpace: commit
    // any unsent staged candidate exactly once, here (following the Anti-Patterns
    // entry "flushing in several places" in RESEARCH.md, this happens once per
    // key event and only here).
    let flushed = ctx.flush_last_commit();

    if !ctrl && !other && is_romaji_char(keysym) {
        ctx.process_key(keysym as u8 as char, false);
        return 1;
    }

    // D-158: a printable ASCII key that is not `is_romaji_char` (space, `+`, `(`,
    // `=`, `` ` `` and so on) is appended to whatever this key event commits (the
    // romaji buffer, or the commit-display word the D-12 flush above already
    // produced) and goes out in a single CommitString; it is not forwarded. When
    // there is nothing to commit it stays unconsumed and reaches the application
    // untouched.
    if !ctrl && !other && is_printable_ascii(keysym) {
        return if ctx.commit_with_trailing_char(keysym as u8 as char) {
            1
        } else {
            0
        };
    }

    // D-185/D-186 (Phase 11): a printable ASCII key (the same 0x20..=0x7E range as
    // D-158) held with Alt but neither Ctrl nor Super is committed as text with Alt
    // removed, the only way to strip a modifier under fcitx5's Wayland frontend
    // (D-184: a forwarded key carries the physically held modifiers). Shift is
    // already applied to the keysym (D-186); fcitx5-sekka passes the unnormalized
    // keysym for these keys. It is committed even when there is nothing else to
    // commit, and the forward flag is left alone so the committed character and the
    // key are never both delivered (D-155). D-187/D-188 keep a non-printable key
    // with Alt, and any key with Ctrl or Super, on the D-03/D-162 path below.
    if alt_only && is_printable_ascii(keysym) {
        ctx.commit_alt_passthrough_char(keysym as u8 as char);
        return 1;
    }

    // Anything else (non-character keys, including keys with Ctrl/Alt/Super).
    // Since Phase 11 (D-185), what reaches here is only a non-character key (with
    // or without Alt, D-187) or a key with Ctrl or Super (whether or not Alt is
    // also held, D-188); an Alt-only printable key was committed above.
    // Earlier description: commit the romaji as it is and forward the key too (D-03). Since D-158
    // (Phase 9), the only keys that reach this branch are non-printable keys and
    // Ctrl/Super-modified keys, plus Alt-modified non-printable keys (D-166:
    // history kept, not erased; D-185 moved Alt-only printable keys above).
    if ctx.commit_raw_romaji() {
        *forward_key = true;
        return 1;
    }
    // The invariant (a true `flushed` means the romaji buffer is empty) makes it
    // impossible for commit_raw_romaji above and this branch to both apply.
    if flushed {
        *forward_key = true;
        return 1;
    }
    0
}

/// Dispatches a key event to the input context that should receive it
/// (D-171/ARCHITECTURE case C, Phase 10)
///
/// Selects between `handle_selecting_key` and `dispatch_input` by
/// `ctx.state()`, and on a `handle_selecting_key` miss, confirms and
/// reprocesses through `dispatch_input` exactly as
/// `sekka_context_process_key_event` used to do inline. This is the whole
/// non-trigger key-routing decision, factored out so `handle_registration_key`
/// can send a key to the innermost registration step's context the same way
/// the top-level FFI entry point sends it to the top-level context.
fn route_key(ctx: &mut SekkaContext, forward_key: &mut bool, keysym: u32, modifiers: u32) -> c_int {
    if ctx.state() == ConversionState::Selecting {
        if handle_selecting_key(ctx, keysym, modifiers) {
            return 1;
        }
        // Other key (D-09): confirm with the selected candidate, then
        // reprocess the same key as the Input state (with an empty buffer). A
        // character key starts new romaji input; a printable key is appended
        // to the confirmed candidate (D-158); a non-printable key is
        // forwarded (D-03/D-162). An Alt-only printable key is never matched
        // against D-09's table, so it too lands here: the confirmed candidate gets
        // the character appended without Alt (D-185).
        ctx.confirm();
        let result = dispatch_input(ctx, forward_key, keysym, modifiers);
        if result == 0 {
            *forward_key = true;
            return 1;
        }
        return result;
    }

    dispatch_input(ctx, forward_key, keysym, modifiers)
}

/// Whether, outside registration, this key would reach `dispatch_input`'s
/// "Anything else" branch and be committed and forwarded to the application
/// in the active step's current state (D-177)
///
/// Covers Tab, the arrow keys, Home, End, Delete, the F keys, Ctrl + a
/// letter not in the D-09 table, Ctrl-R with nothing staged, and every
/// Alt/Super-modified key - the keys that, outside registration, `dispatch_input`
/// or `handle_selecting_key`'s own miss-and-fall-through would ultimately
/// commit and forward. `false` for every key `handle_registration_key`
/// should still route to the inner step: BackSpace and printable ASCII
/// (handled by their own dedicated branches below, D-158/D-175), the D-09
/// table while `active` is in the Selecting state (so the candidate window
/// keeps working, D-178), and Ctrl-R while a candidate is staged (D-171's
/// recursive entry). Alt/Super always forward (D-177: "Alt＋キーも、登録中は
/// 何もしない"), checked first so it takes priority even inside the
/// candidate window.
fn would_forward_outside_registration(active: &SekkaContext, keysym: u32, modifiers: u32) -> bool {
    let ctrl = (modifiers & MOD_CTRL) != 0;
    let other = (modifiers & (MOD_ALT | MOD_SUPER)) != 0;
    if other {
        return true;
    }
    if active.state() == ConversionState::Selecting && is_selecting_table_key(keysym, modifiers) {
        return false;
    }
    if ctrl {
        return !(ctrl_letter(keysym, b'r') && active.has_staged_candidate());
    }
    if keysym == 0xFF08 || is_printable_ascii(keysym) {
        return false;
    }
    true
}

/// Dispatches a key event while registering (D-172/D-173/D-174/D-175/D-176/
/// D-177/D-178/REG-02, Phase 10)
///
/// The final form of the registration-mode key table (D-178), evaluated in
/// this order - each branch calls `ctx.absorb_registration_output()` before
/// returning, pulling any newly committed inner output up into `draft`
/// (REG-02) even on an early return, so the caller never has to remember to
/// call it separately:
/// 1. Enter/KP_Enter with no modifier finishes the current registration step
///    (`finish_registration`, D-172/D-176).
/// 2. An unmodified Esc or Ctrl-G cancels the step (D-174) - unless the
///    innermost step's own candidate window is open, in which case they only
///    close that window (routed through `route_key` so `handle_selecting_key`'s
///    own Esc/Ctrl-G/q entries handle it, D-09), leaving the registration
///    session itself in place. `q` needs no dedicated branch here: inside the
///    candidate window `route_key` already closes it, and outside it `q` is
///    simply ordinary romaji input.
///  3. BackSpace: routed to the inner step first via `route_key` exactly like
///    an ordinary key (closes the candidate window and/or reverts to the
///    original romaji, D-160/D-161, when there is something for it to act
///    on); when that leaves it unconsumed (the innermost step had nothing to
///    delete), the last character of the word being assembled is deleted
///    instead (`pop_registration_draft`, D-175) - and when the word is empty
///    too, this is simply a no-op (the reading is never touched and nothing
///    is forwarded).
/// 4. A key that `would_forward_outside_registration` says would reach the
///    application outside registration is consumed and changes nothing at
///    all (D-177) - not even routed to the inner step, so the candidate that
///    may be staged there (commit display or the candidate window) is left
///    exactly as it was.
/// 5. Everything else is routed to the inner step's context via `route_key`,
///    exactly like an ordinary key event. When that leaves it unconsumed and
///    the key is a printable ASCII character with no modifier, it is
///    appended to the word instead (`push_registration_draft`, D-177/D-158) -
///    even when the innermost step's own buffer is empty, matching what
///    `commit_with_trailing_char` does outside registration.
///
/// In branches 3 and 5, `route_key`'s own forward-key output parameter is
/// always discarded after asserting it stayed `false`: nothing typed while
/// registering is ever forwarded to the application (REG-02), and branch 4
/// already stops every key that would have set it before `route_key` is ever
/// called, so it can never actually become `true` here.
fn handle_registration_key(ctx: &mut SekkaContext, keysym: u32, modifiers: u32) {
    let ctrl = (modifiers & MOD_CTRL) != 0;
    let other = (modifiers & (MOD_ALT | MOD_SUPER)) != 0;

    // 1. Enter / KP_Enter (D-172/D-176).
    if !ctrl && !other && (keysym == 0xFF0D || keysym == 0xFF8D) {
        ctx.finish_registration();
        ctx.absorb_registration_output();
        return;
    }

    // 2. Esc / Ctrl-G (D-174/D-179).
    if (!ctrl && !other && keysym == 0xFF1B) || (ctrl && !other && ctrl_letter(keysym, b'g')) {
        if ctx.active().state() == ConversionState::Selecting {
            let mut discarded_forward = false;
            route_key(ctx.active_mut(), &mut discarded_forward, keysym, modifiers);
        } else {
            ctx.cancel_registration();
        }
        ctx.absorb_registration_output();
        return;
    }

    // 3. BackSpace (D-175).
    if !ctrl && !other && keysym == 0xFF08 {
        let mut discarded_forward = false;
        let consumed = route_key(ctx.active_mut(), &mut discarded_forward, keysym, modifiers);
        debug_assert!(!discarded_forward);
        if consumed == 0 {
            ctx.pop_registration_draft();
        }
        ctx.absorb_registration_output();
        return;
    }

    // 4. Keys that would reach the application outside registration (D-177):
    // consumed without touching the inner step at all.
    if would_forward_outside_registration(ctx.active(), keysym, modifiers) {
        ctx.absorb_registration_output();
        return;
    }

    // 5. Everything else -> the inner step; a printable-ASCII miss appends
    // to the word instead (D-177/D-158).
    let mut discarded_forward = false;
    let consumed = route_key(ctx.active_mut(), &mut discarded_forward, keysym, modifiers);
    debug_assert!(!discarded_forward);
    if consumed == 0 && !ctrl && !other && is_printable_ascii(keysym) {
        ctx.push_registration_draft(keysym as u8 as char);
    }
    ctx.absorb_registration_output();
}

// ---------------------------------------------------------------------------
// Opaque pointer types
// ---------------------------------------------------------------------------

/// Input context for the FFI (an opaque pointer)
///
/// The C side only ever handles it as a pointer.
/// It holds a `SekkaContext` internally.
pub struct SekkaContextFfi {
    ctx: SekkaContext,
    /// Whether the raw key of the previous key event should be forwarded to the
    /// application (D-03). Taking it with `sekka_context_take_forward_key` resets
    /// it to false.
    forward_key: bool,
}

/// Dictionary handle for the FFI (an opaque pointer)
///
/// The C side only ever handles it as a pointer.
/// It holds an `Arc<dyn Dictionary>` internally and can be shared by several contexts.
pub struct SekkaDictionaryFfi {
    dict: Arc<dyn Dictionary>,
}

// ---------------------------------------------------------------------------
// Version string
// ---------------------------------------------------------------------------

/// The library version string (NUL-terminated)
static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

// ---------------------------------------------------------------------------
// T026a: context management
// ---------------------------------------------------------------------------

/// Creates a new input context
///
/// The caller is responsible for freeing it with `sekka_context_free` after use.
/// Returns NULL when creation fails (on panic).
#[no_mangle]
pub extern "C" fn sekka_context_new() -> *mut SekkaContextFfi {
    catch_unwind(|| {
        let ffi = Box::new(SekkaContextFfi {
            ctx: SekkaContext::new(),
            forward_key: false,
        });
        Box::into_raw(ffi)
    })
    .unwrap_or(ptr::null_mut())
}

/// Frees an input context
///
/// Passing a NULL pointer does nothing.
/// It must not be called more than once for the same pointer (undefined behaviour).
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
/// After the call, `ctx` is invalid.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_free(ctx: *mut SekkaContextFfi) {
    if ctx.is_null() {
        return;
    }
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(ctx));
    }));
}

/// Resets the state of an input context
///
/// Clears the preedit, the output queue and the romaji buffer. The dictionary list
/// is not reset.
/// Passing a NULL pointer does nothing.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_reset(ctx: *mut SekkaContextFfi) {
    if ctx.is_null() {
        return;
    }
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        ctx.ctx.reset();
    }));
}

/// Frees a string allocated for the C side
///
/// Used to free string pointers returned by `sekka_context_get_preedit`,
/// `sekka_context_poll_output` and friends.
/// Passing a NULL pointer does nothing.
///
/// # Safety
/// `str_ptr` must be NULL or an unfreed string returned by this library.
#[no_mangle]
pub unsafe extern "C" fn sekka_free_string(str_ptr: *mut c_char) {
    if str_ptr.is_null() {
        return;
    }
    let _ = catch_unwind(|| unsafe {
        drop(CString::from_raw(str_ptr));
    });
}

/// Returns the library version string
///
/// The returned pointer refers to static storage and must not be freed.
#[no_mangle]
pub extern "C" fn sekka_get_version() -> *const c_char {
    VERSION.as_ptr() as *const c_char
}

// ---------------------------------------------------------------------------
// T026b: key events and preedit
// ---------------------------------------------------------------------------

/// Processes a key event
///
/// # Arguments
/// * `ctx` - pointer to the input context
/// * `keysym` - the X11 key symbol value
///   For a key held with Alt but neither Ctrl nor Super, the caller (fcitx5-sekka)
///   passes the unnormalized keysym (`KeyEvent::rawKey()`), so `Alt+d` arrives as `d`
///   and `Alt+Shift+d` as `D` (D-185/D-186). Every other key is normalized, and a
///   letter held with Ctrl arrives uppercase (`ctrl_letter`).
/// * `modifiers` - the modifier key bitmask
/// * `is_release` - non-zero for a key release
///
/// # Returns
/// Returns 1 when the event was consumed and 0 when it was ignored.
/// Returns 0 for a NULL pointer or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_process_key_event(
    ctx: *mut SekkaContextFfi,
    keysym: u32,
    modifiers: u32,
    is_release: c_int,
) -> c_int {
    if ctx.is_null() {
        return 0;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        let release = is_release != 0;
        // Ignore key releases.
        if release {
            return 0;
        }

        // Initialize the forward flag for this event (never carry over the previous one).
        ctx.forward_key = false;

        // A lone modifier key press counts as unconsumed and changes no state, no
        // buffer and no last commit (so that Shift + a letter, or Ctrl then J, work).
        if is_modifier_keysym(keysym) {
            return 0;
        }

        // Phase 10: while registering, every key is consumed by the
        // registration mode itself (REG-02 - nothing is ever forwarded to the
        // application from here). `handle_registration_key` routes to the
        // innermost step's context and calls `absorb_registration_output`
        // itself before returning (D-178), pulling whatever that step just
        // committed up into its parent's draft.
        if ctx.ctx.is_registering() {
            handle_registration_key(&mut ctx.ctx, keysym, modifiers);
            return 1;
        }

        route_key(&mut ctx.ctx, &mut ctx.forward_key, keysym, modifiers)
    }))
    .unwrap_or(0)
}

/// Takes whether the raw key of the previous key event should be forwarded to the
/// application (D-03)
///
/// After emitting the committed output and updating the preedit, the caller
/// (fcitx5-sekka) forwards the raw key of that key event only when this returns 1.
/// Taking the value resets it to 0. Returns 0 for a NULL pointer or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_take_forward_key(ctx: *mut SekkaContextFfi) -> c_int {
    if ctx.is_null() {
        return 0;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        if ctx.forward_key {
            ctx.forward_key = false;
            1
        } else {
            0
        }
    }))
    .unwrap_or(0)
}

/// Gets the preedit string
///
/// While registering (Phase 10), returns `registration_word()` instead - the
/// word being assembled in the popup (D-170). The application's own preedit
/// during registration is `sekka_context_get_registration_reading` instead.
///
/// The caller is responsible for freeing the returned pointer with `sekka_free_string`.
/// Returns NULL for a NULL pointer or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_get_preedit(ctx: *mut SekkaContextFfi) -> *mut c_char {
    if ctx.is_null() {
        return ptr::null_mut();
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        let preedit = ctx
            .ctx
            .registration_word()
            .unwrap_or_else(|| ctx.ctx.get_preedit().to_string());
        match CString::new(preedit) {
            Ok(cstr) => cstr.into_raw(),
            Err(_) => ptr::null_mut(),
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Gets the cursor position within the preedit
///
/// Returns the cursor position in bytes. While registering, this is the byte
/// length of `registration_word()` (see `sekka_context_get_preedit`).
/// Returns 0 for a NULL pointer or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_get_preedit_cursor_pos(ctx: *mut SekkaContextFfi) -> c_int {
    if ctx.is_null() {
        return 0;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        match ctx.ctx.registration_word() {
            Some(word) => word.len() as c_int,
            None => ctx.ctx.get_preedit().len() as c_int,
        }
    }))
    .unwrap_or(0)
}

/// Polls for a committed output string
///
/// Takes one committed string out of the output queue. Returns NULL when the queue
/// is empty.
/// The caller is responsible for freeing the returned pointer with `sekka_free_string`.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_poll_output(ctx: *mut SekkaContextFfi) -> *mut c_char {
    if ctx.is_null() {
        return ptr::null_mut();
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        match ctx.ctx.poll_output() {
            Some(output) => match CString::new(output) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => ptr::null_mut(),
            },
            None => ptr::null_mut(),
        }
    }))
    .unwrap_or(ptr::null_mut())
}

// ---------------------------------------------------------------------------
// T026c: dictionary management
// ---------------------------------------------------------------------------

// --- Dictionary error codes (D-64) ---
//
// These hold the same numeric values as the hand-written enum in
// `fcitx5-sekka/src/sekka.h`. Changing only one side would still compile against
// the old signature, so the two must be synchronized by hand whenever a value
// changes (02.1-RESEARCH.md Pitfall 1).

/// Success
pub const SEKKA_DICT_OK: c_int = 0;
/// Invalid argument (a NULL path, a non-UTF-8 path, and so on)
pub const SEKKA_DICT_ERROR_INVALID_ARG: c_int = 1;
/// The dictionary file was not found (`DictError::NotFound`)
pub const SEKKA_DICT_ERROR_NOT_FOUND: c_int = 2;
/// The dictionary file cannot be opened (permissions and so on, `DictError::IoError`)
pub const SEKKA_DICT_ERROR_UNREADABLE: c_int = 3;
/// The dictionary file is corrupt (`DictError::CorruptFormat`)
pub const SEKKA_DICT_ERROR_CORRUPT: c_int = 4;
/// Any other error
pub const SEKKA_DICT_ERROR_OTHER: c_int = 5;

/// The single place where a `DictError` is mapped to an integer code for the C ABI.
///
/// This table is never scattered outside `capi.rs` (the C++ side only interprets numbers).
fn dict_error_code(e: &DictError) -> c_int {
    match e {
        DictError::NotFound(_) => SEKKA_DICT_ERROR_NOT_FOUND,
        DictError::IoError(_) => SEKKA_DICT_ERROR_UNREADABLE,
        DictError::CorruptFormat(_) => SEKKA_DICT_ERROR_CORRUPT,
        DictError::BackendError(_)
        | DictError::SerializationError(_)
        | DictError::ReadOnlyViolation => SEKKA_DICT_ERROR_OTHER,
    }
}

/// Opens a file dictionary (read-only) and reports the error kind on failure
///
/// Opens the immutable-format (`ImmutableFileDict`) master dictionary at the given path.
/// Writes the three kinds of dictionary load failure (missing / unopenable /
/// corrupt) into `out_error` (D-64, the detection side of SC6).
///
/// # Arguments
/// * `path` - path of the dictionary file (a NUL-terminated UTF-8 string)
/// * `encoding` - encoding name (currently ignored; reserved for future use)
/// * `out_error` - where to write the error kind; nothing is written when NULL
///
/// # Returns
/// A dictionary handle, or NULL on failure.
/// The caller is responsible for freeing it with `sekka_free_dictionary`.
///
/// # Safety
/// `path` must be NULL or point to a valid NUL-terminated string.
/// `out_error` must be NULL or point to a writable `c_int`.
#[no_mangle]
pub unsafe extern "C" fn sekka_file_dict_new_with_error(
    path: *const c_char,
    _encoding: *const c_char,
    out_error: *mut c_int,
) -> *mut SekkaDictionaryFfi {
    // Writing it outside the closure (outside catch_unwind) means every failure
    // path that returns early (the NULL check, non-UTF-8, a panic) writes it in
    // exactly one place.
    let write_error = |code: c_int| {
        if !out_error.is_null() {
            unsafe {
                *out_error = code;
            }
        }
    };
    if path.is_null() {
        write_error(SEKKA_DICT_ERROR_INVALID_ARG);
        return ptr::null_mut();
    }
    let result = catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path_str = unsafe { CStr::from_ptr(path) };
        let path_str = match path_str.to_str() {
            Ok(s) => s,
            Err(_) => return Err(SEKKA_DICT_ERROR_INVALID_ARG),
        };
        match ImmutableFileDict::open(path_str) {
            Ok(dict) => {
                let ffi = Box::new(SekkaDictionaryFfi {
                    dict: Arc::new(dict),
                });
                Ok(Box::into_raw(ffi))
            }
            Err(e) => Err(dict_error_code(&e)),
        }
    }));
    match result {
        Ok(Ok(ptr)) => {
            write_error(SEKKA_DICT_OK);
            ptr
        }
        Ok(Err(code)) => {
            write_error(code);
            ptr::null_mut()
        }
        Err(_) => {
            // Spell out `out_error` even when a panic drops us into the `unwrap_or` equivalent.
            write_error(SEKKA_DICT_ERROR_OTHER);
            ptr::null_mut()
        }
    }
}

/// Opens a file dictionary (read-only)
///
/// Opens the immutable-format (`ImmutableFileDict`) master dictionary at the given path.
/// Use `sekka_file_dict_new_with_error` when the error kind is needed.
///
/// # Arguments
/// * `path` - path of the dictionary file (a NUL-terminated UTF-8 string)
/// * `encoding` - encoding name (currently ignored; reserved for future use)
///
/// # Returns
/// A dictionary handle, or NULL on failure.
/// The caller is responsible for freeing it with `sekka_free_dictionary`.
///
/// # Safety
/// `path` must be NULL or point to a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn sekka_file_dict_new(
    path: *const c_char,
    encoding: *const c_char,
) -> *mut SekkaDictionaryFfi {
    unsafe { sekka_file_dict_new_with_error(path, encoding, ptr::null_mut()) }
}

/// Opens a user dictionary (read-write)
///
/// Opens the sled-backed user dictionary at the given path. The database is
/// created when the path does not exist.
///
/// # Arguments
/// * `path` - path of the dictionary file (a NUL-terminated UTF-8 string)
/// * `encoding` - encoding name (currently ignored; reserved for future use)
///
/// # Returns
/// A dictionary handle, or NULL on failure.
/// The caller is responsible for freeing it with `sekka_free_dictionary`.
///
/// # Safety
/// `path` must be NULL or point to a valid NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn sekka_user_dict_new(
    path: *const c_char,
    _encoding: *const c_char,
) -> *mut SekkaDictionaryFfi {
    if path.is_null() {
        return ptr::null_mut();
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path_str = unsafe { CStr::from_ptr(path) };
        let path_str = match path_str.to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        };
        match UserDict::open(path_str) {
            Ok(dict) => {
                let ffi = Box::new(SekkaDictionaryFfi {
                    dict: Arc::new(dict),
                });
                Box::into_raw(ffi)
            }
            Err(_) => ptr::null_mut(),
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Frees a dictionary handle
///
/// Passing a NULL pointer does nothing.
/// It must not be called more than once for the same pointer (undefined behaviour).
///
/// # Safety
/// `dict` must be NULL or an unfreed pointer returned by `sekka_file_dict_new` / `sekka_user_dict_new`.
/// After the call, `dict` is invalid.
#[no_mangle]
pub unsafe extern "C" fn sekka_free_dictionary(dict: *mut SekkaDictionaryFfi) {
    if dict.is_null() {
        return;
    }
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(dict));
    }));
}

/// Saves a single dictionary handle to disk (D-102)
///
/// `sekka_context_save_dictionaries` requires a `SekkaContextFfi*` (an input
/// context), so if no input context exists at the moment fcitx5 calls
/// `AddonInstance::save()`, there is nothing to walk and nothing gets saved
/// (RESEARCH Assumptions Log A1). `SekkaEngine` always holds the dictionary handle
/// (`userDict_`) itself, so a per-handle API like this one can save regardless of
/// whether an input context exists. For a read-only dictionary handle it safely
/// returns 0 thanks to the default implementation of `Dictionary::save()`
/// (`Ok(())`). Both are kept because `sekka_context_save_dictionaries` is a public
/// API the C ABI contract (`libsekka-capi.md:242-243`) already promises and D-102
/// decided to honour that contract. Panics on the Rust side are caught with
/// `catch_unwind` and turned into a non-zero return rather than propagated to the
/// C++ side.
///
/// # Safety
/// `dict` must be NULL or an unfreed pointer returned by `sekka_file_dict_new` / `sekka_user_dict_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_dictionary_save(dict: *mut SekkaDictionaryFfi) -> c_int {
    if dict.is_null() {
        return 1;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dict = unsafe { &*dict };
        match dict.dict.save() {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }))
    .unwrap_or(1)
}

/// Sets the dictionary list of a context
///
/// Any previously set dictionary list is replaced. Dictionaries are shared with the
/// context, so the same dictionary handle can be set on several contexts. A `count`
/// of 0 empties the dictionary list.
///
/// # Arguments
/// * `ctx` - pointer to the input context
/// * `dicts` - array of pointers to dictionary handles (may be NULL when `count` is 0)
/// * `count` - number of elements in the array
///
/// # Note
/// Ownership of the dictionary handles stays with the caller, who frees them with
/// `sekka_free_dictionary` once they are no longer needed. The context keeps
/// referring to the dictionaries after that, and a dictionary itself is closed when
/// its last reference disappears.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
/// `dicts` must be NULL or point to an array of `count` elements.
/// Each element must be NULL or an unfreed dictionary handle.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_set_dictionaries(
    ctx: *mut SekkaContextFfi,
    dicts: *mut *mut SekkaDictionaryFfi,
    count: c_int,
) {
    if ctx.is_null() || count < 0 || (dicts.is_null() && count > 0) {
        return;
    }
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        let dict_ptrs: &[*mut SekkaDictionaryFfi] = if count == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(dicts, count as usize) }
        };

        let dict_list = dict_ptrs
            .iter()
            .filter(|p| !p.is_null())
            .map(|&p| Arc::clone(unsafe { &(*p).dict }))
            .collect();

        ctx.ctx.reset();
        ctx.ctx.set_dictionaries(dict_list);
    }));
}

// ---------------------------------------------------------------------------
// Candidate list operations (US2)
// ---------------------------------------------------------------------------

/// Returns the current number of candidates
///
/// While registering (Phase 10, REG-02), follows the innermost step
/// (`active()`) - the frozen outer step's own candidates (if any) are never
/// visible during registration (D-170).
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_get_candidate_count(ctx: *mut SekkaContextFfi) -> c_int {
    if ctx.is_null() {
        return 0;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        ctx.ctx.active().get_candidates().len() as c_int
    }))
    .unwrap_or(0)
}

/// Gets the candidate list
///
/// # Returns
/// The number of slots written into `candidates`. A candidate containing a NUL byte
/// (where `CString::new` fails) gets NULL written into its slot, but that slot is
/// still counted (folded todo / 01.2-REVIEW WR-01 -> 01.4-REVIEW WR-01). The caller
/// must pass this number straight to `sekka_free_candidate_list`. It means something
/// different from `sekka_context_get_candidate_count` (the total number of
/// candidates, which ignores `offset`/`max_count`).
///
/// While registering (Phase 10, REG-02), follows the innermost step
/// (`active()`) - the frozen outer step's own candidates (if any) are never
/// visible during registration (D-170).
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
/// `candidates` must be NULL or point to an array with at least `max_count` writable elements.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_get_candidates(
    ctx: *mut SekkaContextFfi,
    candidates: *mut *mut c_char,
    max_count: c_int,
    offset: c_int,
) -> c_int {
    if ctx.is_null() || candidates.is_null() || max_count <= 0 || offset < 0 {
        return 0;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        let cands = ctx.ctx.active().get_candidates();
        let offset = offset as usize;
        let max_count = max_count as usize;
        let out = unsafe { slice::from_raw_parts_mut(candidates, max_count) };
        let mut count = 0;
        for i in 0..max_count {
            if offset + i >= cands.len() {
                break;
            }
            if let Ok(cstr) = CString::new(cands[offset + i].display.clone()) {
                out[i] = cstr.into_raw();
                count += 1;
            } else {
                // A candidate containing a NUL byte gets NULL written, but it is
                // still a written slot, so it counts (keeping this in step with the
                // range sekka_free_candidate_list walks; folded todo / SC7).
                out[i] = ptr::null_mut();
                count += 1;
            }
        }
        count as c_int
    }))
    .unwrap_or(0)
}

/// Selects the candidate at the given index
///
/// While registering (Phase 10, REG-02), follows the innermost step
/// (`active_mut()`) - selection during registration walks the innermost
/// step's own candidate window, never the frozen outer step's.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_select_candidate(ctx: *mut SekkaContextFfi, index: c_int) {
    if ctx.is_null() || index < 0 {
        return;
    }
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        let active = ctx.ctx.active_mut();
        // `next_candidate` / `prev_candidate` return without doing anything when
        // `state != Selecting`. Entering the loop below with a non-empty candidate
        // list while `state != Selecting` would therefore leave `candidate_index`
        // frozen forever and freeze all of fcitx5 (01.2-REVIEW WR-02). Unless this
        // invariant is written into the code, it will be hit the moment the C++ side
        // starts calling this function for click selection in the candidate window.
        if active.state() != ConversionState::Selecting {
            return;
        }
        let count = active.get_candidates().len() as i32;
        if index >= count {
            return;
        }
        // A second brake. `next_candidate` / `prev_candidate` wrap around at the end
        // and the start, so any target index is reached in at most `count` steps.
        // Even if the guard above is broken later, this loop always terminates.
        let mut steps_left = count;
        while active.get_candidate_index() != index && steps_left > 0 {
            if active.get_candidate_index() < index {
                active.next_candidate();
            } else {
                active.prev_candidate();
            }
            steps_left -= 1;
        }
    }));
}

/// Commits, in place, the selected candidate or the candidate shown in the preedit
/// in the commit display state (D-15)
///
/// In the Selecting state the selected candidate is moved into the commit display
/// state and then committed; in the commit display state it is committed as it is
/// (delegated to `finalize_staged`).
///
/// While registering (Phase 10, REG-02 / D-116 / D-173 / D-175), follows the
/// innermost step: `finalize_staged()` runs on `active_mut()` instead of the
/// outer context, so a click on an inner candidate (`SekkaCandidateList`'s
/// `selectAt`, C++ side unmodified) goes into the word being assembled - the
/// same "decide only" treatment as Ctrl-M (D-173) - and never reaches the
/// application. `absorb_registration_output()` then pulls whatever that step
/// just committed up into its parent's `draft`, exactly like every other
/// registration-mode key event. Outside registration `absorb_registration_output`
/// is a no-op, so this stays identical to the pre-Phase-10 behaviour. The
/// explicit-reset (`SekkaState::reset(true)`) handling of D-181 is
/// `sekka_context_finalize_for_reset` (a later plan) - not this function.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_confirm_candidate(ctx: *mut SekkaContextFfi) {
    if ctx.is_null() {
        return;
    }
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        ctx.ctx.active_mut().finalize_staged();
        ctx.ctx.absorb_registration_output();
    }));
}

/// Gets the index of the selected candidate
///
/// Returns -1 when nothing is selected, for a NULL pointer, or on panic.
///
/// While registering (Phase 10, REG-02), follows the innermost step
/// (`active()`).
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_get_candidate_index(ctx: *mut SekkaContextFfi) -> c_int {
    if ctx.is_null() {
        return -1;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        ctx.ctx.active().get_candidate_index()
    }))
    .unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Word registration (Phase 10)
// ---------------------------------------------------------------------------

/// Commits what the application shows before an explicit reset or an input
/// method switch (D-15 / D-181)
///
/// `SekkaState::reset(true)` calls this instead of `sekka_context_confirm_candidate`
/// (fcitx5-sekka's own C++ wiring, Task 2 of this plan). `sekka_context_confirm_candidate`
/// keeps its role as the click path of the candidate window (D-116). During word
/// registration, `finalize_for_reset` commits only the outermost step's reading -
/// the application's input position never shows more than that (D-170) - and
/// discards every registration step, nested or not: the word being assembled and
/// the candidate staged before Ctrl-R are neither committed nor registered.
/// Outside registration this is identical to `sekka_context_confirm_candidate`
/// (`finalize_staged`, D-15 unchanged). A real focus loss (D-14/D-180) never calls
/// this - only `sekka_context_reset` runs, which discards the registration session
/// the same way but commits nothing at all.
///
/// Passing a NULL pointer does nothing.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_finalize_for_reset(ctx: *mut SekkaContextFfi) {
    if ctx.is_null() {
        return;
    }
    let _ = catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        ctx.ctx.finalize_for_reset();
    }));
}

/// Returns whether word registration is active (D-170)
///
/// Returns 0 (not registering) for a NULL pointer or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_is_registering(ctx: *mut SekkaContextFfi) -> c_int {
    if ctx.is_null() {
        return 0;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        if ctx.ctx.is_registering() {
            1
        } else {
            0
        }
    }))
    .unwrap_or(0)
}

/// Gets the outermost registration step's typed reading (D-168/D-170)
///
/// For the application's own input position (client preedit): just the
/// reading, with no label and no inner-step content. Returns an empty string
/// (not NULL) when not registering. The caller is responsible for freeing the
/// returned pointer with `sekka_free_string`. Returns NULL for a NULL pointer
/// or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_get_registration_reading(
    ctx: *mut SekkaContextFfi,
) -> *mut c_char {
    if ctx.is_null() {
        return ptr::null_mut();
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        let reading = ctx.ctx.registration_reading().unwrap_or("").to_string();
        match CString::new(reading) {
            Ok(cstr) => cstr.into_raw(),
            Err(_) => ptr::null_mut(),
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Gets the registration popup's label (`registration_prompt()`, D-170)
///
/// For the popup's auxUp: every nested step's reading followed by "登録 ".
/// Returns an empty string (not NULL) when not registering. The caller is
/// responsible for freeing the returned pointer with `sekka_free_string`.
/// Returns NULL for a NULL pointer or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_get_registration_prompt(
    ctx: *mut SekkaContextFfi,
) -> *mut c_char {
    if ctx.is_null() {
        return ptr::null_mut();
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        let prompt = ctx.ctx.registration_prompt().unwrap_or_default();
        match CString::new(prompt) {
            Ok(cstr) => cstr.into_raw(),
            Err(_) => ptr::null_mut(),
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Frees the memory of a candidate list
///
/// Pass the return value of `sekka_context_get_candidates` straight through as
/// `count`. NULL slots are skipped (including the NULL written for a candidate
/// containing a NUL byte, which is skipped safely; folded todo / SC7).
///
/// # Safety
/// `candidates` must be NULL or point to an array of the `count` elements
/// `sekka_context_get_candidates` wrote. Each element must be NULL or an unfreed string.
#[no_mangle]
pub unsafe extern "C" fn sekka_free_candidate_list(candidates: *mut *mut c_char, count: c_int) {
    if candidates.is_null() || count <= 0 {
        return;
    }
    let _ = catch_unwind(|| {
        let ptrs = unsafe { slice::from_raw_parts(candidates, count as usize) };
        for &p in ptrs {
            if !p.is_null() {
                unsafe {
                    drop(CString::from_raw(p));
                }
            }
        }
    });
}

/// Saves every dictionary the context refers to, to disk (D-102)
///
/// Calls `SekkaContext::save_dictionaries()` (added in `Task 1`) and returns the
/// real outcome. As the C ABI contract says (`libsekka-capi.md:242-243`), 0 means
/// success and non-zero means an error. It returns non-zero when `ctx` is NULL or
/// when any dictionary fails to save. For a context with no dictionaries set there
/// is nothing to save, so it returns 0 (a no-op success). Panics on the Rust side
/// are caught with `catch_unwind` and turned into a non-zero return rather than
/// propagated to the C++ side.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_save_dictionaries(ctx: *mut SekkaContextFfi) -> c_int {
    if ctx.is_null() {
        return 1;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &*ctx };
        match ctx.ctx.save_dictionaries() {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }))
    .unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Configuration API (US3)
// ---------------------------------------------------------------------------

/// Notifies that the conversion trigger was pressed (D-110)
///
/// The trigger means three different things depending on the state:
/// - initial state with a non-empty romaji buffer: start conversion
/// - commit display state (a word was just committed): start reselection
/// - during reselection: advance to the next candidate
///
/// Those three branches are already implemented in
/// `SekkaContext::process_key(ch, is_ctrl_j=true)` (`libsekka/src/context.rs`).
/// This function only delegates the single bit of information the C++ side has -
/// "the trigger key was pressed" - as `is_ctrl_j=true`, keeping the state
/// transition branching itself inside libsekka. This mirrors how upstream
/// kiyoka/sekka binds the variable `sekka-rK-trans-key` in both `sekka-mode-map`
/// and `sekka-select-mode-map` (D-109). Deciding whether a key is the trigger
/// (matching against the KeyList with `Key::checkKeyList`) is the C++ side's job
/// (`fcitx5-sekka/src/sekka.cpp`), and this function receives nothing beyond "the
/// trigger was pressed" (the signature of `sekka_context_process_key_event` does
/// not change, D-110).
///
/// # Returns
/// Returns 1 when the event was consumed and 0 when it was ignored. Returns 0 for a
/// NULL pointer or on panic.
///
/// # Safety
/// `ctx` must be NULL or an unfreed pointer returned by `sekka_context_new`.
#[no_mangle]
pub unsafe extern "C" fn sekka_context_trigger(ctx: *mut SekkaContextFfi) -> c_int {
    if ctx.is_null() {
        return 0;
    }
    catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = unsafe { &mut *ctx };
        // Initialize the forward flag for this event (never carry over the previous
        // one; the same discipline as `sekka_context_process_key_event`). Without
        // this, the forward flag of the previous key event would survive and a
        // trigger press would deliver the raw key to the application twice.
        ctx.forward_key = false;

        // Phase 10: while registering, the trigger is just an ordinary key
        // routed to the innermost step (Ctrl-J conversion inside the word
        // being assembled, REG-02) and always consumed - never forwarded.
        if ctx.ctx.is_registering() {
            ctx.ctx.active_mut().process_key('\0', true);
            ctx.ctx.absorb_registration_output();
            return 1;
        }

        if ctx.ctx.process_key('\0', true) {
            1
        } else {
            0
        }
    }))
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::dict_format;
    use crate::dictionary::DictEntry;
    use std::collections::BTreeMap;
    use std::ffi::CStr;

    #[test]
    fn creating_and_freeing_a_context() {
        unsafe {
            let ctx = sekka_context_new();
            assert!(!ctx.is_null());
            sekka_context_free(ctx);
        }
    }

    #[test]
    fn freeing_a_null_pointer_is_safe() {
        unsafe {
            sekka_context_free(ptr::null_mut());
            sekka_free_string(ptr::null_mut());
            sekka_free_dictionary(ptr::null_mut());
        }
    }

    #[test]
    fn operations_on_a_null_context_are_safe() {
        unsafe {
            sekka_context_reset(ptr::null_mut());
            assert_eq!(sekka_context_process_key_event(ptr::null_mut(), 0, 0, 0), 0);
            assert!(sekka_context_get_preedit(ptr::null_mut()).is_null());
            assert_eq!(sekka_context_get_preedit_cursor_pos(ptr::null_mut()), 0);
            assert!(sekka_context_poll_output(ptr::null_mut()).is_null());
            assert_eq!(sekka_context_get_candidate_index(ptr::null_mut()), -1);
            assert_eq!(sekka_context_take_forward_key(ptr::null_mut()), 0);
        }
    }

    #[test]
    fn the_version_string_can_be_obtained() {
        unsafe {
            let ver = sekka_get_version();
            assert!(!ver.is_null());
            let ver_str = CStr::from_ptr(ver);
            let ver_str = ver_str
                .to_str()
                .expect("failed to convert the version string to UTF-8");
            assert!(!ver_str.is_empty());
            // Confirm it matches the version in Cargo.toml.
            assert_eq!(ver_str, env!("CARGO_PKG_VERSION"));
        }
    }

    #[test]
    fn key_input_updates_the_preedit() {
        unsafe {
            let ctx = sekka_context_new();
            assert!(!ctx.is_null());

            // Type 'a' -> the romaji "a" appears in the preedit.
            let consumed = sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            assert_eq!(consumed, 1);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            let preedit_str = CStr::from_ptr(preedit);
            assert_eq!(preedit_str.to_str().unwrap(), "a");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn release_events_are_ignored() {
        unsafe {
            let ctx = sekka_context_new();
            let consumed = sekka_context_process_key_event(ctx, b'a' as u32, 0, 1);
            assert_eq!(consumed, 0);
            sekka_context_free(ctx);
        }
    }

    #[test]
    fn enter_during_input_commits_the_romaji_as_is_and_forwards() {
        unsafe {
            let ctx = sekka_context_new();

            // Type "ls" (the romaji accumulates in the buffer).
            sekka_context_process_key_event(ctx, b'l' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b's' as u32, 0, 0);

            // Enter commits the romaji as it is and the key is forwarded too (D-03).
            let consumed = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(consumed, 1);

            // The preedit becomes empty.
            let preedit = sekka_context_get_preedit(ctx);
            let preedit_str = CStr::from_ptr(preedit);
            assert_eq!(preedit_str.to_str().unwrap(), "");
            sekka_free_string(preedit);

            // Poll the output (the romaji as it is, "ls").
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            let output_str = CStr::from_ptr(output);
            assert_eq!(output_str.to_str().unwrap(), "ls");
            sekka_free_string(output);

            // Confirm the queue is now empty.
            let output2 = sekka_context_poll_output(ctx);
            assert!(output2.is_null());

            // The same key (Enter) is forwarded.
            assert_eq!(sekka_context_take_forward_key(ctx), 1);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_printable_symbol_during_input_is_appended_to_the_romaji_and_not_forwarded() {
        // D-158 / COMMIT-01 tracer (todo symptom 2's keystroke sequence `C t r l +`).
        // Before D-158, `+` (0x2B) is not `is_romaji_char`, so it fell into the
        // "Anything else" branch: the romaji buffer ("Ctrl") was committed as-is and
        // `+` was forwarded, losing the `+` whenever the client fails to reflect the
        // commit before processing the forwarded key. D-158 appends the printable key
        // to whatever this key event commits, so it goes out as a single "Ctrl+" and
        // nothing is forwarded.
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'C' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b't' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'r' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'l' as u32, 0, 0);

            let consumed = sekka_context_process_key_event(ctx, 0x2B, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "Ctrl+");
            sekka_free_string(output);

            let output2 = sekka_context_poll_output(ctx);
            assert!(output2.is_null());

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_space_during_input_is_appended_to_the_romaji_and_not_forwarded() {
        // D-158 (Phase 9): Space is a printable key (`is_printable_ascii`), so it is
        // no longer a non-character key that commits-and-forwards (D-03/D-166: this
        // test used to be `non_character_keys_during_input_commit_the_romaji_and_forward`).
        // It is appended to the romaji buffer's commit instead, and not forwarded.
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'h' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);

            let consumed = sekka_context_process_key_event(ctx, 0x20, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "hi ");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn escape_during_input_commits_the_romaji_as_is_and_forwards() {
        unsafe {
            let ctx = sekka_context_new();

            for ch in "abc".bytes() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }

            let consumed = sekka_context_process_key_event(ctx, 0xFF1B, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "abc");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 1);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn the_case_is_preserved_on_commit() {
        // D-158 (Phase 9): Space is now appended to the commit ("Hello ") instead of
        // being a separate forwarded key; the case-preservation claim this test
        // fixes is unchanged.
        unsafe {
            let ctx = sekka_context_new();

            for ch in "Hello".bytes() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }

            let consumed = sekka_context_process_key_event(ctx, 0x20, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "Hello ");
            sekka_free_string(output);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn symbols_during_input_accumulate_in_the_romaji_buffer() {
        // D-20/D-29: `.` joined the accepted symbol set, so it is treated as a
        // character key (accumulating in the buffer) rather than a non-character key
        // (which would immediately commit the romaji buffer and forward to the
        // application). The expectations are the exact opposite of the pre-flip test.
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null(), "a symbol key should not commit");
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "ka.");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn digits_during_input_accumulate_in_the_romaji_buffer() {
        // D-21/D-29: digits (0-9) became character keys, accepted together with
        // upstream's number conversion branch (NumberOnly/NumberPrefixed). The
        // expectations are the exact opposite of the pre-flip test.
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            let consumed = sekka_context_process_key_event(ctx, b'1' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null(), "a digit key should not commit");
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "ka1");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn is_romaji_char_accepts_digits_0_to_9() {
        // D-21: pins that 0x30('0') to 0x39('9') are true for is_romaji_char on its own.
        for keysym in 0x30u32..=0x39 {
            assert!(is_romaji_char(keysym), "keysym={:#x}", keysym);
        }
    }

    #[test]
    fn is_romaji_char_accepts_a_hyphen_as_a_character_key() {
        // 02-01: pins with is_romaji_char alone that `-` (keysym 0x2D) is a character
        // key. The character key set of spec.md FR-003/FR-014 and REQUIREMENTS.md
        // FR-003 lists `-` explicitly, and the implementation (the trailing
        // `|| keysym == b'-' as u32`) was confirmed to match (02-RESEARCH.md Open
        // Question 1 / Assumptions Log A4).
        assert!(is_romaji_char(b'-' as u32));
    }

    #[test]
    fn is_printable_ascii_covers_exactly_0x20_to_0x7e() {
        // D-158: pins the boundary of `is_printable_ascii` on its own, including
        // that control keysyms outside the printable ASCII range (BackSpace,
        // Enter, Tab, Escape, the arrow keys) are never treated as printable
        // (09-RESEARCH Security Domain: do not append control keys as printable
        // characters).
        for keysym in [0x1Fu32, 0x7Fu32] {
            assert!(!is_printable_ascii(keysym), "keysym={:#x}", keysym);
        }
        for keysym in [0x20u32, 0x2Bu32, 0x7Eu32] {
            assert!(is_printable_ascii(keysym), "keysym={:#x}", keysym);
        }
        for keysym in [
            0xFF08u32, 0xFF09, 0xFF0D, 0xFF1B, 0xFF51, 0xFF52, 0xFF53, 0xFF54,
        ] {
            assert!(!is_printable_ascii(keysym), "keysym={:#x}", keysym);
        }
    }

    #[test]
    fn printable_ascii_boundaries_decide_between_appending_and_forwarding() {
        // D-158: end-to-end through dispatch_input, the same boundary values as
        // `is_printable_ascii_covers_exactly_0x20_to_0x7e` decide between
        // appending (0x20, 0x7E) and the D-03 commit-and-forward path (0x1F,
        // 0x7F). Each case uses a fresh context with "k" already in the buffer.
        unsafe {
            for (keysym, expected_output, expected_forward) in [
                (0x1Fu32, "k", 1),
                (0x20, "k ", 0),
                (0x7E, "k~", 0),
                (0x7F, "k", 1),
            ] {
                let ctx = sekka_context_new();
                sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, 0, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(
                    CStr::from_ptr(output).to_str().unwrap(),
                    expected_output,
                    "keysym={:#x}",
                    keysym
                );
                sekka_free_string(output);

                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    expected_forward,
                    "keysym={:#x}",
                    keysym
                );

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn a_printable_symbol_with_ctrl_or_super_commits_and_forwards_even_with_alt() {
        // D-162/D-188: printable keys held with Ctrl or Super (Alt or not) are
        // excluded from D-158 and from D-185's Alt passthrough (`alt_only` is false
        // as soon as Ctrl or Super is held) and keep the D-03 commit-and-forward
        // behavior, like any other modified key. Until Phase 11 a lone Alt was in this
        // list too (see the test below). Each case uses a fresh context with "k"
        // already in the buffer.
        unsafe {
            for modifiers in [MOD_CTRL, MOD_SUPER, MOD_CTRL | MOD_ALT, MOD_SUPER | MOD_ALT] {
                let ctx = sekka_context_new();
                sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);

                let consumed = sekka_context_process_key_event(ctx, 0x2B, modifiers, 0);
                assert_eq!(consumed, 1, "modifiers={:#x}", modifiers);

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "modifiers={:#x}", modifiers);
                assert_eq!(
                    CStr::from_ptr(output).to_str().unwrap(),
                    "k",
                    "modifiers={:#x}",
                    modifiers
                );
                sekka_free_string(output);

                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    1,
                    "modifiers={:#x}",
                    modifiers
                );

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn a_printable_symbol_with_alt_alone_is_appended_without_alt_and_not_forwarded() {
        // D-185: a printable key held with Alt alone is appended to whatever this key
        // event commits and is not forwarded (the Alt is stripped by committing it as
        // text). Phase 11 and earlier committed "k" and forwarded the key (D-162).
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);

            let consumed = sekka_context_process_key_event(ctx, 0x2B, MOD_ALT, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "k+");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn an_alt_printable_key_with_nothing_to_commit_is_consumed_and_committed_alone() {
        // D-185 (table row 1): unlike D-158 (a plain printable key with nothing to
        // commit stays unconsumed), an Alt-held printable key is consumed even then:
        // leaving it unconsumed would hand the application the Alt-modified key.
        unsafe {
            let ctx = sekka_context_new();

            let consumed = sekka_context_process_key_event(ctx, b'd' as u32, MOD_ALT, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "d");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(preedit_string(ctx), "");

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn alt_shift_keys_arrive_with_shift_applied_and_are_committed_as_is() {
        // D-186 (S1): fcitx5 delivers Alt+Shift+d as keysym 'D' and Alt+Shift+; as
        // keysym ':' (Shift is already applied to the keysym, and Sekka does not pass
        // Shift to libsekka), so the Alt branch commits exactly that character. Digits
        // and ':' are romaji characters, but with Alt held they never reach the romaji
        // buffer: they are committed as text.
        unsafe {
            for (keysym, expected) in [(b'D' as u32, "D"), (b':' as u32, ":"), (b'3' as u32, "3")] {
                let ctx = sekka_context_new();

                let consumed = sekka_context_process_key_event(ctx, keysym, MOD_ALT, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(
                    CStr::from_ptr(output).to_str().unwrap(),
                    expected,
                    "keysym={:#x}",
                    keysym
                );
                sekka_free_string(output);

                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "keysym={:#x}",
                    keysym
                );
                assert_eq!(preedit_string(ctx), "", "keysym={:#x}", keysym);

                sekka_context_free(ctx);
            }

            // During romaji input the character is joined to the buffer in one commit.
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);

            let consumed = sekka_context_process_key_event(ctx, b':' as u32, MOD_ALT, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "k:");
            sekka_free_string(output);
            assert!(
                sekka_context_poll_output(ctx).is_null(),
                "a single commit only"
            );
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn alt_printable_ascii_boundaries_decide_between_committing_and_forwarding() {
        // D-186 (Claude's Discretion): the printable range with Alt held is the same as
        // D-158's (0x20..=0x7E; Latin-1 such as 0xA5 is not included). Inside the range
        // the character is committed without Alt and not forwarded; outside it the key
        // is a non-character key (D-187): commit and forward when there is something to
        // commit, unconsumed when there is not. Each case uses a fresh context.
        unsafe {
            // With "k" in the buffer.
            for (keysym, expected_output, expected_forward) in [
                (0x1Fu32, "k", 1),
                (0x20, "k ", 0),
                (0x7E, "k~", 0),
                (0x7F, "k", 1),
                (0xA5, "k", 1),
            ] {
                let ctx = sekka_context_new();
                sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, MOD_ALT, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(
                    CStr::from_ptr(output).to_str().unwrap(),
                    expected_output,
                    "keysym={:#x}",
                    keysym
                );
                sekka_free_string(output);

                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    expected_forward,
                    "keysym={:#x}",
                    keysym
                );

                sekka_context_free(ctx);
            }

            // With nothing in the buffer: the range is consumed and committed alone,
            // everything else stays unconsumed.
            for (keysym, expected_output) in [
                (0x20u32, Some(" ")),
                (0x7E, Some("~")),
                (0x1F, None),
                (0x7F, None),
                (0xA5, None),
            ] {
                let ctx = sekka_context_new();

                let consumed = sekka_context_process_key_event(ctx, keysym, MOD_ALT, 0);
                assert_eq!(
                    consumed,
                    if expected_output.is_some() { 1 } else { 0 },
                    "keysym={:#x}",
                    keysym
                );

                let output = sekka_context_poll_output(ctx);
                match expected_output {
                    Some(expected) => {
                        assert!(!output.is_null(), "keysym={:#x}", keysym);
                        assert_eq!(
                            CStr::from_ptr(output).to_str().unwrap(),
                            expected,
                            "keysym={:#x}",
                            keysym
                        );
                        sekka_free_string(output);
                    }
                    None => assert!(output.is_null(), "keysym={:#x}", keysym),
                }
                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "keysym={:#x}",
                    keysym
                );

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn the_long_vowel_mark_accumulates_as_romaji() {
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'o' as u32, 0, 0);
            let consumed = sekka_context_process_key_event(ctx, b'-' as u32, 0, 0);
            assert_eq!(consumed, 1);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "ko-");
            sekka_free_string(preedit);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_non_character_key_with_an_empty_buffer_is_not_consumed() {
        unsafe {
            let ctx = sekka_context_new();

            let consumed = sekka_context_process_key_event(ctx, 0x20, 0, 0);
            assert_eq!(consumed, 0);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_printable_symbol_with_nothing_to_commit_is_not_consumed() {
        // D-158: a printable symbol other than Space (which the test above already
        // pins) is also unconsumed when there is nothing for this key event to
        // commit (empty buffer, no staged candidate, no flush).
        unsafe {
            let ctx = sekka_context_new();

            let consumed = sekka_context_process_key_event(ctx, 0x2B, 0, 0);
            assert_eq!(consumed, 0);

            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_lone_modifier_key_does_not_commit_romaji() {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);

            for keysym in [0xFFE1u32, 0xFFE3, 0xFFE9, 0xFFEB, 0xFE03] {
                let consumed = sekka_context_process_key_event(ctx, keysym, 0, 0);
                assert_eq!(consumed, 0);
                let preedit = sekka_context_get_preedit(ctx);
                assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "k");
                sekka_free_string(preedit);
            }

            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_lone_modifier_key_does_not_invalidate_the_commit_display_state() {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            sekka_context_trigger(ctx);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            let consumed = sekka_context_process_key_event(ctx, 0xFFE3, 0, 0);
            assert_eq!(consumed, 0);

            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            // Entering reselection causes neither a commit nor a delete request (stage 1 of D-18).
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn other_keys_in_the_commit_display_state_commit_and_forward() {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            sekka_context_trigger(ctx);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            // In the commit display state every key other than Ctrl-J commits and forwards (D-12).
            let consumed = sekka_context_process_key_event(ctx, 0xFF53, 0, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 1);

            // It is already committed, so the following Ctrl-J has nothing to reselect and is unconsumed.
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn backspace_deletes_one_romaji_character() {
        unsafe {
            let ctx = sekka_context_new();
            for ch in "kan".bytes() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }

            let consumed = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(consumed, 1);
            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "ka");
            sekka_free_string(preedit);

            sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "");
            sekka_free_string(preedit);

            let consumed = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(consumed, 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn super_keys_are_non_character_keys() {
        // D-188: a key held with Super stays a non-character key (commit the romaji
        // and forward). The Alt half of this test moved to
        // `an_alt_letter_during_input_is_appended_without_alt_and_not_forwarded`
        // (D-185); Phase 11 and earlier committed and forwarded Alt keys too (D-162).
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            let consumed = sekka_context_process_key_event(ctx, b'f' as u32, 0x40, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "k");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 1);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn an_alt_letter_during_input_is_appended_without_alt_and_not_forwarded() {
        // D-185 (table row 2): during romaji input, an Alt-held letter is appended to
        // the romaji as it stands (D-03: not converted) and committed in one go;
        // nothing is forwarded. fcitx5-sekka passes the unnormalized (lowercase)
        // keysym for Alt-only keys.
        unsafe {
            let ctx = sekka_context_new();

            for ch in [b'K', b'a', b'n', b'j'] {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            let consumed = sekka_context_process_key_event(ctx, b'd' as u32, MOD_ALT, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "Kanjd");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(preedit_string(ctx), "");

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn other_ctrl_keys_are_non_character_keys() {
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            let consumed = sekka_context_process_key_event(ctx, b's' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "k");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 1);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn reset_clears_the_preedit() {
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);

            sekka_context_reset(ctx);

            let preedit = sekka_context_get_preedit(ctx);
            let preedit_str = CStr::from_ptr(preedit);
            assert_eq!(preedit_str.to_str().unwrap(), "");
            sekka_free_string(preedit);

            assert_eq!(sekka_context_get_preedit_cursor_pos(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn the_cursor_position_is_returned_correctly() {
        unsafe {
            let ctx = sekka_context_new();

            // Type 'k' (accumulates in the romaji buffer).
            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            let pos = sekka_context_get_preedit_cursor_pos(ctx);
            assert_eq!(pos, 1); // "k" = 1 byte

            // Type 'a' -> the preedit is "ka" (still romaji).
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            let pos = sekka_context_get_preedit_cursor_pos(ctx);
            assert_eq!(pos, 2); // "ka" = 2 bytes

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_null_dictionary_path_is_handled_safely() {
        unsafe {
            let dict = sekka_file_dict_new(ptr::null(), ptr::null());
            assert!(dict.is_null());

            let dict = sekka_user_dict_new(ptr::null(), ptr::null());
            assert!(dict.is_null());
        }
    }

    // === D-64: error kinds of sekka_file_dict_new_with_error ===

    #[test]
    fn a_missing_path_yields_not_found_and_creates_nothing() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let missing_path = tmp.path().join("does-not-exist.dict");
            let path_cstr =
                CString::new(missing_path.to_str().unwrap()).expect("failed to build the CString");

            let mut out_error: c_int = SEKKA_DICT_OK;
            let dict =
                sekka_file_dict_new_with_error(path_cstr.as_ptr(), ptr::null(), &mut out_error);

            assert!(dict.is_null());
            assert_eq!(out_error, SEKKA_DICT_ERROR_NOT_FOUND);
            assert!(
                !missing_path.exists(),
                "no empty dictionary may be created at a missing path (proof that sled's old behaviour is gone)"
            );
        }
    }

    #[test]
    fn a_file_with_a_broken_header_yields_corrupt() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let path = tmp.path().join("corrupt.dict");
            std::fs::write(&path, [0u8; 64]).expect("failed to write");
            let path_cstr =
                CString::new(path.to_str().unwrap()).expect("failed to build the CString");

            let mut out_error: c_int = SEKKA_DICT_OK;
            let dict =
                sekka_file_dict_new_with_error(path_cstr.as_ptr(), ptr::null(), &mut out_error);

            assert!(dict.is_null());
            assert_eq!(out_error, SEKKA_DICT_ERROR_CORRUPT);
        }
    }

    #[test]
    fn a_null_out_error_does_not_crash() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let missing_path = tmp.path().join("does-not-exist.dict");
            let path_cstr =
                CString::new(missing_path.to_str().unwrap()).expect("failed to build the CString");

            let dict =
                sekka_file_dict_new_with_error(path_cstr.as_ptr(), ptr::null(), ptr::null_mut());
            assert!(dict.is_null());
        }
    }

    #[test]
    fn a_null_path_yields_invalid_arg() {
        unsafe {
            let mut out_error: c_int = SEKKA_DICT_OK;
            let dict = sekka_file_dict_new_with_error(ptr::null(), ptr::null(), &mut out_error);

            assert!(dict.is_null());
            assert_eq!(out_error, SEKKA_DICT_ERROR_INVALID_ARG);
        }
    }

    #[test]
    fn creating_and_freeing_a_user_dictionary() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let dict_path = tmp.path().join("test_user_dict");
            let path_cstr =
                CString::new(dict_path.to_str().unwrap()).expect("failed to build the CString");

            let dict = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(!dict.is_null());
            sekka_free_dictionary(dict);
        }
    }

    #[test]
    fn a_dictionary_handle_can_be_shared_by_several_contexts() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let dict_path = tmp.path().join("shared_user_dict");
            let path_cstr =
                CString::new(dict_path.to_str().unwrap()).expect("failed to build the CString");
            let mut dict = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(!dict.is_null());

            let ctx1 = sekka_context_new();
            let ctx2 = sekka_context_new();
            // Setting the same handle on two contexts, and setting it again, must not double-free.
            sekka_context_set_dictionaries(ctx1, &mut dict, 1);
            sekka_context_set_dictionaries(ctx2, &mut dict, 1);
            sekka_context_set_dictionaries(ctx1, &mut dict, 1);

            // The context stays usable after the handle is freed.
            sekka_free_dictionary(dict);
            assert_eq!(sekka_context_process_key_event(ctx1, b'a' as u32, 0, 0), 1);

            // The dictionary list can be emptied.
            sekka_context_set_dictionaries(ctx2, ptr::null_mut(), 0);

            sekka_context_free(ctx1);
            sekka_context_free(ctx2);
        }
    }

    #[test]
    fn setting_dictionaries_on_a_null_context_is_safe() {
        unsafe {
            sekka_context_set_dictionaries(ptr::null_mut(), ptr::null_mut(), 0);
        }
    }

    #[test]
    fn an_uppercase_ctrl_j_also_converts() {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            sekka_context_free(ctx);
        }
    }

    #[test]
    fn ctrl_j_stages_the_first_candidate_without_committing() {
        unsafe {
            let ctx = sekka_context_new();

            // Type "a" (accumulates in the romaji buffer).
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);

            // Ctrl-J places the first candidate in the preedit (no candidate window,
            // and no committed_output either; revised D-08).
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            // The first candidate lands in the preedit ("あ", since this is hiragana-only mode).
            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            let preedit_str = CStr::from_ptr(preedit);
            assert_eq!(preedit_str.to_str().unwrap(), "あ");
            sekka_free_string(preedit);

            // Nothing is sent to the application yet.
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_second_ctrl_j_enters_reselection_without_any_delete_request() {
        unsafe {
            let ctx = sekka_context_new();

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            // Ctrl-J only places it in the preedit and does not commit (revised D-08).
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            // A second Ctrl-J in the commit display state enters reselection.
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            let preedit_str = CStr::from_ptr(preedit);
            assert_eq!(preedit_str.to_str().unwrap(), "か");
            sekka_free_string(preedit);

            // Move to the next candidate (Ctrl-J with the uppercase keysym; fcitx5 normalizes Ctrl + letter to uppercase).
            let consumed = sekka_context_process_key_event(ctx, b'J' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), 1);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            let preedit_str = CStr::from_ptr(preedit);
            assert_eq!(preedit_str.to_str().unwrap(), "カ");
            sekka_free_string(preedit);

            // Enter only returns to the commit display state and does not commit yet (revised D-08).
            let consumed = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_get_candidate_count(ctx), 0);

            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "カ");
            sekka_free_string(preedit);

            // Only the next key (anything but Ctrl-J) commits it (D-12).
            let consumed = sekka_context_process_key_event(ctx, b's' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "カ");
            sekka_free_string(output);
            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "s");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    /// Types "K","a", commits immediately with Ctrl-J (「か」, since there is no
    /// dictionary) and enters reselection with a second Ctrl-J. Reselection offers
    /// four candidates: か(0), カ(1), Ｋａ(2), Ka(3).
    unsafe fn reselection_is_entered_for_ka_without_a_dictionary(ctx: *mut SekkaContextFfi) {
        unsafe {
            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            sekka_context_trigger(ctx);
            let output = sekka_context_poll_output(ctx);
            if !output.is_null() {
                sekka_free_string(output);
            }
            let entered = sekka_context_trigger(ctx);
            assert_eq!(entered, 1);
        }
    }

    /// Helper that returns the preedit of the candidate during reselection
    unsafe fn preedit_string(ctx: *mut SekkaContextFfi) -> String {
        unsafe {
            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            let preedit_str = CStr::from_ptr(preedit).to_str().unwrap().to_string();
            sekka_free_string(preedit);
            preedit_str
        }
    }

    #[test]
    fn ctrl_l_and_ctrl_e_switch_to_alphabet_candidates_during_reselection() {
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);

            // Ctrl-L (lowercase keysym) switches to the half-width alphabet candidate.
            let consumed = sekka_context_process_key_event(ctx, b'l' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "Ka");

            // Ctrl-L (uppercase keysym) does the same.
            let consumed = sekka_context_process_key_event(ctx, b'L' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "Ka");

            // Ctrl-E (lowercase keysym) switches to the full-width alphabet candidate.
            let consumed = sekka_context_process_key_event(ctx, b'e' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "Ｋａ");

            // Ctrl-E (uppercase keysym) does the same.
            let consumed = sekka_context_process_key_event(ctx, b'E' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "Ｋａ");

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn ctrl_i_ctrl_k_and_ctrl_u_switch_to_kana_candidates_during_reselection() {
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);

            // Ctrl-I (lower/uppercase) switches to the katakana candidate.
            let consumed = sekka_context_process_key_event(ctx, b'i' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "カ");

            let consumed = sekka_context_process_key_event(ctx, b'I' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "カ");

            // Ctrl-K (lower/uppercase) switches to the katakana candidate as well.
            let consumed = sekka_context_process_key_event(ctx, b'k' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "カ");

            let consumed = sekka_context_process_key_event(ctx, b'K' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "カ");

            // Ctrl-U (lower/uppercase) switches to the hiragana candidate.
            let consumed = sekka_context_process_key_event(ctx, b'u' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "か");

            let consumed = sekka_context_process_key_event(ctx, b'U' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(preedit_string(ctx), "か");

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn without_kanji_candidates_ctrl_a_is_consumed_and_changes_nothing() {
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);

            // Right after entering reselection the selection is index 0 (か).
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);

            // With no dictionary there are no Kanji/KanjiWithOkuri candidates. The key
            // is consumed but the selection does not change (lower and uppercase alike).
            let consumed = sekka_context_process_key_event(ctx, b'a' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);

            let consumed = sekka_context_process_key_event(ctx, b'A' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    // === The remaining key assignments during reselection (D-09) ===

    #[test]
    fn the_next_candidate_key_in_selection_mode() {
        unsafe {
            for (keysym, modifiers) in [
                (b'n' as u32, 0x4u32),
                (b'N' as u32, 0x4u32),
                (0x20u32, 0u32),
                (b'j' as u32, 0x4u32),
                (b'J' as u32, 0x4u32),
            ] {
                let ctx = sekka_context_new();
                reselection_is_entered_for_ka_without_a_dictionary(ctx);
                assert_eq!(sekka_context_get_candidate_index(ctx), 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(consumed, 1);
                assert_eq!(sekka_context_get_candidate_index(ctx), 1);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn the_previous_candidate_key_in_selection_mode() {
        unsafe {
            for (keysym, modifiers) in [(b'p' as u32, 0x4u32), (b'P' as u32, 0x4u32)] {
                let ctx = sekka_context_new();
                reselection_is_entered_for_ka_without_a_dictionary(ctx);
                let count = sekka_context_get_candidate_count(ctx);

                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(consumed, 1);
                assert_eq!(sekka_context_get_candidate_index(ctx), count - 1);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn the_confirm_key_in_selection_mode() {
        unsafe {
            for (keysym, modifiers) in [
                (0xFF0Du32, 0u32),
                (0xFF8Du32, 0u32),
                (b'm' as u32, 0x4u32),
                (b'M' as u32, 0x4u32),
            ] {
                let ctx = sekka_context_new();
                reselection_is_entered_for_ka_without_a_dictionary(ctx);
                // Advance to the next candidate and then confirm (か -> カ).
                sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(consumed, 1);

                // It only returns to the commit display state and does not commit yet (revised D-08).
                let output = sekka_context_poll_output(ctx);
                assert!(output.is_null());
                let preedit = sekka_context_get_preedit(ctx);
                assert!(!preedit.is_null());
                assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "カ");
                sekka_free_string(preedit);

                assert_eq!(sekka_context_take_forward_key(ctx), 0);
                assert_eq!(sekka_context_get_candidate_count(ctx), 0);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn the_cancel_key_in_selection_mode() {
        unsafe {
            for (keysym, modifiers) in [
                (b'g' as u32, 0x4u32),
                (b'G' as u32, 0x4u32),
                (0x71u32, 0u32),
                (0xFF1Bu32, 0u32),
            ] {
                let ctx = sekka_context_new();
                reselection_is_entered_for_ka_without_a_dictionary(ctx);
                // Even after advancing and then cancelling, the preedit returns to the originally committed word (か).
                sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(consumed, 1);

                // Cancelling does not commit (the meaning changed in D-19).
                let output = sekka_context_poll_output(ctx);
                assert!(output.is_null());
                let preedit = sekka_context_get_preedit(ctx);
                assert!(!preedit.is_null());
                assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "か");
                sekka_free_string(preedit);

                assert_eq!(sekka_context_take_forward_key(ctx), 0);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn an_uppercase_q_in_selection_mode_is_an_other_key() {
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);

            // An uppercase Q is not the cancel key but an "other key": it confirms with
            // the selected candidate (か) and then the same key continues as new romaji input.
            let consumed = sekka_context_process_key_event(ctx, b'Q' as u32, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
            sekka_free_string(output);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "Q");
            sekka_free_string(preedit);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn other_character_keys_in_selection_mode_commit_and_continue_input() {
        // D-21: confirms that '1' (a digit), like 'k' (a letter), takes the same path
        // (confirm, then re-dispatch the same key to dispatch_input).
        unsafe {
            for (keysym, expected_preedit) in [(b'k' as u32, "k"), (b'1' as u32, "1")] {
                let ctx = sekka_context_new();
                reselection_is_entered_for_ka_without_a_dictionary(ctx);
                // Advance to the next candidate and then press a character key (カ is committed and new input begins).
                sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, 0, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "カ");
                sekka_free_string(output);

                let preedit = sekka_context_get_preedit(ctx);
                assert!(!preedit.is_null(), "keysym={:#x}", keysym);
                assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), expected_preedit);
                sekka_free_string(preedit);

                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "keysym={:#x}",
                    keysym
                );

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn other_non_character_keys_in_selection_mode_commit_and_forward() {
        // D-21: '1' (a digit) became a character key in D-20/D-21, so it was removed
        // from this list (the test
        // "other_character_keys_in_selection_mode_commit_and_continue_input" above
        // covers the character key path, digits included). BackSpace (0xFF08) was
        // removed by D-161 (Phase 9): it now has its own entry in the D-09 table
        // (see `backspace_in_selection_mode_closes_the_window_and_reverts_to_the_romaji`
        // below) instead of falling through to "other key" here.
        unsafe {
            for keysym in [0xFF53u32, 0xFF09] {
                let ctx = sekka_context_new();
                reselection_is_entered_for_ka_without_a_dictionary(ctx);
                // Advance to the next candidate and then press a non-character key (カ is committed and the key forwarded).
                sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, 0, 0);
                assert_eq!(consumed, 1);

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null());
                assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "カ");
                sekka_free_string(output);

                assert_eq!(sekka_context_take_forward_key(ctx), 1);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn an_alt_key_in_selection_mode_confirms_and_is_appended_without_alt() {
        // D-185 (table row 4): in the candidate window an Alt-held printable key is
        // not matched against D-09's table (`handle_selecting_key` treats Alt as an
        // other key, even for the next-candidate `n`), so it confirms the selected
        // candidate and is appended to it, without forwarding. Until Phase 11 an
        // Alt key here confirmed and was forwarded (D-162).
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);

            let consumed = sekka_context_process_key_event(ctx, b'n' as u32, MOD_ALT, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "かn");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(sekka_context_get_candidate_count(ctx), 0);
            assert_eq!(preedit_string(ctx), "");

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_printable_symbol_in_selection_mode_confirms_and_is_appended() {
        // D-158: a printable symbol during reselection is an "other key" (D-09)
        // that confirms the selected candidate, but instead of being forwarded
        // like `other_non_character_keys_in_selection_mode_commit_and_forward`
        // above, the symbol is appended to the confirmed candidate in a single
        // commit and the candidate window closes.
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);
            // Advance to the next candidate (カ) first.
            sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);

            let consumed = sekka_context_process_key_event(ctx, 0x28, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "カ(");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(sekka_context_get_candidate_count(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn alt_printable_keys_in_selection_mode_confirm_the_selected_candidate_and_are_appended() {
        // D-185 (table row 4) with D-09's table keys: in the candidate window an
        // Alt-held key is never matched against D-09's table (`handle_selecting_key`
        // returns false for Alt, even for Space, `n`, `q` and `j`), so `route_key`
        // confirms the selected candidate and `dispatch_input` appends the
        // character without Alt. One CommitString, no forward, window closed.
        for (keysym, ch) in [
            (b'd' as u32, "d"),
            (0x20u32, " "),
            (b'n' as u32, "n"),
            (b'q' as u32, "q"),
            (b'j' as u32, "j"),
        ] {
            unsafe {
                let ctx = sekka_context_new();
                reselection_is_entered_for_ka_without_a_dictionary(ctx);
                // Advance to the next candidate (カ) first.
                sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, MOD_ALT, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(
                    CStr::from_ptr(output).to_str().unwrap(),
                    format!("カ{}", ch),
                    "keysym={:#x}",
                    keysym
                );
                sekka_free_string(output);

                assert!(
                    sekka_context_poll_output(ctx).is_null(),
                    "a single commit only (keysym={:#x})",
                    keysym
                );
                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "keysym={:#x}",
                    keysym
                );
                assert_eq!(
                    sekka_context_get_candidate_count(ctx),
                    0,
                    "keysym={:#x}",
                    keysym
                );
                assert_eq!(preedit_string(ctx), "", "keysym={:#x}", keysym);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn an_alt_printable_key_in_the_commit_display_state_is_appended_to_the_word() {
        // D-185 (table row 3): in the commit display state the D-12 flush commits
        // the staged word and the Alt branch appends the character without Alt, in
        // a single CommitString; nothing is forwarded (D-155) and the staged word is
        // gone afterwards.
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, b'd' as u32, MOD_ALT, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "かd");
            sekka_free_string(output);

            assert!(
                sekka_context_poll_output(ctx).is_null(),
                "a single commit only"
            );
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(preedit_string(ctx), "");
            assert_eq!(
                sekka_context_trigger(ctx),
                0,
                "the staged word was consumed, so there is nothing left to reselect"
            );

            sekka_context_free(ctx);
        }
    }

    // === Exhaustive per-key-kind tests for the commit display state (VALIDATION.md Wave 0 Gap 3, Pitfall 1) ===

    /// Sends "K","a" and one Ctrl-J to reach make_commit_display_state (「か」, since
    /// there is no dictionary). Confirms poll_output is still NULL before returning
    /// the context (revised D-08).
    unsafe fn make_commit_display_state() -> *mut SekkaContextFfi {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(
                output.is_null(),
                "nothing should be committed right after Ctrl-J (revised D-08)"
            );
            ctx
        }
    }

    #[test]
    fn a_non_character_key_in_the_commit_display_state_commits_and_forwards() {
        unsafe {
            // Enter / Escape / Tab / Right arrow / Ctrl + 'a' (the non-character,
            // non-printable keys that can be pressed in the commit display state,
            // excluding Ctrl-J, the single exception of D-12). Space moved to
            // `a_space_in_the_commit_display_state_is_appended_to_the_word_and_not_forwarded`
            // with D-158 (Phase 9), since it is now a printable key that appends
            // instead of forwarding.
            for (keysym, modifiers) in [
                (0xFF0Du32, 0u32),
                (0xFF1Bu32, 0u32),
                (0xFF09u32, 0u32),
                (0xFF53u32, 0u32),
                (b'a' as u32, 0x4u32),
            ] {
                let ctx = make_commit_display_state();

                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(
                    consumed, 1,
                    "keysym={:#x} modifiers={:#x}",
                    keysym, modifiers
                );

                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
                sekka_free_string(output);

                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    1,
                    "keysym={:#x} should be forwarded to the application",
                    keysym
                );
                // The second take_forward_key is 0 (taking it resets the internal value).
                assert_eq!(sekka_context_take_forward_key(ctx), 0);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn a_space_in_the_commit_display_state_is_appended_to_the_word_and_not_forwarded() {
        // D-158 (Phase 9): Space in the commit display state is appended to the
        // displayed word ("か ") in a single commit and not forwarded (moved out of
        // `a_non_character_key_in_the_commit_display_state_commits_and_forwards`).
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, 0x20, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か ");
            sekka_free_string(output);

            // Confirm there is no double commit (the second poll is null).
            let output2 = sekka_context_poll_output(ctx);
            assert!(output2.is_null());

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_printable_symbol_in_the_commit_display_state_is_appended_to_the_word() {
        // D-158: a printable symbol other than Space (`(`) in the commit display
        // state is appended to the displayed word in a single commit, like
        // `a_space_in_the_commit_display_state_is_appended_to_the_word_and_not_forwarded`
        // above but with `(` instead of Space.
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, 0x28, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か(");
            sekka_free_string(output);

            // Confirm there is no double commit (the second poll is null).
            let output2 = sekka_context_poll_output(ctx);
            assert!(output2.is_null());

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn backspace_in_the_commit_display_state_reverts_to_the_romaji_and_does_not_forward() {
        // D-13 used to commit the word and forward BackSpace to the application;
        // D-160 (Phase 9) replaces that with reverting to the original romaji
        // ("Ka" minus its last character, see make_commit_display_state's doc
        // comment) instead, committing nothing and forwarding nothing. A further
        // BackSpace, once the romaji is exhausted, is unconsumed (0) and reaches
        // the application as an ordinary BackSpace (the outcome still differs from
        // BackSpace while there are characters in the romaji buffer, tested
        // separately by `backspace_deletes_one_romaji_character`).
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "K");
            sekka_free_string(preedit);

            let consumed = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(consumed, 1);
            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "");
            sekka_free_string(preedit);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let consumed = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(consumed, 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn backspace_in_selection_mode_closes_the_window_and_reverts_to_the_romaji() {
        // D-161: BackSpace during reselection has its own entry in the D-09 table
        // instead of falling through to "other key". It closes the candidate
        // window and reverts to the original romaji ("Ka" minus its last
        // character), matching
        // `backspace_in_the_commit_display_state_reverts_to_the_romaji_and_does_not_forward`
        // above (D-160).
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);
            // Advance to the next candidate (カ) first.
            sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);

            let consumed = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(sekka_context_get_candidate_count(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "K");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn backspace_then_retyping_after_conversion_converts_the_corrected_romaji() {
        // CONTEXT.md D-160's own example (`K a C-j BS i C-j`): after reverting to
        // the romaji with BackSpace, retyping the corrected letter and converting
        // again produces an ordinary conversion, with no leftover state from the
        // reverted word.
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(consumed, 1);

            let consumed = sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            let preedit = sekka_context_get_preedit(ctx);
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "き");
            sekka_free_string(preedit);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_character_key_in_the_commit_display_state_commits_and_starts_new_input() {
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, b'd' as u32, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
            sekka_free_string(output);

            // A character key is not forwarded (it becomes new romaji input).
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "d");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_symbol_only_buffer_is_raw_committed_by_space() {
        // D-27: to type a bare symbol, the existing rule that a non-character key
        // commits the romaji buffer as it is applies unchanged. No symbol-specific
        // escape hatch is added. D-158 (Phase 9): Space is now appended to that
        // commit (". ") instead of being forwarded as a separate key.
        unsafe {
            let ctx = sekka_context_new();

            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), ".");
            sekka_free_string(preedit);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let consumed = sekka_context_process_key_event(ctx, 0x20, 0, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), ". ");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_symbol_key_in_the_commit_display_state_commits_once_and_does_not_forward() {
        // D-28: pressing a symbol key in the commit display state commits the displayed
        // word exactly once, as D-12 says, and the key itself is not forwarded to the
        // application but becomes the first character of new romaji input.
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
            sekka_free_string(output);

            // Confirm there is no double commit (the second poll is null).
            let output2 = sekka_context_poll_output(ctx);
            assert!(
                output2.is_null(),
                "the same word must not be committed twice"
            );

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), ".");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_digit_key_in_the_commit_display_state_commits_once_and_does_not_forward() {
        // D-20/D-28: pressing a digit key in the commit display state behaves like a symbol
        // key: as D-12 says it commits the displayed word exactly once, and the key itself is
        // not forwarded to the application but becomes the first character of new romaji input.
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_process_key_event(ctx, b'1' as u32, 0, 0);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
            sekka_free_string(output);

            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "1");
            sekka_free_string(preedit);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn only_ctrl_j_does_not_commit_in_the_commit_display_state() {
        unsafe {
            let ctx = make_commit_display_state();

            // The single exception of D-12: Ctrl-J does not commit but enters reselection.
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            assert!(sekka_context_get_candidate_count(ctx) > 0);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn a_lone_modifier_key_does_not_commit_in_the_commit_display_state() {
        unsafe {
            for keysym in [0xFFE1u32, 0xFFE3u32] {
                let ctx = make_commit_display_state();

                let consumed = sekka_context_process_key_event(ctx, keysym, 0, 0);
                assert_eq!(consumed, 0, "keysym={:#x}", keysym);

                let output = sekka_context_poll_output(ctx);
                assert!(output.is_null());

                let preedit = sekka_context_get_preedit(ctx);
                assert!(!preedit.is_null());
                assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "か");
                sekka_free_string(preedit);

                assert_eq!(sekka_context_take_forward_key(ctx), 0);

                sekka_context_free(ctx);
            }
        }
    }

    #[test]
    fn the_word_in_the_commit_display_state_is_not_committed_twice() {
        unsafe {
            let ctx = make_commit_display_state();

            sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            let output1 = sekka_context_poll_output(ctx);
            assert!(!output1.is_null());
            assert_eq!(CStr::from_ptr(output1).to_str().unwrap(), "か");
            sekka_free_string(output1);

            // The second poll_output is NULL (the same word is not committed twice).
            let output2 = sekka_context_poll_output(ctx);
            assert!(output2.is_null());

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn confirm_candidate_commits_the_word_in_the_commit_display_state() {
        unsafe {
            let ctx = make_commit_display_state();

            // Pinning D-15 at the C ABI level: confirm_candidate works in the commit display state too.
            sekka_context_confirm_candidate(ctx);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
            sekka_free_string(output);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn confirm_candidate_during_reselection_commits_the_selected_candidate() {
        unsafe {
            let ctx = make_commit_display_state();

            // Enter reselection and move to the next candidate (without a dictionary, Ka gives か(0), カ(1), Ｋａ(2), Ka(3)).
            let entered = sekka_context_trigger(ctx);
            assert_eq!(entered, 1);
            let next = sekka_context_process_key_event(ctx, b'j' as u32, 0x4, 0);
            assert_eq!(next, 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), 1);

            sekka_context_confirm_candidate(ctx);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "カ");
            sekka_free_string(output);

            sekka_context_free(ctx);
        }
    }

    // === Learning wiring (layer 2 of D-41; a full C ABI round through a real UserDict) ===

    /// The set of dictionary handles for
    /// `committing_through_a_real_dictionary_puts_it_first_next_time`. The tempdir must
    /// stay alive until the test ends (dropping it first removes the directory and sled
    /// fails), so it is returned inside the struct.
    struct RealDictHandles {
        ctx: *mut SekkaContextFfi,
        master_dict: *mut SekkaDictionaryFfi,
        user_dict: *mut SekkaDictionaryFfi,
        _master_tmp: tempfile::TempDir,
        _user_tmp: tempfile::TempDir,
    }

    /// Combines a synthetic master dictionary (the immutable format built directly with
    /// `dict_format::write_dict`, the same practice as `create_test_dict` in
    /// `immutable_dict.rs`, transcribed here because that one is confined to its own
    /// `mod tests`) with a real `UserDict` in a tempdir, and passes them to
    /// `sekka_context_set_dictionaries` with the master first and the user dictionary
    /// second (an order that fails unless the reordering of D-39 works).
    unsafe fn context_with_real_dicts(entries: &[(&str, Vec<DictEntry>)]) -> RealDictHandles {
        unsafe {
            let master_tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let master_path = master_tmp.path().join("master_dict");
            {
                let mut map: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
                for (reading, dict_entries) in entries {
                    map.insert(reading.to_string(), dict_entries.clone());
                }
                dict_format::write_dict(&master_path, &map)
                    .expect("failed to create the test master dictionary");
            }

            let user_tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let user_path = user_tmp.path().join("user_dict");

            let master_path_cstr =
                CString::new(master_path.to_str().unwrap()).expect("failed to build the CString");
            let user_path_cstr =
                CString::new(user_path.to_str().unwrap()).expect("failed to build the CString");

            let master_dict = sekka_file_dict_new(master_path_cstr.as_ptr(), ptr::null());
            assert!(!master_dict.is_null());
            let user_dict = sekka_user_dict_new(user_path_cstr.as_ptr(), ptr::null());
            assert!(!user_dict.is_null());

            let ctx = sekka_context_new();
            assert!(!ctx.is_null());

            // Pass the master first and the user dictionary second (an order that fails
            // unless the reordering of D-39 works).
            let mut dicts = [master_dict, user_dict];
            sekka_context_set_dictionaries(ctx, dicts.as_mut_ptr(), 2);

            RealDictHandles {
                ctx,
                master_dict,
                user_dict,
                _master_tmp: master_tmp,
                _user_tmp: user_tmp,
            }
        }
    }

    #[test]
    fn committing_through_a_real_dictionary_puts_it_first_next_time() {
        unsafe {
            let entries = vec![
                DictEntry::new("．"),
                DictEntry::new("・"),
                DictEntry::new("。"),
                DictEntry::new("…"),
            ];
            let handles = context_with_real_dicts(&[(".", entries)]);
            let ctx = handles.ctx;

            // "." -> Ctrl-J enters the commit display state. Nothing is committed yet (revised D-08).
            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(
                output.is_null(),
                "nothing should be committed right after Ctrl-J (revised D-08)"
            );

            // Another Ctrl-J enters reselection and moves to the second candidate (「。」).
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            sekka_context_select_candidate(ctx, 2);

            let mut buf: [*mut c_char; 8] = [ptr::null_mut(); 8];
            let count = sekka_context_get_candidates(ctx, buf.as_mut_ptr(), 8, 0);
            assert!(count > 2, "count={}", count);
            assert_eq!(CStr::from_ptr(buf[2]).to_str().unwrap(), "。");
            sekka_free_candidate_list(buf.as_mut_ptr(), count);

            // confirm_candidate confirms and commits (delegated to finalize_staged; the
            // recording also runs here, D-34/D-15).
            sekka_context_confirm_candidate(ctx);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "。");
            sekka_free_string(output);

            // Then "." -> Ctrl-J again enters reselection and 「。」 is now first in the candidate list.
            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            let count2 = sekka_context_get_candidate_count(ctx);
            assert!(count2 > 0);
            let mut buf2: [*mut c_char; 8] = [ptr::null_mut(); 8];
            let got2 = sekka_context_get_candidates(ctx, buf2.as_mut_ptr(), 8, 0);
            assert!(got2 > 0);
            assert_eq!(
                CStr::from_ptr(buf2[0]).to_str().unwrap(),
                "。",
                "the learned word should come first"
            );
            sekka_free_candidate_list(buf2.as_mut_ptr(), got2);

            sekka_free_dictionary(handles.master_dict);
            sekka_free_dictionary(handles.user_dict);
            sekka_context_free(ctx);
        }
    }

    /// The set of dictionary handles for `a_full_round_succeeds_even_without_a_writable_dictionary` (master only).
    struct MasterOnlyHandles {
        ctx: *mut SekkaContextFfi,
        master_dict: *mut SekkaDictionaryFfi,
        _tmp: tempfile::TempDir,
    }

    unsafe fn master_only_context(entries: &[(&str, Vec<DictEntry>)]) -> MasterOnlyHandles {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let master_path = tmp.path().join("master_dict");
            {
                let mut map: BTreeMap<String, Vec<DictEntry>> = BTreeMap::new();
                for (reading, dict_entries) in entries {
                    map.insert(reading.to_string(), dict_entries.clone());
                }
                dict_format::write_dict(&master_path, &map)
                    .expect("failed to create the test master dictionary");
            }

            let master_path_cstr =
                CString::new(master_path.to_str().unwrap()).expect("failed to build the CString");
            let master_dict = sekka_file_dict_new(master_path_cstr.as_ptr(), ptr::null());
            assert!(!master_dict.is_null());

            let ctx = sekka_context_new();
            assert!(!ctx.is_null());

            let mut dicts = [master_dict];
            sekka_context_set_dictionaries(ctx, dicts.as_mut_ptr(), 1);

            MasterOnlyHandles {
                ctx,
                master_dict,
                _tmp: tmp,
            }
        }
    }

    /// The state guard of `sekka_context_select_candidate` (01.2-REVIEW WR-02)
    ///
    /// `next_candidate` / `prev_candidate` return without doing anything when
    /// `state != Selecting`, so without the guard a non-empty candidate list plus
    /// `state != Selecting` would loop forever. That combination does not currently cross
    /// the FFI boundary (`convert_and_stage` always empties the candidates before
    /// returning), so this test pins that the function **always returns and never changes
    /// the selection** in every state reachable from the public API. A red-first test for
    /// the guard itself cannot be written as long as the invariant cannot be broken from
    /// the public API.
    #[test]
    fn select_candidate_outside_reselection_changes_nothing() {
        unsafe {
            let entries = vec![
                DictEntry::new("．"),
                DictEntry::new("・"),
                DictEntry::new("。"),
            ];
            let handles = context_with_real_dicts(&[(".", entries)]);
            let ctx = handles.ctx;

            // During input (Input): there are no candidates.
            assert_eq!(sekka_context_get_candidate_index(ctx), -1);
            sekka_context_select_candidate(ctx, 0);
            sekka_context_select_candidate(ctx, 2);
            assert_eq!(
                sekka_context_get_candidate_index(ctx),
                -1,
                "select_candidate during input creates no selection"
            );

            // Commit display state ("." -> Ctrl-J; the candidates moved into last_commit and candidates is empty).
            assert_eq!(sekka_context_process_key_event(ctx, b'.' as u32, 0, 0), 1);
            assert_eq!(sekka_context_trigger(ctx), 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), -1);
            sekka_context_select_candidate(ctx, 0);
            sekka_context_select_candidate(ctx, 2);
            assert_eq!(
                sekka_context_get_candidate_index(ctx),
                -1,
                "select_candidate in the commit display state creates no selection"
            );

            // During reselection (Selecting): the only state where it does anything.
            assert_eq!(sekka_context_trigger(ctx), 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);
            sekka_context_select_candidate(ctx, 2);
            assert_eq!(sekka_context_get_candidate_index(ctx), 2);

            // An out-of-range index does not change the selection (and does not even enter the loop).
            let count = sekka_context_get_candidate_count(ctx);
            sekka_context_select_candidate(ctx, count);
            sekka_context_select_candidate(ctx, count + 1000);
            assert_eq!(
                sekka_context_get_candidate_index(ctx),
                2,
                "an out-of-range index does not change the selection"
            );

            // The direction back to the start (prev) also stops after the prescribed number of steps.
            sekka_context_select_candidate(ctx, 0);
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);
        }
    }

    #[test]
    fn a_full_round_succeeds_even_without_a_writable_dictionary() {
        // The FFI-side check for D-35/D-37: even with no writable dictionary (UserDict)
        // passed at all, the full round "." -> Ctrl-J -> reselection -> confirm does not
        // crash and 「。」 is committed. Being unable to record does not block input.
        unsafe {
            let entries = vec![
                DictEntry::new("．"),
                DictEntry::new("・"),
                DictEntry::new("。"),
                DictEntry::new("…"),
            ];
            let handles = master_only_context(&[(".", entries)]);
            let ctx = handles.ctx;

            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            assert!(
                sekka_context_poll_output(ctx).is_null(),
                "nothing should be committed right after Ctrl-J (revised D-08)"
            );

            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            sekka_context_select_candidate(ctx, 2);

            sekka_context_confirm_candidate(ctx);
            let output = sekka_context_poll_output(ctx);
            assert!(
                !output.is_null(),
                "committing should succeed even without a writable dictionary (D-35/D-37)"
            );
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "。");
            sekka_free_string(output);

            sekka_free_dictionary(handles.master_dict);
            sekka_context_free(ctx);
        }
    }

    // === D-64/SC7: the count/free contract of sekka_context_get_candidates (folded todo) ===

    #[test]
    fn a_candidate_with_a_nul_byte_keeps_return_value_and_free_range_consistent() {
        unsafe {
            let entries = vec![
                DictEntry::new("候補1"),
                DictEntry::new("候補2"),
                DictEntry::new("候補3\u{0}"),
                DictEntry::new("候補4"),
            ];
            let handles = master_only_context(&[(".", entries)]);
            let ctx = handles.ctx;

            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            // The symbol branch (build_symbol_candidates) appends two full-width /
            // half-width alphabet candidates after the dictionary entries (D-05), so the
            // total is not fixed to the number of dictionary entries. The actual total is
            // taken from `get_candidate_count`, and only the order of the dictionary
            // entries (index 2 holds the NUL byte, index 3 holds 「候補4」) is pinned.
            let expected_total = sekka_context_get_candidate_count(ctx);
            assert!(
                expected_total >= 4,
                "the 4 dictionary entries should at least be among the candidates: {}",
                expected_total
            );

            let mut buf: [*mut c_char; 16] = [ptr::null_mut(); 16];
            let count = sekka_context_get_candidates(ctx, buf.as_mut_ptr(), 16, 0);

            assert_eq!(
                count, expected_total,
                "the number of written slots (including NULLs) should be returned"
            );
            assert!(
                buf[2].is_null(),
                "the slot of a candidate containing a NUL byte should be NULL"
            );
            assert!(
                !buf[3].is_null(),
                "slots after the NUL slot should still be written"
            );
            assert_eq!(CStr::from_ptr(buf[3]).to_str().unwrap(), "候補4");

            // Freeing exactly the range the return value describes does not crash (which is
            // itself the check that no allocated pointer after the NUL slot is left behind).
            sekka_free_candidate_list(buf.as_mut_ptr(), count);

            sekka_free_dictionary(handles.master_dict);
            sekka_context_free(ctx);
        }
    }

    #[test]
    fn an_offset_at_or_past_the_candidate_count_returns_0() {
        unsafe {
            let entries = vec![DictEntry::new("候補1"), DictEntry::new("候補2")];
            let handles = master_only_context(&[(".", entries)]);
            let ctx = handles.ctx;

            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            let total = sekka_context_get_candidate_count(ctx);
            let mut buf: [*mut c_char; 8] = [ptr::null_mut(); 8];
            let count = sekka_context_get_candidates(ctx, buf.as_mut_ptr(), 8, total);
            assert_eq!(count, 0);

            sekka_free_dictionary(handles.master_dict);
            sekka_context_free(ctx);
        }
    }

    #[test]
    fn max_count_larger_than_the_candidate_count_returns_only_written_slots() {
        unsafe {
            let entries = vec![DictEntry::new("候補1"), DictEntry::new("候補2")];
            let handles = master_only_context(&[(".", entries)]);
            let ctx = handles.ctx;

            let consumed = sekka_context_process_key_event(ctx, b'.' as u32, 0, 0);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            let total = sekka_context_get_candidate_count(ctx);
            let mut buf: [*mut c_char; 32] = [ptr::null_mut(); 32];
            let count = sekka_context_get_candidates(ctx, buf.as_mut_ptr(), 32, 0);
            assert_eq!(
                count, total,
                "the rest of the array is untouched and only the number of written slots is returned"
            );
            sekka_free_candidate_list(buf.as_mut_ptr(), count);

            sekka_free_dictionary(handles.master_dict);
            sekka_context_free(ctx);
        }
    }

    // === SC5: measuring the keystroke count for current / before learning / after learning (01.4-03 Task 1) ===

    /// Calls `sekka_context_process_key_event` while counting the presses.
    unsafe fn press_and_count(
        ctx: *mut SekkaContextFfi,
        keysym: u32,
        modifiers: u32,
        count: &mut u32,
    ) -> c_int {
        *count += 1;
        unsafe { sekka_context_process_key_event(ctx, keysym, modifiers, 0) }
    }

    /// Calls `sekka_context_trigger` while counting the presses (D-110: a trigger press
    /// delegates to the argument-less API, but the user still makes one keystroke, so it
    /// counts as one).
    unsafe fn press_trigger_and_count(ctx: *mut SekkaContextFfi, count: &mut u32) -> c_int {
        *count += 1;
        unsafe { sekka_context_trigger(ctx) }
    }

    /// Types romaji character keys one at a time (asserting that each key is consumed).
    unsafe fn type_word(ctx: *mut SekkaContextFfi, word: &str, count: &mut u32) {
        for ch in word.chars() {
            let consumed = unsafe { press_and_count(ctx, ch as u32, 0, count) };
            assert_eq!(consumed, 1, "the character key {:?} should be consumed", ch);
        }
    }

    /// Takes one `sekka_context_poll_output` and returns it as a `String` (`None` for NULL).
    unsafe fn take_output(ctx: *mut SekkaContextFfi) -> Option<String> {
        unsafe {
            let ptr = sekka_context_poll_output(ctx);
            if ptr.is_null() {
                None
            } else {
                let s = CStr::from_ptr(ptr).to_str().unwrap().to_string();
                sekka_free_string(ptr);
                Some(s)
            }
        }
    }

    /// `measures_how_learning_changes_the_keystroke_count`
    ///
    /// SC5 (the Deferred Follow-Ups of test 4 in `01.3-UAT.md`, addressing finding 3 of
    /// `01.4-RESEARCH.md`): the numbers written in the three keystroke rows of D-25 in
    /// `01.3-CONTEXT.md` (current / before learning / after learning) are the values
    /// **this function** obtained by actually sending key events across the C ABI
    /// (`sekka_context_process_key_event`) and counting them. Whenever the numbers are in
    /// doubt, re-read this test and re-run it to reproduce them (primary evidence, so we
    /// do not repeat the failure of the old D-25, which was calculated on paper).
    ///
    /// The candidate lists for the example word 「漢字」 (reading 「かんじ」) and the
    /// symbol 「.」 are hardcoded as seed values straight from real data obtained by
    /// inspecting the built master dictionary (`share/sekka/master-dict.db`, gitignored)
    /// with `cargo run --example inspect-dict -- . かんじ` (measured 2026-09-22). The test
    /// itself never reads any build-output path, so it runs in an environment without
    /// build outputs.
    ///
    /// Because of the upstream behaviour where Enter during candidate selection only
    /// commits and sends no newline (`emacs/sekka.el:137`), the before- and after-learning
    /// sequences need Enter twice (the first only commits, the second flushes and forwards
    /// the newline). The current sequence never enters reselection, so a single Enter both
    /// commits 「.」 raw and forwards the newline.
    #[test]
    fn measures_how_learning_changes_the_keystroke_count() {
        // Real data from the built master dictionary (share/sekka/master-dict.db)
        // (2026-09-22, the stdout of `cargo run --example inspect-dict -- . かんじ`
        // transcribed as it is, keys in the order the real data stores them):
        //   .      -> [{"word":"．"},{"word":"・"},{"word":"。"},{"word":"…","annotation":"..."}]
        //   かんじ  -> [{"word":"漢字"},{"word":"幹事","annotation":"manager"},
        //               {"word":"監事","annotation":"inspector"},{"word":"感じ"},
        //               {"word":"寛治","annotation":"年号(1087-1094)"},
        //               {"word":"莞爾","annotation":"にっこり。「-と笑う」"},
        //               {"word":"完爾","annotation":"人名"},{"word":"完治","annotation":"かんち"},
        //               {"word":"官寺","annotation":"⇔私寺"},{"word":"換字"},
        //               {"word":"冠辞","annotation":"枕言葉"},{"word":"完児","annotation":"人名"}] (12 entries)
        let dot_entries = || {
            vec![
                DictEntry::new("．"),
                DictEntry::new("・"),
                DictEntry::new("。"),
                DictEntry::new("…").with_annotation("..."),
            ]
        };
        let kanji_entries = || {
            vec![
                DictEntry::new("漢字"),
                DictEntry::new("幹事").with_annotation("manager"),
                DictEntry::new("監事").with_annotation("inspector"),
                DictEntry::new("感じ"),
                DictEntry::new("寛治").with_annotation("年号(1087-1094)"),
                DictEntry::new("莞爾").with_annotation("にっこり。「-と笑う」"),
                DictEntry::new("完爾").with_annotation("人名"),
                DictEntry::new("完治").with_annotation("かんち"),
                DictEntry::new("官寺").with_annotation("⇔私寺"),
                DictEntry::new("換字"),
                DictEntry::new("冠辞").with_annotation("枕言葉"),
                DictEntry::new("完児").with_annotation("人名"),
            ]
        };

        const ENTER: u32 = 0xFF0D;
        const CTRL_J: u32 = b'j' as u32;

        unsafe {
            // === Current: no kuten conversion, so 「漢字.」 with a half-width dot plus a newline ===
            let mut count_gendai: u32 = 0;
            let handles =
                context_with_real_dicts(&[(".", dot_entries()), ("かんじ", kanji_entries())]);
            let ctx = handles.ctx;

            type_word(ctx, "Kanji", &mut count_gendai);

            let consumed = press_trigger_and_count(ctx, &mut count_gendai);
            assert_eq!(consumed, 1, "Ctrl-J should stage 漢字");

            let consumed = press_and_count(ctx, b'.' as u32, 0, &mut count_gendai);
            assert_eq!(
                consumed, 1,
                "「.」 should flush the staged 漢字 and land in the buffer"
            );
            let word_output = take_output(ctx).expect("漢字 should be flushed");
            assert_eq!(word_output, "漢字");

            let consumed = press_and_count(ctx, ENTER, 0, &mut count_gendai);
            assert_eq!(consumed, 1, "Enter should commit 「.」 raw (D-27/D-28)");
            let dot_output = take_output(ctx).expect("「.」 should be committed raw");
            assert_eq!(dot_output, ".");
            assert_eq!(
                sekka_context_take_forward_key(ctx),
                1,
                "in the current sequence this Enter should be forwarded to the application as a newline"
            );

            assert_eq!(
                count_gendai, 8,
                "current: 「Kanji」 (5 keys) + Ctrl-J(1) + .(1) + Enter(1) = 8 keystrokes"
            );

            sekka_free_dictionary(handles.master_dict);
            sekka_free_dictionary(handles.user_dict);
            sekka_context_free(ctx);

            // === Before learning -> after learning: two rounds on the same context and the same dictionaries ===
            // (the after-learning round depends on the state the before-learning round
            // actually learned, so it cannot use an independent context. The current round
            // above is an independent context unrelated to these two.)
            let mut count_mae: u32 = 0;
            let handles =
                context_with_real_dicts(&[(".", dot_entries()), ("かんじ", kanji_entries())]);
            let ctx = handles.ctx;

            // --- Before learning: 「漢字。」 plus a newline with an empty user dictionary ---
            type_word(ctx, "Kanji", &mut count_mae);

            let consumed = press_trigger_and_count(ctx, &mut count_mae);
            assert_eq!(consumed, 1, "Ctrl-J should stage 漢字");

            let consumed = press_and_count(ctx, b'.' as u32, 0, &mut count_mae);
            assert_eq!(
                consumed, 1,
                "「.」 should flush the staged 漢字 and land in the buffer"
            );
            let word_output = take_output(ctx).expect("漢字 should be flushed");
            assert_eq!(word_output, "漢字");

            let consumed = press_trigger_and_count(ctx, &mut count_mae);
            assert_eq!(
                consumed, 1,
                "Ctrl-J should stage the candidates of 「.」 (before learning the first one is 「．」)"
            );

            let consumed = press_trigger_and_count(ctx, &mut count_mae);
            assert_eq!(
                consumed, 1,
                "Ctrl-J should enter reselection (begin_reselect)"
            );
            assert_eq!(
                sekka_context_get_candidate_index(ctx),
                0,
                "right after reselection starts the first candidate 「．」 (index=0) should be selected"
            );

            let consumed = press_and_count(ctx, CTRL_J, MOD_CTRL, &mut count_mae);
            assert_eq!(
                consumed, 1,
                "the first next_candidate moves to 「・」 (index=1)"
            );
            let consumed = press_and_count(ctx, CTRL_J, MOD_CTRL, &mut count_mae);
            assert_eq!(
                consumed, 1,
                "the second next_candidate moves to 「。」 (index=2)"
            );
            assert_eq!(
                sekka_context_get_candidate_index(ctx),
                2,
                "「。」 should be the third entry (index=2) of the measured candidate list"
            );

            let consumed = press_and_count(ctx, ENTER, 0, &mut count_mae);
            assert_eq!(
                consumed, 1,
                "the first Enter only commits (emacs/sekka.el:137; no newline is sent)"
            );
            assert!(
                take_output(ctx).is_none(),
                "nothing should be flushed right after the first Enter (revised D-08)"
            );
            assert_eq!(
                sekka_context_take_forward_key(ctx),
                0,
                "the first Enter should not forward a newline"
            );

            let consumed = press_and_count(ctx, ENTER, 0, &mut count_mae);
            assert_eq!(
                consumed, 1,
                "the second Enter should flush 「。」 and forward a newline to the application"
            );
            let dot_output = take_output(ctx).expect("「。」 should be flushed");
            assert_eq!(dot_output, "。");
            assert_eq!(
                sekka_context_take_forward_key(ctx),
                1,
                "the second Enter should forward a newline to the application"
            );

            assert_eq!(
                count_mae, 13,
                "before learning: 「Kanji」 (5) + Ctrl-J(1) + .(1) + Ctrl-J(1) + Ctrl-J(1) \
                 + Ctrl-J x2 (2) + Enter x2 (2) = 13 keystrokes"
            );

            // --- After learning: another round on the same context (「。」 has been learned) ---
            let mut count_ato: u32 = 0;

            type_word(ctx, "Kanji", &mut count_ato);

            let consumed = press_trigger_and_count(ctx, &mut count_ato);
            assert_eq!(consumed, 1, "Ctrl-J should stage 漢字");

            let consumed = press_and_count(ctx, b'.' as u32, 0, &mut count_ato);
            assert_eq!(
                consumed, 1,
                "「.」 should flush the staged 漢字 and land in the buffer"
            );
            let word_output = take_output(ctx).expect("漢字 should be flushed");
            assert_eq!(word_output, "漢字");

            let consumed = press_trigger_and_count(ctx, &mut count_ato);
            assert_eq!(
                consumed, 1,
                "Ctrl-J stages the candidates of 「.」 (learning should have moved 「。」 to the front)"
            );

            let consumed = press_trigger_and_count(ctx, &mut count_ato);
            assert_eq!(
                consumed, 1,
                "Ctrl-J should enter reselection (begin_reselect)"
            );
            assert_eq!(
                sekka_context_get_candidate_index(ctx),
                0,
                "right after reselection starts the learned 「。」 should already be first (index=0) and selected"
            );

            let mut buf: [*mut c_char; 8] = [ptr::null_mut(); 8];
            let got = sekka_context_get_candidates(ctx, buf.as_mut_ptr(), 8, 0);
            assert!(got > 0, "got={}", got);
            assert_eq!(
                CStr::from_ptr(buf[0]).to_str().unwrap(),
                "。",
                "the learned 「。」 should come first in the candidate list"
            );
            sekka_free_candidate_list(buf.as_mut_ptr(), got);

            let consumed = press_and_count(ctx, ENTER, 0, &mut count_ato);
            assert_eq!(
                consumed, 1,
                "the first Enter only commits (emacs/sekka.el:137; no newline is sent)"
            );
            assert!(
                take_output(ctx).is_none(),
                "nothing should be flushed right after the first Enter (revised D-08)"
            );
            assert_eq!(
                sekka_context_take_forward_key(ctx),
                0,
                "the first Enter should not forward a newline"
            );

            let consumed = press_and_count(ctx, ENTER, 0, &mut count_ato);
            assert_eq!(
                consumed, 1,
                "the second Enter should flush 「。」 and forward a newline to the application"
            );
            let dot_output = take_output(ctx).expect("「。」 should be flushed");
            assert_eq!(dot_output, "。");
            assert_eq!(
                sekka_context_take_forward_key(ctx),
                1,
                "the second Enter should forward a newline to the application"
            );

            assert_eq!(
                count_ato, 11,
                "after learning: 「Kanji」 (5) + Ctrl-J(1) + .(1) + Ctrl-J(1) + Ctrl-J(1) + Enter x2 (2) \
                 = 11 keystrokes (「。」 is already first, so the two Ctrl-J presses for next_candidate are unnecessary)"
            );

            sekka_free_dictionary(handles.master_dict);
            sekka_free_dictionary(handles.user_dict);
            sekka_context_free(ctx);
        }
    }

    // === D-102: sekka_context_save_dictionaries / sekka_dictionary_save ===

    #[test]
    fn save_dictionaries_returns_nonzero_for_null() {
        unsafe {
            let result = sekka_context_save_dictionaries(ptr::null_mut());
            assert_ne!(result, 0);
        }
    }

    #[test]
    fn save_dictionaries_returns_0_without_dictionaries() {
        unsafe {
            let ctx = sekka_context_new();
            let result = sekka_context_save_dictionaries(ctx);
            assert_eq!(result, 0);
            sekka_context_free(ctx);
        }
    }

    #[test]
    fn save_dictionaries_returns_0_with_a_user_dictionary_loaded() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let dict_path = tmp.path().join("save_test_user_dict");
            let path_cstr =
                CString::new(dict_path.to_str().unwrap()).expect("failed to build the CString");
            let mut dict = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(!dict.is_null());

            let ctx = sekka_context_new();
            sekka_context_set_dictionaries(ctx, &mut dict, 1);

            let result = sekka_context_save_dictionaries(ctx);
            assert_eq!(result, 0);

            sekka_free_dictionary(dict);
            sekka_context_free(ctx);
        }
    }

    #[test]
    fn dictionary_save_returns_nonzero_for_null() {
        unsafe {
            let result = sekka_dictionary_save(ptr::null_mut());
            assert_ne!(result, 0);
        }
    }

    #[test]
    fn dictionary_save_returns_0_for_a_user_dictionary_handle() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let dict_path = tmp.path().join("dict_save_user_dict");
            let path_cstr =
                CString::new(dict_path.to_str().unwrap()).expect("failed to build the CString");
            let dict = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(!dict.is_null());

            let result = sekka_dictionary_save(dict);
            assert_eq!(result, 0);

            sekka_free_dictionary(dict);
        }
    }

    // === D-110: sekka_context_trigger (the three state branches) ===

    #[test]
    fn trigger_returns_0_for_null() {
        unsafe {
            assert_eq!(sekka_context_trigger(ptr::null_mut()), 0);
        }
    }

    #[test]
    fn trigger_converts_the_romaji_buffer_in_the_initial_state() {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);

            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);

            let preedit = sekka_context_get_preedit(ctx);
            assert!(!preedit.is_null());
            assert_eq!(CStr::from_ptr(preedit).to_str().unwrap(), "か");
            sekka_free_string(preedit);

            // As with the old Ctrl-J, nothing is committed yet (revised D-08).
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn trigger_enters_reselection_from_the_commit_display_state() {
        unsafe {
            let ctx = make_commit_display_state();

            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            assert!(
                sekka_context_get_candidate_count(ctx) >= 2,
                "reselection should offer at least 2 candidates"
            );
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn trigger_advances_to_the_next_candidate_during_reselection() {
        unsafe {
            let ctx = make_commit_display_state();

            assert_eq!(sekka_context_trigger(ctx), 1, "reselection should start");
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);

            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 1);
            assert_eq!(
                sekka_context_get_candidate_index(ctx),
                1,
                "triggering again during reselection should advance to the next candidate (D-109)"
            );

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn trigger_returns_0_with_no_buffer_and_no_last_commit() {
        unsafe {
            let ctx = sekka_context_new();

            let consumed = sekka_context_trigger(ctx);
            assert_eq!(consumed, 0);
            assert_eq!(sekka_context_get_candidate_count(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    #[test]
    fn trigger_does_not_carry_over_the_forward_flag() {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'k' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            // Enter commits the romaji as it is and forwards it (D-03), so forward_key becomes true.
            let consumed = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(consumed, 1);

            // Call trigger without having called take_forward_key.
            let _ = sekka_context_trigger(ctx);
            assert_eq!(
                sekka_context_take_forward_key(ctx),
                0,
                "trigger carried over the forward flag from the previous event"
            );

            sekka_context_free(ctx);
        }
    }

    // === Word registration (D-167/D-170/D-172/D-182/REG-07, Phase 10) ===

    /// The tracer's own key-by-key sequence through the C ABI: `S e k k a`
    /// C-j C-r `S e k i` C-j `K a` C-j Enter registers "せきか" under the
    /// reading "せっか" and commits it; the next `S e k k a` C-j offers the
    /// registered word first (REG-07).
    #[test]
    fn registration_entry_registers_the_word_and_commits_it_through_the_c_abi() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let dict_path = tmp.path().join("test_user_dict");
            let path_cstr =
                CString::new(dict_path.to_str().unwrap()).expect("failed to build the CString");
            let mut dict = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(!dict.is_null());

            let ctx = sekka_context_new();
            sekka_context_set_dictionaries(ctx, &mut dict, 1);

            for ch in "Sekka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            let staged = sekka_context_trigger(ctx);
            assert_eq!(staged, 1);
            let staged_output = sekka_context_poll_output(ctx);
            assert!(
                staged_output.is_null(),
                "nothing should be committed right after Ctrl-J"
            );

            // C-r ('r', Ctrl = 0x4).
            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            let entry_output = sekka_context_poll_output(ctx);
            assert!(entry_output.is_null());
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert_eq!(reg_reading_string(ctx), "せっか");
            assert_eq!(reg_prompt_string(ctx), "登録 ");
            assert_eq!(preedit_string(ctx), "");

            for ch in "Seki".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            assert_eq!(preedit_string(ctx), "せき");

            for ch in "Ka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            assert_eq!(preedit_string(ctx), "せきKa");
            let mid_output = sekka_context_poll_output(ctx);
            assert!(mid_output.is_null());

            sekka_context_trigger(ctx);
            assert_eq!(preedit_string(ctx), "せきか");

            let finished = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "せきか");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(sekka_context_is_registering(ctx), 0);

            for ch in "Sekka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            assert_eq!(
                preedit_string(ctx),
                "せきか",
                "the word registered under せっか should be the first candidate (REG-07)"
            );

            sekka_context_free(ctx);
            sekka_free_dictionary(dict);
        }
    }

    /// Helper: returns `sekka_context_get_registration_reading` as an owned `String`.
    unsafe fn reg_reading_string(ctx: *mut SekkaContextFfi) -> String {
        unsafe {
            let reading = sekka_context_get_registration_reading(ctx);
            assert!(!reading.is_null());
            let s = CStr::from_ptr(reading).to_str().unwrap().to_string();
            sekka_free_string(reading);
            s
        }
    }

    /// Helper: returns `sekka_context_get_registration_prompt` as an owned `String`.
    unsafe fn reg_prompt_string(ctx: *mut SekkaContextFfi) -> String {
        unsafe {
            let prompt = sekka_context_get_registration_prompt(ctx);
            assert!(!prompt.is_null());
            let s = CStr::from_ptr(prompt).to_str().unwrap().to_string();
            sekka_free_string(prompt);
            s
        }
    }

    #[test]
    fn registration_getters_are_null_safe_and_empty_outside_registration() {
        unsafe {
            assert_eq!(sekka_context_is_registering(ptr::null_mut()), 0);
            assert!(sekka_context_get_registration_reading(ptr::null_mut()).is_null());
            assert!(sekka_context_get_registration_prompt(ptr::null_mut()).is_null());

            let ctx = sekka_context_new();
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(reg_reading_string(ctx), "");
            assert_eq!(reg_prompt_string(ctx), "");

            sekka_context_free(ctx);
        }
    }

    // === D-167/D-169/D-174 entry and cancel through the C ABI (Phase 10) ===

    /// D-167: Ctrl-R also enters registration from inside the reselection
    /// window, and captures the typed reading (D-168) - not the display of
    /// whichever candidate happened to be selected.
    #[test]
    fn ctrl_r_in_selection_mode_enters_registration() {
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);
            // Advance to the second candidate (カ) before pressing Ctrl-R.
            sekka_context_trigger(ctx);
            assert_eq!(preedit_string(ctx), "カ");
            assert_eq!(sekka_context_get_candidate_index(ctx), 1);

            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert_eq!(
                reg_reading_string(ctx),
                "か",
                "the typed reading, not the selected candidate's display"
            );

            let consumed = sekka_context_process_key_event(ctx, 0xFF1Bu32, 0, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(sekka_context_get_candidate_count(ctx), 4);
            assert_eq!(sekka_context_get_candidate_index(ctx), 1);
            assert_eq!(preedit_string(ctx), "カ");
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());

            sekka_context_free(ctx);
        }
    }

    /// D-169: Ctrl-R on a shape `begin_registration` refuses (okuri-ari here)
    /// is consumed and changes nothing, whether pressed from the commit
    /// display state or from inside the candidate window.
    #[test]
    fn ctrl_r_on_a_refused_shape_is_consumed_and_changes_nothing() {
        unsafe {
            let ctx = sekka_context_new();
            for ch in "KaKu".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            let preedit_before = preedit_string(ctx);
            assert!(!preedit_before.is_empty());

            let consumed = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(preedit_string(ctx), preedit_before);

            sekka_context_trigger(ctx);
            let count_before = sekka_context_get_candidate_count(ctx);
            assert!(count_before > 0);

            let consumed = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(sekka_context_get_candidate_count(ctx), count_before);

            sekka_context_free(ctx);
        }
    }

    /// D-167 / D-03: Ctrl-R while romaji is still being typed (no staged
    /// candidate) keeps its old meaning unchanged - commit the romaji as it
    /// is and forward the key. Ctrl-R with nothing at all typed is
    /// unconsumed, same as any other Ctrl+letter with an empty buffer.
    #[test]
    fn ctrl_r_during_romaji_input_still_commits_and_forwards() {
        unsafe {
            let ctx = sekka_context_new();
            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);

            let consumed = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "Ka");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 1);
            assert_eq!(sekka_context_is_registering(ctx), 0);

            sekka_context_free(ctx);

            let ctx = sekka_context_new();
            let consumed = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(consumed, 0);
            sekka_context_free(ctx);
        }
    }

    /// D-174 / D-179: Esc and Ctrl-G cancel registration from the commit
    /// display state (no inner candidate window open) and return exactly to
    /// the staged candidate as if Ctrl-R had never been pressed - the outer
    /// step can still be committed normally afterward.
    #[test]
    fn escape_and_ctrl_g_cancel_registration_back_to_the_staged_candidate() {
        unsafe {
            for (keysym, modifiers) in [(0xFF1Bu32, 0u32), (b'g' as u32, 0x4u32)] {
                let ctx = make_commit_display_state();
                let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
                assert_eq!(entered, 1, "keysym={:#x}", keysym);

                sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
                sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);

                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);
                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "keysym={:#x}",
                    keysym
                );
                let output = sekka_context_poll_output(ctx);
                assert!(output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(sekka_context_is_registering(ctx), 0, "keysym={:#x}", keysym);
                assert_eq!(preedit_string(ctx), "か", "keysym={:#x}", keysym);

                // D-162: outside registration the step continues exactly as before.
                let finished = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
                assert_eq!(finished, 1, "keysym={:#x}", keysym);
                let output = sekka_context_poll_output(ctx);
                assert!(!output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "か");
                sekka_free_string(output);
                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    1,
                    "keysym={:#x}",
                    keysym
                );

                sekka_context_free(ctx);
            }
        }
    }

    /// D-174: Esc, `q` and Ctrl-G inside the *inner* candidate window only
    /// close that window (D-09) and leave the registration session itself
    /// in place - the word being assembled reverts to what was staged
    /// before the window was entered.
    #[test]
    fn escape_q_and_ctrl_g_in_the_inner_window_only_close_the_window() {
        unsafe {
            for (keysym, modifiers) in [
                (0xFF1Bu32, 0u32),
                (b'q' as u32, 0u32),
                (b'g' as u32, 0x4u32),
            ] {
                let ctx = make_commit_display_state();
                sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

                sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
                sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
                sekka_context_trigger(ctx); // stage か
                sekka_context_trigger(ctx); // enter the inner candidate window
                sekka_context_trigger(ctx); // advance to カ

                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);
                let output = sekka_context_poll_output(ctx);
                assert!(output.is_null(), "keysym={:#x}", keysym);
                assert_eq!(sekka_context_is_registering(ctx), 1, "keysym={:#x}", keysym);
                assert_eq!(preedit_string(ctx), "か", "keysym={:#x}", keysym);

                sekka_context_free(ctx);
            }
        }
    }

    /// REG-05: Enter with nothing ever typed into the registration step is
    /// consumed but changes nothing - registration stays active, nothing is
    /// committed and the outer reading is unchanged.
    #[test]
    fn enter_with_an_empty_word_is_consumed_and_changes_nothing() {
        unsafe {
            let ctx = make_commit_display_state();
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            let consumed = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(consumed, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert_eq!(reg_reading_string(ctx), "か");
            assert_eq!(preedit_string(ctx), "");

            sekka_context_free(ctx);
        }
    }

    // === REG-02 candidate delegation and Enter's 3 inner states (D-116/D-172/D-176, Phase 10) ===

    /// REG-02 / D-116 / D-173: the candidate getters, `select_candidate` and
    /// `confirm_candidate` all follow the innermost registration step. The
    /// frozen outer step's own candidates are never visible while
    /// registering, and a click-style confirm (the same call the C++ side's
    /// `SekkaCandidateList::selectAt` makes) goes into the word being
    /// assembled instead of the application.
    #[test]
    fn candidate_getters_follow_the_active_step_during_registration() {
        unsafe {
            let ctx = sekka_context_new();
            reselection_is_entered_for_ka_without_a_dictionary(ctx);
            assert_eq!(sekka_context_get_candidate_count(ctx), 4);

            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);
            assert_eq!(
                sekka_context_get_candidate_count(ctx),
                0,
                "the frozen outer candidates must not be visible while registering"
            );

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx); // stage き
            sekka_context_trigger(ctx); // enter the inner candidate window

            assert_eq!(sekka_context_get_candidate_count(ctx), 4);
            let mut buf: [*mut c_char; 8] = [ptr::null_mut(); 8];
            let count = sekka_context_get_candidates(ctx, buf.as_mut_ptr(), 8, 0);
            assert!(count > 0);
            assert_eq!(CStr::from_ptr(buf[0]).to_str().unwrap(), "き");
            sekka_free_candidate_list(buf.as_mut_ptr(), count);
            assert_eq!(sekka_context_get_candidate_index(ctx), 0);

            sekka_context_select_candidate(ctx, 1);
            assert_eq!(sekka_context_get_candidate_index(ctx), 1);
            assert_eq!(preedit_string(ctx), "キ");

            // The click path: select_candidate then confirm_candidate.
            sekka_context_confirm_candidate(ctx);
            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert_eq!(sekka_context_get_candidate_count(ctx), 0);
            assert_eq!(preedit_string(ctx), "キ");

            sekka_context_free(ctx);
        }
    }

    /// D-172: Enter from inside the inner candidate window registers
    /// whichever candidate was selected there (キ, reached with one more
    /// Ctrl-J than the first candidate き).
    #[test]
    fn enter_in_the_inner_window_registers_the_selected_candidate() {
        unsafe {
            let ctx = make_commit_display_state();
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx); // stage き
            sekka_context_trigger(ctx); // enter the inner candidate window
            sekka_context_trigger(ctx); // advance to キ
            assert_eq!(preedit_string(ctx), "キ");

            let finished = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "キ");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);
            assert_eq!(sekka_context_is_registering(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    /// D-172: Enter from the inner commit display state (only one Ctrl-J -
    /// no candidate window entered) registers the word shown there (き).
    #[test]
    fn enter_in_the_inner_commit_display_registers_the_shown_word() {
        unsafe {
            let ctx = make_commit_display_state();
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx); // stage き

            let finished = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "き");
            sekka_free_string(output);

            sekka_context_free(ctx);
        }
    }

    /// D-176 / REG-07: Enter with unconverted romaji left in the inner step
    /// registers the romaji as it is, and the registered word is found
    /// through the same reading afterward (still in the same process).
    #[test]
    fn enter_with_unconverted_romaji_registers_the_romaji_as_is() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let dict_path = tmp.path().join("test_user_dict");
            let path_cstr =
                CString::new(dict_path.to_str().unwrap()).expect("failed to build the CString");
            let mut dict = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(!dict.is_null());

            let ctx = sekka_context_new();
            sekka_context_set_dictionaries(ctx, &mut dict, 1);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            sekka_context_trigger(ctx);

            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);

            for ch in "Linux".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            assert_eq!(preedit_string(ctx), "Linux");

            let finished = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "Linux");
            sekka_free_string(output);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            for ch in "Ka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            assert_eq!(
                preedit_string(ctx),
                "Linux",
                "REG-07: the registered word should be the first candidate under か"
            );

            sekka_context_free(ctx);
            sekka_free_dictionary(dict);
        }
    }

    // === D-173/D-175/D-177/D-178 final key table, D-171 recursion (Phase 10, 10-03) ===

    /// D-175/D-160: BackSpace during registration first edits the innermost
    /// step exactly as it would outside registration (reverting the staged
    /// candidate to its original romaji, then editing the romaji buffer);
    /// once that step is exhausted, BackSpace deletes the last character of
    /// the word being assembled instead; once the word is empty too,
    /// BackSpace is consumed and changes nothing at all - the reading is
    /// never touched and nothing is forwarded.
    #[test]
    fn backspace_in_registration_edits_the_inner_step_then_the_word() {
        unsafe {
            let ctx = make_commit_display_state();
            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx);
            assert_eq!(preedit_string(ctx), "き");

            let bs = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(bs, 1);
            assert_eq!(preedit_string(ctx), "K");

            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx);
            assert_eq!(preedit_string(ctx), "き");

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            assert_eq!(preedit_string(ctx), "きKa");

            for expected in ["きK", "き", ""] {
                let bs = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
                assert_eq!(bs, 1, "expected={expected}");
                assert_eq!(preedit_string(ctx), expected, "expected={expected}");
                assert!(
                    sekka_context_poll_output(ctx).is_null(),
                    "expected={expected}"
                );
                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "expected={expected}"
                );
            }

            // The word is now empty too: BackSpace is consumed and changes
            // nothing at all (D-175 - the reading is never touched, nothing
            // is forwarded).
            let bs = sekka_context_process_key_event(ctx, 0xFF08, 0, 0);
            assert_eq!(bs, 1);
            assert_eq!(preedit_string(ctx), "");
            assert_eq!(reg_reading_string(ctx), "か");
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert!(sekka_context_poll_output(ctx).is_null());
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    /// D-177: keys that would reach the application outside registration
    /// (Tab, arrows, Home, End, Delete, F keys, Ctrl + a letter not in the
    /// D-09 table, and every Alt/Super-modified key) are consumed and
    /// change nothing during registration - not even the innermost step's
    /// own staged candidate, which stays uncommitted throughout.
    #[test]
    fn keys_forwarded_outside_registration_do_nothing_during_registration() {
        unsafe {
            let ctx = make_commit_display_state();
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx);
            assert_eq!(preedit_string(ctx), "き");

            for (keysym, modifiers) in [
                (0xFF09u32, 0u32),      // Tab
                (0xFF53u32, 0u32),      // Right
                (0xFF50u32, 0u32),      // Home
                (0xFF57u32, 0u32),      // End
                (0xFFFFu32, 0u32),      // Delete
                (0xFFBEu32, 0u32),      // F1
                (b'b' as u32, 0x4u32),  // Ctrl-B, not in the D-09 table
                (b'm' as u32, 0x4u32),  // Ctrl-M, outside the candidate window
                (b'a' as u32, 0x8u32),  // Alt-a
                (b'a' as u32, 0x40u32), // Super-a
            ] {
                let consumed = sekka_context_process_key_event(ctx, keysym, modifiers, 0);
                assert_eq!(
                    consumed, 1,
                    "keysym={:#x} modifiers={:#x}",
                    keysym, modifiers
                );
                assert!(
                    sekka_context_poll_output(ctx).is_null(),
                    "keysym={:#x} modifiers={:#x}",
                    keysym,
                    modifiers
                );
                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "keysym={:#x} modifiers={:#x}",
                    keysym,
                    modifiers
                );
                assert_eq!(
                    sekka_context_is_registering(ctx),
                    1,
                    "keysym={:#x} modifiers={:#x}",
                    keysym,
                    modifiers
                );
                assert_eq!(
                    preedit_string(ctx),
                    "き",
                    "keysym={:#x} modifiers={:#x}",
                    keysym,
                    modifiers
                );
                assert!(
                    (&*ctx).ctx.active().has_staged_candidate(),
                    "the inner step's staged candidate must not be committed: keysym={:#x} modifiers={:#x}",
                    keysym,
                    modifiers
                );
            }

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            assert_eq!(preedit_string(ctx), "きK");

            let consumed = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(
                (&*ctx).ctx.active().get_preedit(),
                "K",
                "the romaji buffer must not have been converted"
            );
            assert_eq!(sekka_context_is_registering(ctx), 1);

            sekka_context_free(ctx);
        }
    }

    /// D-177/D-158: printable symbols (not romaji characters) during
    /// registration are appended to the word - even when the innermost
    /// step's own buffer is empty, matching what `commit_with_trailing_char`
    /// does outside registration - and never forwarded.
    #[test]
    fn printable_symbols_during_registration_go_into_the_word() {
        unsafe {
            let ctx = make_commit_display_state();
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx);

            for (keysym, expected) in [(0x2Bu32, "き+"), (0x2B, "き++"), (0x20, "き++ ")] {
                let consumed = sekka_context_process_key_event(ctx, keysym, 0, 0);
                assert_eq!(consumed, 1, "keysym={:#x}", keysym);
                assert_eq!(preedit_string(ctx), expected, "keysym={:#x}", keysym);
                assert!(
                    sekka_context_poll_output(ctx).is_null(),
                    "keysym={:#x}",
                    keysym
                );
                assert_eq!(
                    sekka_context_take_forward_key(ctx),
                    0,
                    "keysym={:#x}",
                    keysym
                );
            }

            let finished = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "き++ ");
            sekka_free_string(output);

            sekka_context_free(ctx);
        }
    }

    /// D-173: Ctrl-M inside the innermost step's candidate window only
    /// confirms the selected candidate exactly like D-09's own table (no
    /// special case for registration) - it does not register or commit
    /// anything by itself; a later Enter still registers whatever is now
    /// shown.
    #[test]
    fn ctrl_m_in_the_inner_window_only_confirms() {
        unsafe {
            let ctx = make_commit_display_state();
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx); // stage き
            sekka_context_trigger(ctx); // enter the inner candidate window
            sekka_context_trigger(ctx); // advance to キ
            assert_eq!(preedit_string(ctx), "キ");

            let consumed = sekka_context_process_key_event(ctx, b'm' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert_eq!(sekka_context_get_candidate_count(ctx), 0);
            assert_eq!(preedit_string(ctx), "キ");
            assert!(sekka_context_poll_output(ctx).is_null());

            let finished = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "キ");
            sekka_free_string(output);
            assert_eq!(sekka_context_is_registering(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    /// Exhaustive agreement test (D-177): for every keysym/modifier
    /// combination in the sweep, `is_selecting_table_key` agrees with
    /// whether `handle_selecting_key` actually consumes the key - keeping
    /// the pure predicate `would_forward_outside_registration` relies on
    /// from silently drifting out of step with the real D-09 table.
    #[test]
    fn selecting_table_predicate_agrees_with_handle_selecting_key() {
        let mut keysyms: Vec<u32> = (0x20..=0x7E).collect();
        keysyms.push(0xFF08);
        keysyms.push(0xFF09);
        keysyms.push(0xFF0D);
        keysyms.push(0xFF1B);
        keysyms.extend(0xFF50..=0xFF57);
        keysyms.push(0xFF8D);
        keysyms.push(0xFFBE);
        keysyms.push(0xFFFF);

        let modifier_sets = [0u32, MOD_CTRL, MOD_ALT, MOD_SUPER, MOD_CTRL | MOD_ALT];

        for &keysym in &keysyms {
            for &modifiers in &modifier_sets {
                let mut c = SekkaContext::new();
                c.process_key('K', false);
                c.process_key('a', false);
                c.process_key('\0', true);
                c.process_key('\0', true);
                assert_eq!(c.state(), ConversionState::Selecting);

                let expected = handle_selecting_key(&mut c, keysym, modifiers);
                let actual = is_selecting_table_key(keysym, modifiers);
                assert_eq!(
                    actual, expected,
                    "keysym={:#x} modifiers={:#x}",
                    keysym, modifiers
                );
            }
        }
    }

    /// D-171: the worked recursion example, without a dictionary - only the
    /// outermost step's Enter ever reaches the application; the inner
    /// step's Enter registers its word into the outer step's own word
    /// instead, and the prompt/reading getters reflect exactly one, then
    /// zero, nested readings.
    #[test]
    fn nested_registration_through_the_c_abi_commits_only_the_outermost_word() {
        unsafe {
            let ctx = sekka_context_new();

            for ch in "Sekka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);

            for ch in "Seki".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            let nested = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(nested, 1);
            assert_eq!(reg_prompt_string(ctx), "せき 登録 ");
            assert_eq!(reg_reading_string(ctx), "せっか");

            for ch in "Ishi".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);

            let finished_inner = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished_inner, 1);
            assert!(sekka_context_poll_output(ctx).is_null());
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert_eq!(reg_prompt_string(ctx), "登録 ");
            assert_eq!(preedit_string(ctx), "いし");

            for ch in "Ka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);

            let finished_outer = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(finished_outer, 1);
            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "いしか");
            sekka_free_string(output);
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    /// D-171/D-179: Esc from inside a nested registration step cancels only
    /// that step and returns exactly to the step before its own Ctrl-R was
    /// pressed (D-179 applied recursively) - a second Esc then cancels the
    /// outer step the same way, restoring the very first commit display and
    /// committing nothing.
    #[test]
    fn nested_escape_returns_to_the_outer_step_before_ctrl_r() {
        unsafe {
            let ctx = sekka_context_new();

            for ch in "Sekka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            for ch in "Seki".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            for ch in "Ishi".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }

            let consumed = sekka_context_process_key_event(ctx, 0xFF1B, 0, 0);
            assert_eq!(consumed, 1);
            assert_eq!(reg_prompt_string(ctx), "登録 ");
            assert_eq!(preedit_string(ctx), "せき");
            assert!(sekka_context_poll_output(ctx).is_null());

            let consumed = sekka_context_process_key_event(ctx, 0xFF1B, 0, 0);
            assert_eq!(consumed, 1);
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(preedit_string(ctx), "せっか");
            assert!(sekka_context_poll_output(ctx).is_null());

            sekka_context_free(ctx);
        }
    }

    /// D-171/D-169: Ctrl-R on a shape `begin_registration` refuses, pressed
    /// from inside an already-active registration step, is consumed and
    /// changes nothing at all - in particular it does not open a second,
    /// nested level.
    #[test]
    fn ctrl_r_inside_registration_on_a_refused_shape_changes_nothing() {
        unsafe {
            let ctx = sekka_context_new();

            for ch in "Sekka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);
            sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);

            for ch in "KaKu".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            sekka_context_trigger(ctx);

            let consumed = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(consumed, 1);
            assert_eq!(reg_prompt_string(ctx), "登録 ");
            assert!(sekka_context_poll_output(ctx).is_null());
            assert_eq!(sekka_context_is_registering(ctx), 1);

            sekka_context_free(ctx);
        }
    }

    // === D-15 / D-180 / D-181: `finalize_for_reset` through the C ABI (Phase 10, 10-04) ===

    /// D-181: an explicit reset commits only the outermost step's typed
    /// reading (D-168) - not the candidate that was selected when Ctrl-R was
    /// pressed ("セッカ") and not the word being assembled ("Ki") - and ends
    /// registration entirely. A NULL pointer does not crash.
    #[test]
    fn finalize_for_reset_commits_the_reading_and_ends_registration() {
        unsafe {
            sekka_context_finalize_for_reset(ptr::null_mut());

            let ctx = sekka_context_new();
            for ch in "Sekka".chars() {
                sekka_context_process_key_event(ctx, ch as u32, 0, 0);
            }
            // Ctrl-J stages the first candidate (せっか, no dictionary set).
            sekka_context_trigger(ctx);
            // A second Ctrl-J enters reselection.
            sekka_context_trigger(ctx);
            // A third Ctrl-J (an explicit next-candidate key in the Selecting
            // state) moves to the second candidate (セッカ).
            let next = sekka_context_trigger(ctx);
            assert_eq!(next, 1);
            assert_eq!(preedit_string(ctx), "セッカ");

            // Enter only moves the selection into the commit display state -
            // nothing is sent to the application yet (revised D-08).
            let confirmed = sekka_context_process_key_event(ctx, 0xFF0D, 0, 0);
            assert_eq!(confirmed, 1);
            assert!(sekka_context_poll_output(ctx).is_null());
            assert_eq!(preedit_string(ctx), "セッカ");

            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);
            assert_eq!(sekka_context_is_registering(ctx), 1);
            assert_eq!(
                reg_reading_string(ctx),
                "せっか",
                "the typed reading (D-168), not the selected candidate's display"
            );

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);

            sekka_context_finalize_for_reset(ctx);

            let output = sekka_context_poll_output(ctx);
            assert!(!output.is_null());
            assert_eq!(CStr::from_ptr(output).to_str().unwrap(), "せっか");
            sekka_free_string(output);
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(preedit_string(ctx), "");
            assert_eq!(sekka_context_take_forward_key(ctx), 0);

            sekka_context_free(ctx);
        }
    }

    /// D-180: a real focus loss (`sekka_context_reset`) during registration
    /// commits nothing at all - not even the outermost step's reading - and
    /// ends registration.
    #[test]
    fn reset_during_registration_commits_nothing() {
        unsafe {
            let ctx = make_commit_display_state();

            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);
            assert_eq!(sekka_context_is_registering(ctx), 1);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);

            sekka_context_reset(ctx);

            let output = sekka_context_poll_output(ctx);
            assert!(output.is_null());
            assert_eq!(sekka_context_is_registering(ctx), 0);
            assert_eq!(preedit_string(ctx), "");

            sekka_context_free(ctx);
        }
    }

    /// REG-10: `reset()` (via `reloadDictionaries`' own
    /// `sekka_context_set_dictionaries` path) releases every dictionary Arc a
    /// nested registration step was holding, so the same user dictionary
    /// path can be reopened - it stays locked as long as the context or its
    /// inner registration step still holds it.
    #[test]
    fn set_dictionaries_during_registration_drops_the_inner_steps() {
        unsafe {
            let tmp = tempfile::tempdir().expect("failed to create a temporary directory");
            let dict_path = tmp.path().join("registration_user_dict");
            let path_cstr =
                CString::new(dict_path.to_str().unwrap()).expect("failed to build the CString");
            let mut dict = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(!dict.is_null());

            let ctx = sekka_context_new();
            sekka_context_set_dictionaries(ctx, &mut dict, 1);

            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'a' as u32, 0, 0);
            sekka_context_trigger(ctx);

            let entered = sekka_context_process_key_event(ctx, b'r' as u32, 0x4, 0);
            assert_eq!(entered, 1);

            // The inner registration step converts its own input, inheriting
            // an Arc clone of the same dictionary list (begin_registration).
            sekka_context_process_key_event(ctx, b'K' as u32, 0, 0);
            sekka_context_process_key_event(ctx, b'i' as u32, 0, 0);
            sekka_context_trigger(ctx);

            // Releases only the caller's own handle - the context and its
            // inner registration step still hold their own Arc clones.
            sekka_free_dictionary(dict);

            // Contrast: the same path is still locked by sled (this test has
            // teeth - if this is NOT NULL, the premise above no longer
            // holds; stop and report rather than continuing on a false
            // assumption).
            let still_locked = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(
                still_locked.is_null(),
                "the context and its inner registration step should still hold the dictionary Arc"
            );

            // reloadDictionaries' own path: reset() drops the nested
            // registration session (and its Arc clone), then the empty
            // dictionary list replaces the context's own clone.
            sekka_context_set_dictionaries(ctx, ptr::null_mut(), 0);
            assert_eq!(sekka_context_is_registering(ctx), 0);

            let reopened = sekka_user_dict_new(path_cstr.as_ptr(), ptr::null());
            assert!(
                !reopened.is_null(),
                "every dictionary Arc should have been released by now"
            );
            sekka_free_dictionary(reopened);

            sekka_context_free(ctx);
        }
    }
}
