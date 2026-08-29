# 引き継ぎ (2026-08-28 時点)

リポジトリ移設にあたっての現状記録。**このファイルは状況のスナップショットであり、正典ではない。**
設計判断の正典は [`design.md`](design.md)、製品の前提は [`../PRODUCT.md`](../PRODUCT.md)。
両者と食い違ったらそちらが正しい。

---

## 1. いまどうなっているか (一行で)

**4 つの新機能とメインウィンドウの再設計が入り、テストは全部緑。キー捕獲の不具合も修正済みだが、
修正後の実機確認 (物理キーでの捕獲) がまだ取れていない。**

---

## 2. コミット済み

| コミット | 内容 |
|---|---|
| `bfe2790` | リリースビルドに `tauri/custom-protocol` を必須化 (`npm run release`) |
| `c9ad631` | Q6: 通知音・用途別ホットキー・多重起動防止・UI 再設計・画面質問モード・文体プロファイル拡充 |

`c9ad631` で入ったもの (すべて 実装 → レビュー → 指摘修正 のゲートを通している):

- **通知音** (`src-tauri/src/sound.rs`) — 合成プリセット 12 種 + WAV 指定。録音開始とキャンセル/エラーで鳴る
- **用途別ホットキー** (`src-tauri/src/hotkey.rs`) — `HotkeyMode` でスロット化。貼り付け / クリップボードのみ / 画面質問の 3 用途
- **多重起動防止** (`src-tauri/src/instance.rs`) — `run()` 1 行目の名前付きミューテックス + `WM_COPYDATA` で既存窓を出す
- **UI 再設計** — 左レール型シェル 8 区画。状態表示はレール下端に常駐
- **画面質問モード** (`src-tauri/src/screen.rs`, `screen/win32.rs`) — モニタ単位 UIA 走査 + スクショ fallback。既定 OFF
- **文体プロファイル** (`src-tauri/src/style.rs`) — 既定 31 件 + 版管理 + 履歴からの提案 + 適用記録 + 行単位 UI

---

## 3. 直近の作業 (キー捕獲の DOM 移行) — 完了、実機確認だけ未了

**背景**: `WH_KEYBOARD_LL` が OS に無言で外される問題 (§5) の影響で、キー捕獲 UI が実機で動かなかった。
捕獲は「自ウィンドウにフォーカスがある」場面の機能なのでグローバルフックを使う必然性が無く、
**DOM の `keydown`/`keyup` へ移した**。これでフックの生死と無関係になる。

`event.code` (レイアウト非依存の物理キー位置) を集め、**VK 変換と検証は Rust 側の既存関数を通す**
(`code_to_vk` → `decide_capture_combo` → `sanitize_combo`)。許可キー判定・ラベル生成を
二重実装するとずれるため。フック側の捕獲経路は削除し、`CAPTURE_MODE` フラグだけ
「捕獲中は PTT を消音する」意味で残してある (残さないと、設定しようとキーを押した瞬間に録音が始まる)。

### 実機で出た 3 件の不具合 — すべて修正済み

**(1) 捕獲開始が非同期で、その間のキーが素通りしていた**

`capturingMode` のセットとフォーカス外しが `await invoke("start_hotkey_capture")` の**後**にあり、
IPC 往復の間だけ「キーを受け付けられない時間帯」があった。その間に押した Space は
`preventDefault()` されず、フォーカスの残ったボタンを再発火させて捕獲を即取り消していた。
ユーザーの報告「space キーを押すと、ウィンドウのほうにフォーカスされてしまっていました」がこれ。

**修正**: 楽観的開始 + ロールバック。フラグとフォーカス外しを `await` の**前**へ移し、
IPC が失敗したら巻き戻す。隙間を短くするのではなく**存在しない構造**にした。

**陽性コントロールつき**: テスト `U2c` を先に書き、修正前に落ちることを実測してから直した。
モック invoke に遅延を注入 (`mock.delays`) して IPC 往復を再現する。

| | 修正前 | 修正後 |
|---|---|---|
| `downPrevented` / `upPrevented` | false / false | **true / true** |
| IPC 中の `activeElement` | `hotkey-capture` | **BODY** |
| 捕獲の開始/取消回数 | — | starts=1 / cancels=0 |

**(2) 拒否されたキーで捕獲が終わる** → 戻り値を `Result<_, String>` から
タグ付き enum `CaptureVerdict` (`accepted` / `cancelled` / `rejected` / `expired`) に変更。
**捕獲が続くかどうかを型で表す。** 文字列から状況を推測させる形をやめた。

