# 開発環境

このリポジトリを開発するための環境と、道具の版の固定の仕方。設計は [design.md](design.md) にある。

## 必要なもの

| 対象 | 内容 |
| --- | --- |
| OS | Windows 10/11（x64）。ゲームのビルド・実行・計測は、Windowsのネイティブで行う |
| Git | Git for Windows |
| C++のビルド環境 | Visual Studio Build Tools（C++のワークロード、MSVCのツールセット、Windows SDK） |
| 版の固定 | [mise](https://mise.jdx.dev/)。Rustと補助のCLIの版は `mise.toml` と `mise.lock` で固定する |
| Linux側の道具（任意） | WSL上のNixOS（またはNixの入ったLinux）。ファジング、Windowsに依存しないcrateのLinuxでのビルドとテスト、WASMのビルドの確認に使う。CIでも同じ確認をするので、手元に無くても開発はできる |

## リポジトリの置き場所

- 作業コピーは、短いパス（例：`C:\dev\tapestry`）に置く。Gitでは `core.longpaths=true` を設定し、OSでも長いパスを有効にする。OSの設定だけでは、すべての道具が長いパスに対応するわけではない。
- 改行は `.gitattributes` で決める。
- WSLからは、同じ作業コピーを `/mnt/c/...` として使う。Windowsで作ったworktreeもWSLから読めるように、`git config --global worktree.useRelativePaths true` を設定してからworktreeを作る（Windows側とWSL側のGitが、ともにこの設定に対応している版であること）。作業コピーを変えるGitの操作（コミット、ブランチの切り替えなど）はWindows側だけで行う。ファイルの変更の監視は、WindowsとWSLをまたいで使わない。

## 版の固定

道具の版は mise で固定する。`mise.toml` にはメジャー番号の範囲を書き、正確な版は `mise.lock` で固定する。`mise.toml` で `mise.lock` に無い版を使わない設定にする：

```toml
[tool_config]
locked = true

[settings]
lockfile_platforms = ["windows-x64", "linux-x64"]
```

`[tool_config]` の `locked` は、このリポジトリで宣言した道具だけに効く（グローバルの道具には効かない）。
`mise.lock` の更新は、定期的に動くワークフローがPRとして出す。

### Rust

Rustの版も mise で固定する。言語ごとの版のファイル（`rust-toolchain.toml`）は置かない。

```toml
[tools]
rust = { version = "1", profile = "minimal", components = "rustfmt,clippy" }
```

- miseのRustはrustupを使って導入し、`RUSTUP_TOOLCHAIN` を設定して版を選ぶ。そのため、miseを通さずに `cargo` を呼ぶと、このリポジトリで固定した版にならない。`mise exec -- cargo ...` で呼ぶか、miseを有効にしたシェルで作業する。
- miseを使わない場合は、`mise.lock` に書かれた版をrustupで入れ、`cargo +<版> ...` で呼ぶ。
- 追加のターゲット（`wasm32-unknown-unknown` など）は、それを使う段階で足す。

### フック

[lefthook](https://lefthook.dev/) でgitのpre-commitフックを動かす。lefthook自体も mise で固定する。

```sh
mise install
mise exec -- lefthook install
```

pre-commitでは、禁止語の検査と `rustfmt` の確認を行う。フックは手元で早く気づくためのもので、最終の関門はCIである。

## Linux側の道具（WSL）

- Linux側の道具は、Nixのflake（`flake.nix` と `flake.lock`）の開発用シェルで固定する。ファジングに使う日付付きのnightlyも、ここでだけ使う。
- NixOSでは `nix-command` と `flakes` を有効にしておく。flakeはGitに登録されたファイルしか読まないので、`flake.nix` とそこから参照するファイルはGitに登録してから使う。
- WSLでのビルドの出力は、`CARGO_TARGET_DIR` でWSLの中のディレクトリに分ける。Windows側の出力と混ざらず、`/mnt/c` 越しの書き込みの遅さも避けられる。
- Windows側からWSLのコマンドを呼ぶときは、リポジトリのラッパースクリプトを使う。ラッパーは、作業コピーのWindowsでのパスを受け取ってWSLでのパスに変換し（`wslpath`）、`wsl.exe -d NixOS --cd <変換したパス> -- nix develop --command ...` を実行する。引数の引用と終了コードの受け渡しも、ラッパーで固定する。

## CI

| ランナー | 行うこと |
| --- | --- |
| Windows（OSの版を明示したラベル） | 本番ビルド（`dev-tools` を無効にした構成で固定）、開発用の機能が含まれていないことの確認、WARPによる描画の画像比較、Windows向けのテスト |
| Linux | Windowsに依存しないcrateのビルドとテスト、WASMのビルド、ファジングの短時間の実行、禁止語の検査、依存の検査（`cargo-deny`、`cargo-audit`） |

- どちらのランナーも、同じ `mise.lock` から道具を入れる（`jdx/mise-action`）。キャッシュを戻したあとも導入の処理を走らせ、使われているRustの版を確かめる。
- GitHub Actionsは、コミットのSHAで固定する。
- Windowsのランナーは、ラベルで版を指定してもイメージが更新され続ける。ランナーのイメージの版、使ったアダプター、使える機能を記録する。イメージの更新で画像比較の結果が変わったときは、差分の画像を確かめてから正解の画像を更新する。

## 計測のときの条件

性能と遅れの計測は、普段の開発とは分けた状態で行い、次の条件を結果とともに記録する。

- コードのコミットと、まだコミットしていない変更の有無、ビルドの構成
- 計測の場面と、使った入力ログ、計測の道具の版
- CPUとメモリー、GPUとドライバーの版、Windowsのビルド、電源の設定
- 解像度、リフレッシュレートとVRR
- 音声デバイスとドライバー、WASAPIの共有か排他か、サンプルレート、バッファ、音声の拡張処理の有無
- 入力装置と、つないだUSBの口

OBSなしの基準と、OBSありの基準（エンコーダー、解像度、fpsを固定）は別に持つ。音の遅れの絶対値は、ゲームの内部の時刻だけでなく、試験用の入力装置で確かめる（[design.md](design.md) の「テスト方針」）。

## その他

- ウイルス対策の除外は、既定では設定しない。ビルドが遅いと実際に確かめた場合だけ、ビルドの出力のディレクトリに限って除外する。
- コード署名は、公開して配布する前に準備する。開発中は不要。
