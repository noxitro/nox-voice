# nox-voice

[![CI](https://github.com/noxitro/nox-voice/actions/workflows/ci.yml/badge.svg)](https://github.com/noxitro/nox-voice/actions/workflows/ci.yml)
[![CodeQL](https://github.com/noxitro/nox-voice/actions/workflows/codeql.yml/badge.svg)](https://github.com/noxitro/nox-voice/actions/workflows/codeql.yml)

Windows 11 向けの常駐型 AI 音声入力アプリ。ホットキーを押して話すと、フィラーと言い直しを取り除き句読点を整えたテキストが、**いま入力中のアプリへそのまま貼り付けられます**。

チャット・エディタ・ブラウザなど、貼り付け先は問いません。

```
[ホットキー押下] → [録音] → [音声認識] → [LLM で整形] → [フォーカス中のアプリへ貼り付け]
```

## 特徴

**話し終わってから待たされません。** 整形まで含めて実測の中央値が約 0.9 秒です。

**失敗しても発話が消えません。** 整形が落ちたら音声認識の生テキストをそのまま貼り、その事実を小窓に表示します。認識自体が落ちても音声ファイルを退避します。黙って失敗しないことを最優先に設計しています。

**整形の提供元が二重化されています。** 主(Gemini)が落ちたら副(Groq)が整形を引き継ぎ、両方落ちて初めて生テキストになります。

**貼り付け先を録音中に表示します。** アプリのアイコンと名前が小窓に出るので、意図しないウィンドウへ貼る事故に気づけます。

**貼り付け先に応じて文体を変えられます。** AI への指示文では言い換えを抑え、チャットでは口語にする、といったプロファイルを 32 種類同梱しています。

**読み仮名つきのユーザー辞書**があり、固有名詞の誤変換を減らせます。貼り付け後の手直しから自動で学習することもできます。

**生テキストと整形結果を並べて履歴に残します。** LLM が勝手に足したり削ったりしていないか、後から照合できます。

その他: 用途別ホットキー(貼り付け / クリップボードのみ)、通知音、画面質問モード(既定オフ)、ローカル音声認識へのフォールバック(オプトイン)、OS テーマ追従のダークモード。

## 動作要件

- Windows 10 / 11(x64)
- [Groq](https://console.groq.com/) の API キー(音声認識・整形の控え)
- [Google AI Studio](https://aistudio.google.com/) の API キー(整形)

どちらも無料枠で動きます。**キーはあなた自身のものを使います。** アプリにキーは同梱されておらず、作者が費用を負担することも、あなたの利用が作者に課金されることもありません。

## インストール

[リリースページ](https://github.com/noxitro/nox-voice/releases/latest)の `nox-voice_x.y.z_x64-setup.exe` を実行してください(ユーザー単位のインストールで、管理者権限は要りません)。PC 全体へ入れる場合は同じページの MSI を使います。

> **署名していないため、SmartScreen が警告を出します。** 「詳細情報」→「実行」で進めてください。気になる場合は、下記の方法で配布物を確かめるか、自分でビルドしてください。

### ダウンロードしたファイルの確かめ方

配布物はすべて GitHub Actions がこのリポジトリのタグからビルドしたもので、手元でビルドしたものは置いていません。

- **ハッシュ**: 同じページの `SHA256SUMS.txt` と照合できます(PowerShell: `Get-FileHash .\nox-voice_x.y.z_x64-setup.exe`)
- **来歴**: どのリポジトリのどのタグから Actions がビルドしたかを、[GitHub CLI](https://cli.github.com/) で検証できます

  ```
  gh attestation verify nox-voice_x.y.z_x64-setup.exe --repo noxitro/nox-voice
  ```

同梱している第三者ソフトウェアのライセンス表示は `THIRD_PARTY_NOTICES.txt` にあります(インストール先にも入ります)。

## API キーの設定

**環境変数を推奨します。**

```
GROQ_API_KEY=<あなたのキー>
GEMINI_API_KEY=<あなたのキー>
```

設定画面からも入力できますが、その場合は**あなたの PC 上の `config.json` に平文で保存されます**。同じ PC を他人が触る環境では環境変数を使ってください。環境変数が設定されていればそちらが優先されます。

## 使い方

インストールするとタスクトレイに常駐します。既定のホットキーは **左 Ctrl + Space** で、設定画面から変更できます。

押している間だけ録音する PTT と、押すたびに切り替わるトグルの両方に対応しています。Stream Deck のフットペダルのような**画面を見ない運用**も想定しており、録音開始と失敗は通知音でも伝えます。

## プライバシー

**このアプリはあなたの音声と文字をクラウドへ送ります。** 何がどこへ行くかを明示します。

| 送るもの | 送り先 | いつ |
|---|---|---|
| 録音した音声 | Groq | 毎回 |
| 音声認識の結果・辞書・文体指示 | Google(Gemini) | 整形するとき |
| 同上 | Groq | Gemini が落ちたときのみ |
| フォーカス中の要素のテキスト | Google / Groq | deep context が有効なときのみ(既定オフ) |
| モニタ 1 枚分の画面内容 | Google | 画面質問モードのときのみ(既定オフ・専用ホットキー必須) |

**無料枠の利用規約では、送信内容が提供元の製品改善やモデル学習に使われ、人間のレビュアーが閲覧しうる点に注意してください。** 機密を扱うなら有料プランへ切り替えるか、下記の「ローカルのみ」モードを使ってください。

**ローカルのみモード**(`local-stt` フィーチャ付きでビルドした場合)にすると、**音声ファイルを外部へ送らず**、認識をあなたの PC 上で行います。

> **ただし整形はクラウドのままです。** 認識結果のテキストは引き続き Gemini(および控えの Groq)へ送られます。**テキストも一切送りたくない場合は、設定で整形をオフにしてください。** そのときは認識結果がそのまま貼り付けられ、外部通信は発生しません。

履歴は SQLite でローカルにのみ保存され、保持期限を設定できます。画面質問モードで読んだ内容と、その回答は履歴に一切残しません。

## ビルド

```bash
npm ci
npm run tauri build
```

Node.js 24 以上と Rust の安定版が要ります。成果物は `src-tauri/target/release/bundle/` に出ます。

ローカル音声認識を含める場合は `--features local-stt` を付けます。whisper.cpp のビルドに CMake と C++ ツールチェーンが必要になるため、既定では外してあります。

> 手元でビルドした exe には、依存クレートの絶対パス(`C:\Users\<ユーザー名>\.cargo\...`)が埋め込まれます。人に配るものは、下の「リリースの手順」で CI に作らせてください(CI はパスを付け替え、ライセンス表示を同梱します)。

### 検証

```bash
npm run build                                                    # 型検査 + ビルド
cargo clippy --manifest-path src-tauri/Cargo.toml --lib --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml --lib
npm run sim                                                      # 疑似 E2E
npm run version:check                                            # 版が 5 か所で揃っているか
```

実機 E2E(`npm run e2e:hotkey` など)は実行中のアプリを落とし、本物の設定ファイルを書き換えます。走らせる前に [docs/hotkey-e2e.md](docs/hotkey-e2e.md) を読んでください。

## CI とリリース

GitHub Actions で次を回しています。

| ワークフロー | いつ | 何をするか |
|---|---|---|
| CI | main への push / PR | 型検査・clippy・単体テスト・疑似 E2E、リリース構成の exe での実機 E2E(Windows ランナー)、依存のライセンス・脆弱性の検査 |
| CodeQL | main への push / PR / 毎週 | コードの静的解析(結果は Security タブ) |
| Release | タグ `v*.*.*` | CI を通したうえでインストーラを作り、実際に入れて起動を確かめてから Release に公開 |
| Live API | 毎週(要設定)/ 手動 | 実際の Groq / Gemini で音声認識と整形がまだ通るか |

依存の更新 PR は Dependabot が毎週作ります。Tauri だけは、Rust 側と JS 側の版(major.minor)を揃えて手で上げます。

### リリースの手順

```bash
npm run version:set -- 0.6.0          # 5 か所の版を揃える
git commit -am "chore: 0.6.0 へ上げる" && git push
git tag v0.6.0 && git push origin v0.6.0
```

タグを打つ前に試したいときは、Actions の Release を手動実行(Run workflow)します。公開はせず、作ったインストーラを成果物として残します。版にハイフンを含むタグ(`v0.6.0-beta.1`)はプレリリースになります。

### 実 API のテストを有効にする

Settings → Secrets and variables → Actions で、Secrets に `GROQ_API_KEY` と `GEMINI_API_KEY`、Variables に `LIVE_API_TESTS` = `true` を登録します。送るのは合成音声と固定の例文だけで、キーはフォークからの PR には渡りません。

## 既知の制限

- **管理者権限で動いているアプリへは貼り付けられません**(Windows の UIPI による制限)
- 署名していないため SmartScreen とウイルス対策ソフトが警告します。グローバルキーボードフックを使う都合上、誤検知されることもあります
- 設定画面からキーを入力すると平文で保存されます
- Windows 専用です。Win32 API(キーボードフック・UI Automation・DWM)に直接依存しているため、他 OS への移植予定はありません

## 設計ドキュメント

技術的な判断の根拠と実測値は [docs/design.md](docs/design.md) にあります。「なぜこの実装なのか」「なぜ別の案を採らなかったのか」を、実際に踏んだ失敗とともに記録しています。製品としての方針は [PRODUCT.md](PRODUCT.md) にあります。

## ライセンス

[MIT](LICENSE)
