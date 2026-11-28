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

接頭辞・接尾辞は SKK 辞書と同じく `>` 付きの読みで引く。`>` はローマ字の一部としてためておき、
変換したときだけ読みの印として解釈する。`cho>` を変換すると読み「ちょう>」（超）を、変換した語の
直後に打った `>teki` を変換すると読み「>てき」（的）を引く（`nihon` → 日本、続けて `>teki` →
日本的。接頭辞・接尾辞と前後の語は別々の変換になる）。`>` を打った時点で、表示中の語は確定する。
変換しなければ、`>` を含む入力はそのままの文字で確定する（`a>b` → a>b）。`>` を含む読みは完全
一致と生成候補（ひらがな・カタカナ・ローマ字）だけで引き、あいまい検索はかけない。逆に、`>` を
含まない普通の変換には、接頭辞・接尾辞の読みにしか無い語は出ない。送り仮名付きの接尾辞には
対応していない。

単語登録の中では、登録する単語がカーソルを持つ。← と → で1文字ずつ、Home と End で先頭と末尾へ
動かし（テンキーの同じキーも含む）、入力や変換の結果はカーソルの位置に入る。BackSpace はカーソルの
前の1文字を、Delete はカーソルの後ろの1文字を消す。ローマ字の打ちかけや変換中は、これらのキーは
何もしない。

## 構成

- 不変マスター辞書フォーマット: 単一ファイル・読み取り専用 mmap・キー順ソート＋二分探索。
  ローマ字プレフィックス索引と SymSpell 削除バリアント索引を同梱する（`FORMAT_VERSION = 2`）。
  SKK-JISYO.L 全体で 175,789 キー / 約 36.6 MB。
- ユーザー辞書: sled バックエンド。確定した候補に「最後に選んだ」通し番号を記録し、次の変換で各段（完全一致／あいまい一致）の中で最後に選んだ語を先頭に並べる（MRU）。単語登録も同じ記録を使う。
- 変換処理中に外部サーバーへ通信しない（本家の sekka-server 相当の機能は持たない）。

## ビルド

libsekka のリポジトリ（チェックアウト）の根で次を打つ。

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
libsekka のリポジトリ（チェックアウト）の根で次を打つ。

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

出力は「読み」「語」「最後に選んだ通し番号」のタブ区切り3列。`dump` は辞書を移行も
書き換えもしないので、0.4.0 以前の形式のまま `dump` すると3列目はすべて 0 になる。
出力には入力の履歴（読みと語）が平文で入るので、共有の場所には貼らないこと。

## 更新の注意（0.5.0）

0.5.0 で学習が「選んだ回数」（0.4.0 まで）から「最後に選んだ順」に変わった。ユーザー辞書の
形式も変わるので、更新の前に次を読んでおくこと。

- **移行は一度だけ、自動で行われる。** 0.4.0 以前のユーザー辞書は 0.5.0 の最初の起動で新形式へ
  移行され、各読みの候補の順位は保たれる。移行は何も表示しない。
- **ダウングレードはできない。** 新形式のユーザー辞書は 0.4.0 以前のバイナリでは読めない。
  NixOS の世代ロールバックなどで 0.4.0 以前に戻すと、ユーザー辞書は Nix store の外にあって
  新形式のまま残るので、学習と登録した語が使えなくなる。
- **退避のコピーは自動では作られない。** 移行の前にバックアップは取られないので、0.5.0 へ
  更新する前に、fcitx5 を終了してから `user-dict.db` を手でコピーしておく。

  ```sh
  cp -a ~/.local/share/fcitx5/sekka/user-dict.db ~/.local/share/fcitx5/sekka/user-dict.db.0.4.0-backup
  ```

  `XDG_DATA_HOME` を設定しているときは `$XDG_DATA_HOME/fcitx5/sekka/` の下、設定でユーザー辞書の
  パスを変えているときはそのパスの `user-dict.db` を対象にする。
  0.4.0 以前へ戻すときは、fcitx5 を終了してから新形式の `user-dict.db` を別名へ退け、コピーを
  元の名前に戻す。コピーが無いまま戻すと、学習と登録した語は使えない。コピーにも入力の履歴が
  平文で入るので、共有の場所には置かないこと。
- **マスター辞書にも向きのある非互換がある。** 0.4.0 の `sekka-dict-tool` で作った
  `master-dict.db` は 0.5.0 でそのまま読めるので、作り直さなくてよい。0.5.0 の
  `sekka-dict-tool convert` で作り直した `master-dict.db` は 0.4.0 以前のバイナリでは読めない
  （引いても候補が平仮名・片仮名などの生成候補だけになる）。0.4.0 以前へ戻すときは、0.4.0 の
  ツールで作った `master-dict.db` を使うこと。

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
