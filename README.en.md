# libsekka

A Japanese kana-kanji conversion library in Rust. It implements the conversion model of
[Sekka](https://github.com/kiyoka/sekka) by Kiyoka Nishiyama and exposes it to input
methods through a C ABI.

The fcitx5 addon lives in
[fcitx5-sekka](https://github.com/YuSabo90002/fcitx5-sekka).

[日本語版 / Japanese version](README.md)

## The Sekka conversion model

The **position of the uppercase letters** in the romaji input decides what kind of
conversion happens, so there is no mode-switching key.

| Input | Conversion | Result |
|---|---|---|
| `kanji` | all lowercase -> stays hiragana | かんじ |
| `Kanji` | leading uppercase -> kanji conversion | 漢字 |
| `kanJi` | inner uppercase -> conversion with okurigana | 感じ |
| `OkonaU` | leading and trailing uppercase -> okurigana marker | 行う |
| `2023` | digits only -> exactly 4 candidates | ２０２３ / 2023 / 二〇二三 / 二千二十三 |
| `2023nen` | digit-leading -> `#`-substituted lookup | 二千二十三年 |
| `.` | symbol -> the dictionary's symbol candidates | ． / ・ / 。 / … |

Candidate search has four stages, following upstream:

1. exact match
2. Jaro-Winkler similarity 1.0 (between romaji)
3. SymSpell (edit distance <= 1 between kana)
4. Jaro-Winkler similarity of at least 0.94

That is what lets a typo like `Nihogno` or a Kunrei-shiki spelling like `Konnitiha` still
reach the intended word.

## Structure

- Immutable master dictionary format: a single read-only file, mmapped, sorted by key and
  binary-searched, with a romaji prefix index and a SymSpell delete-variant index bundled in
  (`FORMAT_VERSION = 2`). The whole of SKK-JISYO.L is 175,789 keys / about 21.7 MB.
- User dictionary: backed by sled. It records a dictionary-wide sequence number for the most
  recently selected candidate and puts the most recently selected word first within each
  stage (exact matches, then fuzzy matches) — MRU. Word registration uses the same record.
- No external server is contacted during conversion (there is no equivalent of upstream's
  sekka-server).

## Building

```sh
cargo build --release
cargo test
```

Use [cargo-c](https://github.com/lu-zero/cargo-c) to produce the C ABI shared library and
`sekka.pc`:

```sh
cargo cinstall --release --prefix=/usr --libdir=lib
```

## Generating a dictionary

`sekka-dict-tool` converts the SKK dictionary format into the immutable master dictionary
format. The input encoding is auto-detected (UTF-8, then EUC-JP).

```sh
cargo build --release --bin sekka-dict-tool
./target/release/sekka-dict-tool convert SKK-JISYO.L --output master-dict.db
```

The output is mode 0444 (read-only). Rewriting the master dictionary while it is mmapped
takes the whole process down with SIGBUS, so it must never be placed at a writable path.

Use `dump` to inspect the contents of a user dictionary (it cannot run while fcitx5 has the
same dictionary open, because of sled's exclusive lock):

```sh
./target/release/sekka-dict-tool dump ~/.local/share/fcitx5/sekka/user-dict.db
```

The output is three tab-separated columns: reading, word, and the sequence number of the
last selection. `dump` neither migrates nor rewrites the dictionary, so dumping a v1.3 (or
earlier) dictionary as it is shows 0 in every third column. The output contains your input
history (readings and words) in plain text, so do not paste it into a shared place.

## Upgrading to v1.4

In v1.4 learning changed from a selection count (up to v1.3) to most-recently-selected
order. The user dictionary format changed with it, so read this before you update.

- **The migration happens once, automatically.** A v1.3 (or earlier) user dictionary is
  migrated to the new format on the first start of v1.4, and the order of the candidates for
  each reading is preserved. The migration prints nothing.
- **Downgrading is not supported.** A user dictionary in the new format cannot be read by a
  v1.3 (or earlier) binary. If you go back to v1.3 or earlier, for example by rolling back a
  NixOS generation, the user dictionary lives outside the Nix store and stays in the new
  format, so learning and the words you registered stop working.
- **No backup copy is made automatically.** Nothing is saved before the migration, so before
  updating to v1.4, quit fcitx5 and copy `user-dict.db` by hand:

  ```sh
  cp -a ~/.local/share/fcitx5/sekka/user-dict.db ~/.local/share/fcitx5/sekka/user-dict.db.v1.3-backup
  ```

  If you set `XDG_DATA_HOME`, use `$XDG_DATA_HOME/fcitx5/sekka/`; if you changed the user
  dictionary path in the settings, use `user-dict.db` at that path. To go back to v1.3 or
  earlier, quit fcitx5, move the new-format `user-dict.db` aside, and restore the copy under
  the original name. Without the copy, learning and registered words are not available. The
  copy also holds your input history in plain text, so do not leave it in a shared place.
- **The master dictionary is incompatible in one direction too.** A `master-dict.db` built
  with the v1.3 `sekka-dict-tool` is read by v1.4 as it is, so you do not need to rebuild
  it. A `master-dict.db` rebuilt with the v1.4 `sekka-dict-tool convert` cannot be read by a
  v1.3 (or earlier) binary (a lookup returns only the generated candidates such as hiragana
  and katakana). When you go back to v1.3 or earlier, use a `master-dict.db` built with the
  v1.3 tool.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE).

Parts of this library are ported from upstream Sekka
(<https://github.com/kiyoka/sekka>, master @ `0f73ee9`, retrieved 2026-09-23). Those files
carry Kiyoka Nishiyama's copyright and the upstream source in their SPDX headers.

- `src/kana_romaji.rs` - `emacs/sekka-jarowinkler.el` (the kana -> Hepburn romaji table, transcribed in full)
- `src/fuzzy.rs` - `emacs/sekka-jarowinkler.el`, `emacs/sekka-tests.el`
- `src/roman_index.rs` - `emacs/sekka-jarowinkler.el`, `emacs/sekka-jisyo.el`
- `src/symspell.rs` - `emacs/sekka-symspell.el`
- `src/candidate.rs` - `emacs/sekka-sharp-number.el`, `emacs/sekka-tests.el`
- `src/conversion.rs` - `emacs/sekka-henkan.el`

Other files follow upstream's **behaviour** as well, but no code was ported into them.

The dictionary data (SKK-JISYO.L, <https://github.com/skk-dev/dict>) is GPL-2.0-or-later and
is not included in this repository. Note that the `master-dict.db` produced by
`sekka-dict-tool` is a derivative of it (if you distribute one, follow the license of
SKK-JISYO.L).
