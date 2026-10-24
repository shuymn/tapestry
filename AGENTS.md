# tapestry

Windows専用のBMSプレーヤー（7鍵＋スクラッチの音楽ゲーム）。設計の正本は [docs/design.md](docs/design.md)、環境と道具の版は [docs/setup.md](docs/setup.md)。設計の判断を変えるときは、先に design.md を直す。

## 守ること

- **市販ゲームの名前を書かない:** 元になった市販ゲームの作品名・シリーズ名・メーカー名を、識別子・コメント・文字列・ログ・テスト名・ファイル名・文書に入れない。「7鍵」「スクラッチ」「BMS」のような一般的な語を使う。例外は禁止語の一覧のファイルだけ。
- **評価器は純粋:** `eval` は譜面・入力ログ・プリセットだけから結果を決める。時計・入出力・音声・描画のcrateに依存させない。時刻は整数µs（`i64`）、ゲージは固定小数点で、浮動小数点を使わない。
- **Windowsに依存するコードは `input`・`audio`・`render`・`player` に閉じ込める:** それ以外のcrateはLinuxでもビルドとテストが通る状態を保つ。
- **開発用の機能は `dev-tools` featureの中だけ:** 本番ビルドにはコードそのものを含めない。実行時のフラグで隠す方式にしない。
- **プレイ中の処理でメモリを確保しない:** 入力・ロジック・音声のコールバック・描画の毎フレームの処理が対象（design.md「主要なライブラリとアルゴリズム」）。
- **権利のある譜面や音源をリポジトリに置かない:** テスト用の譜面は、テストのコードで組み立てるか、自作の小さなBMSのテキストで作る。

## コマンド

Rustと道具の版は mise で固定している。`cargo` は `mise exec --` を通して呼ぶ（通さないと固定した版にならない）。

```sh
mise install
mise exec -- lefthook install
mise exec -- cargo fmt --check
mise exec -- cargo clippy --workspace --all-targets -- -D warnings
mise exec -- cargo test --workspace
```

Linux側の道具（ファジング、Linuxでのビルド）は、WSLのラッパースクリプト経由で `nix develop` の中で動かす（docs/setup.md「Linux側の道具」）。