**(3) `event.code` が空で届く** → **ハーネス側の問題だった。**
実装は正しく、`Input.dispatchKeyEvent` 特有の癖。**これが T10 失敗の真因**でもあった:
T7 の Esc が `Unidentified` で届く → 取り消されず捕獲が開いたまま → T10 の click が
「捕獲中なら畳む」に入って終了ログだけ出して return → 素通り。
ハーネスに `detectKeyTransport()` を追加し、接続直後に F13 を 1 打送って
`code` が届くか**実測**し、駄目なら `KeyboardEvent` 合成へ落とすようにした。

---

## 4. テストとゲート

| ゲート | コマンド | 現在値 |
|---|---|---|
| Rust 単体 | `cargo test --lib` (`src-tauri/`) | **480 passed / 0 failed / 28 ignored** |
| Rust lint | `cargo clippy --lib --all-targets` | 警告ゼロ |
| 型 | `npx tsc --noEmit` | エラーなし |
| フロント | `npm run build` | 成功 |
| UI 疑似 E2E | `node e2e/sim-ui.mjs` | **22 PASS / 0 FAIL** |
| 多重起動 疑似 | `cargo test --lib instance::sim` | 7 passed |
| 一括 | `npm run sim` | 上 2 つをまとめて |

### 実 E2E (実機が要る)

| ハーネス | コマンド | 最終結果 |
|---|---|---|
| 多重起動 | `npm run e2e:single-instance` | **PASS 5 / FAIL 0** (2026-08-27 実測) |
| ホットキー | `npm run e2e:hotkey` | **未再実行**。前回は PASS 4 / FAIL 4 / SKIP 3 だったが、§3 の不具合 3 件を修正済みなので測り直しが要る |

**どちらも `taskkill /F /IM nox-voice.exe` を打ち、合成キーを前景へ送る。**
実行中はキーボードに触れないこと。作業中のマシンでは走らせない。

### 疑似テストとは

実機 E2E が回せない状況で、機構そのものを検証するために作ったもの (`npm run sim`)。

- `instance::sim` — 実プロセス 2 個でミューテックスを競合させ、`WM_COPYDATA` を実際に送受信する
- `e2e/sim-ui.mjs` — headless Edge + `window.__TAURI_INTERNALS__` のモックで DOM を検証

**緑でも実機 E2E の代わりにはならない。** 実際、§3 の不具合 (1) は疑似テストをすり抜けた —
モックが**即座に解決する**テストでは、IPC 往復の隙間に起きる不具合は永久に再現しない。
遅延注入 (`mock.delays`) を足して初めて捕まえられた。
`invoke` のコマンド名・引数名の綴り、物理キーが OS → WebView2 → DOM と届くこと、
WebView2 固有の描画は疑似テストでは測れない。

---

## 5. 環境の罠 (踏むと時間を溶かす)

### `cargo build --release` は Tauri ではリリースにならない

素の release ビルドは「最適化された dev ビルド」で、フロントを同梱せず
**devUrl (`localhost:1420`) を焼き込む**。vite が居ないので起動すると
`ERR_CONNECTION_REFUSED` の画面になる (実際に踏んだ)。

**必ず `npm run release` を使うこと。** 検証はビルド成功ではなく **exe の中身**で行う:

```bash
grep -ac "<dist のハッシュ付きアセット名>" src-tauri/target/release/nox-voice.exe
```

1 以上なら同梱済み。dev 焼き込みの exe でも Rust 側 (フック・録音・貼付) は正常に動くため、
**「起動した」だけでは気づけない**。

### `WH_KEYBOARD_LL` は OS に無言で外される

[MSDN 原文](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc):

> If the hook procedure times out, the system passes the message to the next hook. However,
> **on Windows 7 and later, the hook is silently removed without being called.
> There is no way for the application to know whether the hook is removed.**

**タイムアウトの計測対象はコールバックの実行時間ではなく、フックスレッドへのメッセージ往復全体。**
つまりコールバックがいくら速くても、**高 CPU 負荷でスレッドがスケジュールされなければ外される**。
`THREAD_PRIORITY_TIME_CRITICAL` でも免除されない (免除の一次情報は存在しない)。

