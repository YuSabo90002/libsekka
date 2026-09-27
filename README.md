# libsekka

日本語かな漢字変換ライブラリ（Rust）。[Sekka](https://github.com/kiyoka/sekka)（石火、
Kiyoka Nishiyama 作）の変換モデルを実装し、C ABI を通じて入力メソッドから利用できる。

fcitx5 用のアドオンは [fcitx5-sekka](https://github.com/YuSabo90002/fcitx5-sekka)。

[English version](README.en.md)

## Sekka の変換モデル

ローマ字入力の**大文字の位置**が変換の種類を決める（モード切替のキーを持たない）。

| 入力 | 変換 | 結果 |
|---|---|---|
| `kanji` | 全小文字 → ひらがなのまま | かんじ |
| `Kanji` | 先頭大文字 → 漢字変換 | 漢字 |
| `kanJi` | 途中大文字 → 送り仮名付き変換 | 感じ |
| `OkonaU` | 先頭＋末尾大文字 → 送り仮名マーカー | 行う |
| `2023` | 数字のみ → 4候補固定 | ２０２３ / 2023 / 二〇二三 / 二千二十三 |
| `2023nen` | 数字始まり → `#` 置換辞書引き | 二千二十三年 |
| `.` | 記号 → 辞書の記号候補 | ． / ・ / 。 / … |

候補の検索は4段構造で、本家に準拠する。

1. 完全一致
2. Jaro-Winkler 類似度 1.0（ローマ字同士）
3. SymSpell（かな同士の編集距離 ≤ 1）
4. Jaro-Winkler 類似度 0.94 以上

これにより `Nihogno`（タイプミス）や `Konnitiha`（訓令式）からも意図した語に到達する。

## 構成

- 不変マスター辞書フォーマット: 単一ファイル・読み取り専用 mmap・キー順ソート＋二分探索。
  ローマ字プレフィックス索引と SymSpell 削除バリアント索引を同梱する（`FORMAT_VERSION = 2`）。
  SKK-JISYO.L 全体で 175,789 キー / 約 21.7 MB。
- ユーザー辞書: sled バックエンド。確定した候補の頻度を記録し、次回以降の並びに反映する。
- 変換処理中に外部サーバーへ通信しない（本家の sekka-server 相当の機能は持たない）。

## ビルド

```sh
cargo build --release
cargo test
```

C ABI 共有ライブラリと `sekka.pc` を出すには [cargo-c](https://github.com/lu-zero/cargo-c)
を使う。

```sh
cargo cinstall --release --prefix=/usr --libdir=lib
```

## 辞書の生成

`sekka-dict-tool` が SKK 辞書形式から不変マスター辞書フォーマットへ変換する。
入力のエンコーディングは自動判別（UTF-8 → EUC-JP）。

```sh
cargo build --release --bin sekka-dict-tool
./target/release/sekka-dict-tool convert SKK-JISYO.L --output master-dict.db
```

生成物は 0444（読み取り専用）になる。マスター辞書は mmap 中に書き換えられると SIGBUS で
プロセスごと落ちるため、書き込み可能なパスに置いてはならない。

ユーザー辞書の中身を確認するには `dump` を使う（fcitx5 が同じ辞書を開いている間は
sled の排他ロックにより実行できない）。

```sh
./target/release/sekka-dict-tool dump ~/.local/share/fcitx5/sekka/user-dict.db
```

## ライセンス

GPL-3.0-or-later。詳細は [LICENSE](LICENSE)。

本家 Sekka（<https://github.com/kiyoka/sekka>、master @ `0f73ee9`、2026-09-23 取得）から
移植した部分がある。該当ファイルには SPDX ヘッダで Kiyoka Nishiyama の著作権表示と
移植元を明記している。

- `src/kana_romaji.rs` — `emacs/sekka-jarowinkler.el`（かな→ヘボン式ローマ字表の全文転記）
- `src/fuzzy.rs` — `emacs/sekka-jarowinkler.el`、`emacs/sekka-tests.el`
- `src/roman_index.rs` — `emacs/sekka-jarowinkler.el`、`emacs/sekka-jisyo.el`
- `src/symspell.rs` — `emacs/sekka-symspell.el`
- `src/candidate.rs` — `emacs/sekka-sharp-number.el`、`emacs/sekka-tests.el`
- `src/conversion.rs` — `emacs/sekka-henkan.el`

上記以外のファイルも変換の**挙動**は本家に準拠させているが、コードの移植は行っていない。

辞書データ（SKK-JISYO.L、<https://github.com/skk-dev/dict>）は GPL-2.0 以降であり、
本リポジトリには含まれない。`sekka-dict-tool` が生成する `master-dict.db` はその派生物で
あることに注意（配布する場合は SKK-JISYO.L のライセンスに従うこと）。
