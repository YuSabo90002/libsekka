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
- User dictionary: backed by sled. It records the frequency of committed candidates and
  reflects that in later orderings.
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
./target/release/sekka-dict-tool dump ~/.local/share/sekka/user-dict.db
```

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