- **`cargo build` や E2E と並行してアプリを動かすとフックが死ぬ。** 実機確認の前にビルドを終わらせること
- 番犬 (`spawn_hook_watchdog`) が 15 秒周期で生存確認し再設置するが、
  [Microsoft の事例記事](https://learn.microsoft.com/it-it/archive/blogs/alejacma/global-hooks-getting-lost-on-windows-7)
  では**定期再設置でも救えず Raw Input への移行で解決**している
- `SendInput` は **UIPI でブロックされても戻り値でも `GetLastError` でも判別できない**。
  昇格ウィンドウが前景だと「生きているフックを死んだと誤判定」しうる。
  再設置ログには前景プロセス名を添えてある (切り分けの手掛かりはそこにしかない)

**中期的な移行候補**: Raw Input (`RegisterRawInputDevices` + `WM_INPUT`)。
MSDN が公式に推奨しており、タイムアウト無言除去の仕組みが無い。
このアプリはキーを一切抑制しないので LL フックである必然性が無い。
ただし `WM_INPUT` に `dwExtraInfo` 相当が無く、**自己注入の判別を再設計する必要がある**。
詳細は `design.md` の該当節。

---

## 6. 実機でしか確かめられないこと (未実施)

優先度順:

1. **キー捕獲が物理キーで動くか** — 最初に確認すべきこと。§3 の修正は疑似テストでしか
   確かめていない。設定ウィンドウを前面にして「キーを押して設定」→ 左Ctrl + F13 等を押して離す。
   あわせて Space 単独 / Enter で「拒否理由が出て捕獲が続く」ことも見る
2. **画像込みの Gemini 往復** — 画面質問モードで唯一の未検証経路。**この機能の成否はここに懸かっている** (下記実測を参照)
3. **昇格ケース** (管理者起動の 1 個目 + 通常起動の 2 個目) — UAC 同意が要るため自動化不可。
   `ERROR_ACCESS_DENIED` が実際に返るかは未実測
4. **`npm run e2e:hotkey`** の完走 (§3 の修正後)
5. 画像込みの Gemini 往復時間 (画面質問モード)。テキストのみで 772ms の実測はあるが、
   PNG が乗ると数秒に伸びうる
6. 見た目の目視 — ダークでのレールと地の境界、sticky 保存バーと最後の入力欄の重なり

---

## 6-2. 実測 (2026-08-29)

### `live_screen_scan` — 走査は速い。UIA はほとんど読めない

```
モニタ : 前景ウィンドウのモニタ
所要   : 144.1ms
アプリ                   経路            位置          文字数
Typeless.exe          ValuePattern   中央下         105
OpenCode.exe          要素名          画面ほぼ全体    0
Taskmgr.exe           要素名          中央中段       0
画像   : 1536x864 / 351 KB
```

- **所要 144ms。** 4 秒の予算に対して余裕がある。時間予算の懸念は解消
- **Electron 系 (OpenCode) は `要素名` で 0 文字。** design.md Q1 の予測どおり。
  **つまり UIA はこの用途でほとんど当てにならず、実質スクリーンショットが本体になる**
- **スクショの fallback は動いている** (1536x864 / 351KB が実際に生成された)。
  `GetDIBits` を選択解除前に呼んでいた不具合 (P2-a) の修正が効いていることの傍証

### `live_gemini_screen_ask` — テキスト経路は通る

```
回答: - 認証まわりの調査 / - 履歴DBの移行 / - オーバーレイの再設計   (1375 ms)
```

インジェクション耐性のテストも合格 (画面テキストに混ぜた命令に従わず、本来の一覧を返した)。
**ただしどちらも画像を含まない。** 画像込みの往復は**未検証**。

### 残る最大の未知

UIA が読めない以上、実運用では毎回スクリーンショットが送られる。
**1536px に縮小した画面から Gemini が細い等幅フォント (ターミナル・エディタのペイン) を
読み切れるかどうかが、この機能が使い物になるかを決める。** 読めなければ
縮小上限を上げる (送信量とのトレードオフ) か、ウィンドウ単位のキャプチャに戻す判断が要る。

---

## 7. 開発の進め方 (この作業で採っていた体制)

- **実装 = Opus 5 のサブエージェント / フェーズ間レビュー = Fable のサブエージェント**
- レビューは**指摘のみでファイルを編集しない**。実装セッションへ差し戻して直す
- UI を触る作業のレビューゲートには `/impeccable audit` を含める
- 実機 E2E が回せない状況では疑似テスト (`npm run sim`) を回す

---

## 8. リポジトリに含めていないファイル

`.opencode/` / `LOOP.md` / `clippy_output.json` / `workflow.config.json` は
別ツールの作業台帳と生成物。**移設時に持っていく必要はない** (`.gitignore` にも入れていない。
必要なら移設先で判断)。
