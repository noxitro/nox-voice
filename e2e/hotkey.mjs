// ホットキー経路の E2E。実アプリを起動し、実キー相当の合成入力を送って
// 「ログに何が出たか」で判定する。
//
// 前提と限界:
// - 送るキーは SendInput 由来なので LLKHF_INJECTED が立つ。hook が
//   「自プロセスの dwExtraInfo マーカーだけを弾く」形になっていることが前提。
//   一律 LLKHF_INJECTED 除外に戻すと、このテストは**全滅する**(陽性コントロール)。
// - 合成キーは前景アプリにも流れる。トリガーには物理キーボードに存在しない
//   F13..F16 を使い、さらに nox-voice 自身のウィンドウを前景にしてから送る。
// - 設定ファイルは実物 (app_config_dir) を使う。開始時に退避し、終了時に必ず戻す。
//
// 2026-08-28: キー捕獲は DOM の keydown/keyup へ移った。捕獲系のテスト
// (T3 / T7 / T10) は SendInput ではなく **CDP から webview へキーを入れる**
// (`Cdp#key` の doc)。前景を取れない環境でも判定できるので、これらは
// injectionReachesHook に依らない (SKIP にならない)。逆に「物理キーが
// OS → WebView2 → DOM と届くか」はここでは測れない。docs/hotkey-e2e.md 参照。
//
// 環境変数:
// - NOX_E2E_EXE      測る exe (既定は src-tauri/target/debug/nox-voice.exe)。
//                    CI はリリースビルドを渡す
// - NOX_E2E_NO_AUDIO 録音デバイスの無い環境 (GitHub Actions の Windows
//                    ランナー等) で 1 にする。下の NO_AUDIO の doc を参照
// - NOX_E2E_CDP_PORT WebView2 のデバッグポート (既定は毎回ランダム)。管理者権限で
//                    走らせるときは環境変数での指定が無視されるので、HKLM の
//                    ポリシーで同じポートを渡す (docs/hotkey-e2e.md)
// - NOX_E2E_NO_INJECTION 合成入力がフックまで届かない環境で 1 にする。
//                    下の NO_INJECTION の doc を参照 (e2e/probe-input.ps1 で判定できる)
//
// 使い方: node e2e/hotkey.mjs
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

import { describeCdpFailure } from "./cdp-diagnose.mjs";


// 毎回ポートを変える。固定にすると前回の TIME_WAIT / 生き残りブラウザに
// 引っかかって「デバッグポートが開かない」run が混ざる。
// NOX_E2E_CDP_PORT があればそれを使う (管理者権限で走る CI はポートを HKLM の
// ポリシーで決め打ちで渡すため。docs/hotkey-e2e.md の「CI で走らせる」)。
const CDP_PORT = Number(process.env.NOX_E2E_CDP_PORT) || 9400 + Math.floor(Math.random() * 400);
const APP_DIR = path.join(process.env.APPDATA, "com.noxitro.nox-voice");
const CONFIG = path.join(APP_DIR, "config.json");
const CONFIG_BACKUP = path.join(APP_DIR, "config.json.e2e-backup");
const LOG = path.join(process.env.LOCALAPPDATA, "com.noxitro.nox-voice", "logs", "nox-voice.log");
const REPO = path.resolve(import.meta.dirname, "..");
const EXE = process.env.NOX_E2E_EXE || path.join(REPO, "src-tauri", "target", "debug", "nox-voice.exe");
const SEND_KEYS = path.join(REPO, "e2e", "send-keys.ps1");

/** 録音が本当に始まったときの行。`録音開始 [貼り付け] (挿入先: …)`。
 *
 * 単に「録音開始」で探してはいけない。フォーカス診断 (debug) が
 * `audio::start` の**前**に `[focus] 録音開始 直前` を出すので、録音デバイスが
 * 無くて開始に失敗した run でも「開始した」と読めてしまう。 */
const STARTED = "録音開始 [";
/** 録音デバイスが無いときに、同じ押下で出る行 (audio.rs の NoInputDevice)。 */
const NO_DEVICE = "録音を開始できません: 録音デバイスが見つかりません";

/** 録音デバイスの無い環境で走らせる (`NOX_E2E_NO_AUDIO=1`)。
 *
 * デバイスが無いと {@link STARTED} は永久に出ない。それでもホットキーが
 * フックに届いたことは、同じ押下で出る {@link NO_DEVICE} で分かる。
 * このモードではその行を「発火した」証拠として数え、録音の確定を要する
 * 判定 (T1 の確定側 / T11 / T12) は**測らない**。測っていないことは
 * 結果の行と最後の集計に必ず書く。
 *
 * 指定しないまま録音デバイスの無い環境で走らせると T1 が FAIL になり、
 * 理由としてこの変数を案内する (フックの故障と取り違えないため)。 */
const NO_AUDIO = process.env.NOX_E2E_NO_AUDIO === "1";

/** 合成入力 (SendInput) がフックまで届かない環境で走らせる (`NOX_E2E_NO_INJECTION=1`)。
 *
 * GitHub Actions の Windows ランナーがそう: アプリの番犬が、自分で送った生存確認の
 * キーすら観測できない (2026-09-26 のログ)。OS だけで同じことを確かめるのが
 * e2e/probe-input.ps1 で、CI はその結果でこのモードを決める。
 *
 * このモードではキーを送らない。キー送出を要するテストは、結果を見ずに SKIP と書く
 * (陰性側のテストも「何も起きなかった」を合格と読まない — 送っていないのだから)。
 * 測るのは CDP から DOM へキーを入れる捕獲系 (T3 / T7 / T10)、再起動後の設定の
 * 読み込み (T6b)、多重起動 (T8)。SKIP は集計に出すが、モードを指定した時点で
 * 測らないと決めてあるので、それだけでは終了コードを落とさない。 */
const NO_INJECTION = process.env.NOX_E2E_NO_INJECTION === "1";

const VK = {
  LCTRL: 0xa2,
  LSHIFT: 0xa0,
  SPACE: 0x20,
  F13: 0x7c,
  F14: 0x7d,
  F15: 0x7e,
  ESC: 0x1b,
  // 既定のホットキー。単独 Alt の回帰 (T11/T12) に使う。
  RALT: 0xa5,
};

const results = [];
/** 合成入力がフックまで届く環境か。届かない run は「失敗」ではなく「欠測」。 */
let injectionReachesHook = !NO_INJECTION;
/** 開始時に設定ファイルがあったか。無かったなら終了時に「無い」へ戻す。
 * 確かめる前に落ちたときに本物の設定を消さないよう、既定は true。 */
let hadConfig = true;
let logOffset = 0;
let app = null;
let vite = null;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function record(name, ok, detail, needsInjectedKeys = true) {
  // 合成入力がフックに届かない環境では、キーを要するテストは判定できない。
  // ここを FAIL にすると「アプリが壊れている」と読めてしまうので SKIP と書く
  // (環境要因の不成立は失敗ではなく欠測)。
  const notMeasured = needsInjectedKeys && NO_INJECTION;
  const verdict = notMeasured ? "SKIP" : ok ? "PASS" : needsInjectedKeys && !injectionReachesHook ? "SKIP" : "FAIL";
  if (notMeasured) detail = "合成キー無しモードでは測らない (キーを送っていない)";
  results.push({ name, verdict, detail });
  console.log(`${verdict}  ${name}${detail ? `\n        ${detail}` : ""}`);
  if (verdict === "FAIL") {
    // 落ちたときは「何が起きなかったか」だけでなく「何が起きたか」を出す。
    const tail = readNewLog().split(/\r?\n/).filter(Boolean).slice(-15);
    console.log(tail.map((l) => `        | ${l}`).join("\n"));
  }
}

// --- ログ --------------------------------------------------------------------

function readNewLog() {
  if (!fs.existsSync(LOG)) return "";
  const buf = fs.readFileSync(LOG);
  const text = buf.subarray(Math.min(logOffset, buf.length)).toString("utf8");
  return text;
}
function markLog() {
  logOffset = fs.existsSync(LOG) ? fs.statSync(LOG).size : 0;
}
/** ログに `needle` が現れるまで待つ。現れなければ null (= 不在の証拠)。 */
async function waitForLog(needle, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const text = readNewLog();
    const hit = text.split(/\r?\n/).find((l) => l.includes(needle));
    if (hit) return hit;
    if (Date.now() > deadline) return null;
    await sleep(150);
  }
}
/** ホットキーが発火した証拠を待つ。陽性・陰性の両方の判定に使う。
 *
 * 通常は録音が本当に始まった行だけを数える。録音デバイスの無いモード
 * (NO_AUDIO) では、同じ押下で出る開始失敗の行も数える。 */
async function waitForFired(timeoutMs) {
  if (NO_INJECTION) return null; // キーを送っていないので待っても来ない
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const hit = readNewLog()
      .split(/\r?\n/)
      .find((l) => l.includes(STARTED) || (NO_AUDIO && l.includes(NO_DEVICE)));
    if (hit) return hit;
    if (Date.now() > deadline) return null;
    await sleep(150);
  }
}
/** 発火のあと、録音が確定するまで待つ (次のテストへ録音を持ち越さないため)。
 * 録音デバイスの無いモードでは確定するものが無いので、落ち着くのだけ待つ。 */
async function settleRecording() {
  if (NO_INJECTION) return null;
  if (NO_AUDIO) {
    await sleep(800);
    return null;
  }
  return waitForLog("録音確定", 8000);
}

function restoreConfig() {
  // 有無を確かめてから写すと、その間に変わりうる (CodeQL: file system race)。
  // 退避を元の名前へ移して置き換え、無ければ ENOENT で分かる。
  try {
    fs.renameSync(CONFIG_BACKUP, CONFIG);
    console.log("\n設定ファイルを元に戻しました");
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
    if (!hadConfig) {
      fs.rmSync(CONFIG, { force: true });
      console.log("\n開始前は設定ファイルが無かったので、テストで作ったものを消しました");
    }
  }
}

// --- キー送出 ----------------------------------------------------------------

function sendKeys(steps, focusPid) {
  if (NO_INJECTION) return "";
  const args = ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", SEND_KEYS, "-Steps", steps];
  if (focusPid) args.push("-FocusPid", String(focusPid));
  const r = spawnSync("powershell.exe", args, { encoding: "utf8" });
  if (r.status !== 0) throw new Error(`send-keys 失敗: ${r.stderr || r.stdout}`);
  const out = r.stdout.trim();
  if (process.env.NOX_E2E_VERBOSE) console.log(`        [keys] ${out.replace(/\s+/g, " ")}`);
  return out;
}

// --- CDP ---------------------------------------------------------------------

/** 設定画面 (`#hotkey-capture` を持つ page) の CDP に接続する。
 *
 * URL で選ばない。オーバーレイと設定画面はどちらも `tauri.localhost` 配下で、
 * URL だけでは取り違える。「操作したい要素があるページ」を実際に評価して選ぶ。 */
async function connectSettingsPage() {
  const deadline = Date.now() + 30000;
  let lastError = null;
  for (;;) {
    try {
      const res = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
      const targets = (await res.json()).filter((t) => t.type === "page" && t.webSocketDebuggerUrl);
      for (const t of targets) {
        const cdp = await Cdp.connect(t.webSocketDebuggerUrl);
        await cdp.send("Runtime.enable");
        const found = await cdp.eval(`Boolean(document.getElementById("hotkey-capture"))`);
        if (found) return cdp;
        cdp.ws.close();
      }
    } catch (e) {
      // まだ上がっていないことが多い。繋がらずに終わったときの材料として控える。
      lastError = e;
    }
    if (Date.now() > deadline) {
      throw new Error(
        `設定画面の webview に CDP 接続できない\n${await describeCdpFailure(CDP_PORT, lastError)}`,
      );
    }
    await sleep(500);
  }
}

class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(ev.data);
      const p = this.pending.get(msg.id);
      if (p) {
        this.pending.delete(msg.id);
        msg.error ? p.reject(new Error(JSON.stringify(msg.error))) : p.resolve(msg.result);
      }
    });
  }
  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((res, rej) => {
      ws.addEventListener("open", res, { once: true });
      ws.addEventListener("error", rej, { once: true });
    });
    return new Cdp(ws);
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  async eval(expr) {
    const r = await this.send("Runtime.evaluate", {
      expression: expr,
      awaitPromise: true,
      returnByValue: true,
    });
    if (r.exceptionDetails) throw new Error(`評価失敗: ${JSON.stringify(r.exceptionDetails)}`);
    return r.result.value;
  }

  /** この接続で「キーを送る手段」を 1 度だけ実測して決める。
   *
   * # なぜ推測で決め打ちしないのか (2026-08-28、実測で踏んだ)
   *
   * `Input.dispatchKeyEvent` はレンダラの入力パイプラインを通るので本来は
   * こちらが望ましい。ところが WebView2 では **`code` が空のまま DOM へ届く**
   * ことが実測で判明した。捕獲は `event.code` だけを見ているので、Esc が
   * "Unidentified" になって取り消しが効かず、T7 が捕獲を開いたまま素通りし、
   * その残骸が次の T10 を巻き添えにして落とした (原因の切り分けに丸ごと
   * 1 往復かかった)。
   *
   * そこで**実際に 1 打送って `code` が届くかを確かめる**。届かなければ
   * `KeyboardEvent` の合成へ落とす。どちらを使ったかは結果に必ず出す —
   * 「どの経路で緑になったのか」が分からない緑は信用できない。
   *
   * 捕獲が始まる前に呼ぶこと (プローブのキーが捕獲へ混ざらないように)。 */
  async detectKeyTransport() {
    if (this.transport) return this.transport;
    this.transport = await (async () => {
      try {
        await this.eval(
          `window.__noxProbe = "(未着)";` +
            `window.__noxProbeHandler = (e) => { window.__noxProbe = e.code === "" ? "(空文字)" : e.code; };` +
            `window.addEventListener("keydown", window.__noxProbeHandler, true); true`,
        );
        await this.send("Input.dispatchKeyEvent", {
          type: "rawKeyDown",
          code: "F13",
          key: "F13",
          windowsVirtualKeyCode: 0x7c,
          nativeVirtualKeyCode: 0x7c,
        });
        const seen = await this.eval(`window.__noxProbe`);
        await this.eval(
          `window.removeEventListener("keydown", window.__noxProbeHandler, true); true`,
        );
        if (seen === "F13") return { how: "cdp", label: "Input.dispatchKeyEvent" };
        return { how: "synthetic", label: `KeyboardEvent 合成 (CDP は code=${seen})` };
      } catch (e) {
        return { how: "synthetic", label: `KeyboardEvent 合成 (Input domain 不可: ${e.message})` };
      }
    })();
    return this.transport;
  }

  /** 設定 UI へキーを 1 打送る (押す or 離す)。
   *
   * # なぜ SendInput ではなく CDP なのか (2026-08-28)
   *
   * キー捕獲は DOM の `keydown` / `keyup` で行うようになった
   * (`docs/design.md`「キー捕獲を DOM イベントへ移す」)。DOM へ届くには
   * **アプリのウィンドウが前景でなければならない**が、このハーネスは
   * バックグラウンドのコンソールから起動するので前景を取れないことが多い。
   * `SendInput` の合成キーは前景のアプリへ行ってしまい、捕獲には届かない。
   *
   * **測れないこと**: 物理キーが OS → WebView2 → DOM と届くこと自体。
   * ここは前景を取れる実機でしか確かめられない (docs/hotkey-e2e.md)。 */
  async key(type, code, vk) {
    const transport = await this.detectKeyTransport();
    if (transport.how === "cdp") {
      await this.send("Input.dispatchKeyEvent", {
        type: type === "down" ? "rawKeyDown" : "keyUp",
        code,
        key: code,
        windowsVirtualKeyCode: vk,
        nativeVirtualKeyCode: vk,
      });
      return transport.label;
    }
    await this.eval(
      `window.dispatchEvent(new KeyboardEvent(${JSON.stringify(type === "down" ? "keydown" : "keyup")},` +
        ` { code: ${JSON.stringify(code)}, bubbles: true, cancelable: true }))`,
    );
    return transport.label;
  }
}

// --- アプリの起動と停止 ------------------------------------------------------

/** デバッグビルドは frontendDist ではなく devUrl (http://localhost:1420) を見る。
 *
 * vite が上がっていないと**画面は真っ白のまま起動する**。エラーも出ないので、
 * 「ボタンが見つからない」まで進んで初めて分かる。先にここで担保しておく。 */
async function startVite() {
  const alive = await fetch("http://localhost:1420/").then(() => true).catch(() => false);
  if (alive) return null;
  // npm.cmd は shell 経由でないと Node 20+ で EINVAL になる。
  const proc = spawn("npm.cmd", ["run", "dev"], { cwd: REPO, stdio: "ignore", shell: true });
  const deadline = Date.now() + 30000;
  for (;;) {
    if (await fetch("http://localhost:1420/").then(() => true).catch(() => false)) return proc;
    if (Date.now() > deadline) throw new Error("vite dev サーバーが上がらない");
    await sleep(500);
  }
}

function killApp() {
  spawnSync("taskkill.exe", ["/F", "/IM", "nox-voice.exe"], { encoding: "utf8" });
  // WebView2 のブラウザプロセスは nox-voice を落としても生き残ることがある。
  // 生き残りがいると次の起動はそこへ**相乗り**し、--remote-debugging-port を
  // 付けずに作られた古いブラウザに繋がる = CDP が開かない。
  // 他アプリの WebView2 を巻き添えにしないよう、user-data-folder が
  // 本アプリのものだけを落とす。
  spawnSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-Command",
      "Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | " +
        "Where-Object { $_.CommandLine -like '*com.noxitro.nox-voice*' } | " +
        "ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }",
    ],
    { encoding: "utf8" },
  );
}

async function startApp() {
  markLog();
  app = spawn(EXE, [], {
    env: {
      ...process.env,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
      RUST_LOG: "info",
      // フォーカス診断 (focus_probe) は debug なので、既定の Info では 1 行も
      // 出ない。T11/T12 はその行を読むので、E2E では明示的に上げる。
      // ここを消すと T11/T12 が「証拠が無い」で落ちる (アプリの不具合ではない)。
      NOX_VOICE_LOG: "debug",
    },
    stdio: "ignore",
    detached: false,
  });
  const line = await waitForLog("キーボードフックを設置", 30000);
  if (!line) throw new Error("アプリが起動しない (フック設置のログが出ない)");
  // フック設置のログが出てからメッセージループが回り始めるまでの隙間を埋める。
  // ここを固定 sleep で済ませると、送ったキーが誰にも見られずに消える。
  // 4 秒は実測値。1.5 秒だと T1 が run ごとに落ちたり通ったりした
  // (アプリ側の不具合ではなく、フック設置ログの直後はまだ取りこぼす)。
  await sleep(Number(process.env.NOX_E2E_WARMUP ?? 4000));
  return line;
}

function readConfig() {
  return JSON.parse(fs.readFileSync(CONFIG, "utf8"));
}

/** ログ中の「単独 Alt 対策の打鍵 累計 N 回」の最後の N。無ければ 0。 */
function lastLoneAltBreaks(text) {
  const hits = [...text.matchAll(/単独 Alt 対策の打鍵 累計 (\d+) 回/g)];
  return hits.length ? Number(hits[hits.length - 1][1]) : 0;
}

// --- シナリオ ----------------------------------------------------------------

async function main() {
  if (!fs.existsSync(EXE)) throw new Error(`ビルドが無い: ${EXE} (cargo build を先に)`);
  console.log(
    [
      "注意: このテストは実キー相当の合成入力を送ります。前景奪取は成功しないことが多く、",
      "      送ったキー (Space / Ctrl+Space / F13〜F15) は**そのとき前景のアプリにも届きます**。",
      "      実行中はキーボードに触れず、編集中のウィンドウを前面に置かないでください。",
      "",
    ].join("\n"),
  );

  killApp();
  await sleep(500);
  vite = await startVite();

  // 実設定を退避し、テスト用に差し替える。injection を切るのは、
  // 合成キーで始まった録音の結果がユーザーの作業中ウィンドウへ貼られないため。
  // 新しいマシン (CI のランナーなど) では設定ファイルがまだ無い。アプリは
  // 欠けた項目を既定値で補う (#[serde(default)]) ので、空から組み立てて良い。
  // 有無は写してみて決める (確かめてから写すと、その間に変わりうる)。
  try {
    fs.copyFileSync(CONFIG, CONFIG_BACKUP);
    hadConfig = true;
  } catch (e) {
    if (e.code !== "ENOENT") throw e;
    hadConfig = false;
  }
  const base = hadConfig ? JSON.parse(fs.readFileSync(CONFIG_BACKUP, "utf8")) : {};
  fs.mkdirSync(APP_DIR, { recursive: true });
  const testCfg = {
    ...base,
    injection_enabled: false,
    start_hidden: false,
    history_enabled: false,
    overlay_enabled: false,
    hotkey_vk: VK.SPACE,
    hotkey_mods: [VK.LCTRL],
  };
  fs.writeFileSync(CONFIG, JSON.stringify(testCfg, null, 2));

  try {
    const hookLine = await startApp();
    console.log(`起動: ${hookLine.trim()}`);
    const pid = app.pid;

    let cdp = await connectSettingsPage();
    // どの経路でキーを入れるかを**実測してから**始める (Cdp#detectKeyTransport)。
    // 捕獲が始まる前にやること — プローブのキーが捕獲へ混ざらないように。
    const transport = await cdp.detectKeyTransport();
    record("T0 設定画面へ CDP 接続", true, `捕獲系のキー経路: ${transport.label}`, false);

    // --- T1: 既定の組み合わせ (左Ctrl+Space) の長押しで録音が始まり、離すと確定する
    //   (録音デバイスの無いモードでは「発火した」までを見る)
    markLog();
    sendKeys(`down:A2,down:20,sleep:800,up:20,up:A2`, pid);
    const started = await waitForFired(5000);
    const finalized = await settleRecording();
    // T1 は較正も兼ねる。ここが通らない環境では、以降のキー依存テストは
    // 「アプリの不具合」ではなく「合成入力がフックへ届いていない」ので判定不能。
    // 実機の物理キーでは動くのにここだけ落ちる run が実在する
    // (エージェントのサンドボックス下で起動した場合など)。
    injectionReachesHook = Boolean(started);
    // 録音デバイスが無いだけなら、フックは生きている。取り違えないよう別に言う。
    const noDevice = !started && readNewLog().includes(NO_DEVICE);
    record(
      "T1 既定 左Ctrl+Space の長押し PTT",
      Boolean(started && (NO_AUDIO || finalized)),
      started
        ? `開始=${started.trim()}${NO_AUDIO ? " (録音デバイス無しモード: 確定は測っていない)" : ""}`
        : noDevice
          ? "ホットキーは発火したが、録音デバイスが無くて録音できない。" +
            "フック経路だけを測るなら NOX_E2E_NO_AUDIO=1 で走らせること"
          : "「録音開始」がログに出ない — 合成入力がフックへ届いていない可能性が高い。" +
            "物理キーボードで同じ操作を試して切り分けること",
      NO_INJECTION,
    );
    if (!injectionReachesHook) {
      console.log(
        [
          "",
          NO_INJECTION
            ? "  ※ 合成キー無しモード (NOX_E2E_NO_INJECTION=1): キーは送りません。"
            : noDevice
              ? "  ※ この環境には録音デバイスがありません (NOX_E2E_NO_AUDIO=1 で測れます)。"
              : "  ※ この環境では合成入力がフックに届きません。",
          "  以降のキー依存テストは SKIP 扱いにします。",
          "",
        ].join("\n"),
      );
    }

    // --- T2: 修飾キー無しで Space だけ押しても発火しない (陰性側)
    markLog();
    sendKeys(`down:20,sleep:600,up:20`, pid);
    const spuriousSpace = await waitForFired(2500);
    record(
      "T2 Space 単独では発火しない",
      spuriousSpace === null,
      spuriousSpace ? `誤発火: ${spuriousSpace.trim()}` : "",
    );

    // --- T3: 捕獲 UI で 左Ctrl+F14 を設定できる
    //
    // キーは CDP から webview へ入れる (Cdp#key の doc)。捕獲は DOM 経由に
    // なったので、ここは**フックの生死と無関係に**通らなければならない。
    // 逆に言えば、T1 が SKIP の環境でも T3 は判定できる (needsInjectedKeys=false)。
    markLog();
    await cdp.eval(`document.getElementById("hotkey-capture").click()`);
    const capStart = await waitForLog("キー捕獲モード: 開始", 5000);
    const keyPath = await cdp.key("down", "ControlLeft", VK.LCTRL);
    await cdp.key("down", "F14", VK.F14);
    await sleep(200);
    await cdp.key("up", "F14", VK.F14);
    await cdp.key("up", "ControlLeft", VK.LCTRL);
    const capEnd = await waitForLog("キー捕獲モード: 終了", 5000);
    await sleep(500);
    const cfgAfter = readConfig();
    const label = await cdp.eval(`document.getElementById("hotkey-label").textContent`);
    const captured = cfgAfter.hotkey_vk === VK.F14 && cfgAfter.hotkey_mods.includes(VK.LCTRL);
    record(
      "T3 捕獲 UI (DOM の keydown/keyup) で 左Ctrl+F14 を設定",
      Boolean(capStart && capEnd && captured),
      `経路=${keyPath} / config: vk=0x${cfgAfter.hotkey_vk.toString(16)} mods=${JSON.stringify(cfgAfter.hotkey_mods)} / UI ラベル="${label}"`,
      false,
    );

    // --- T4: 設定した新しい組み合わせが実際に効く
    markLog();
    sendKeys(`down:A2,down:7D,sleep:800,up:7D,up:A2`, pid);
    const newStarted = await waitForFired(5000);
    await settleRecording();
    record(
      "T4 設定した 左Ctrl+F14 で録音が始まる",
      Boolean(newStarted),
      newStarted ? "" : "設定は保存されたが、押しても発火しない",
    );

    // --- T5: 古い組み合わせはもう効かない (陰性側)
    markLog();
    sendKeys(`down:A2,down:20,sleep:600,up:20,up:A2`, pid);
    const oldStill = await waitForFired(2500);
    record(
      "T5 旧 左Ctrl+Space はもう効かない",
      oldStill === null,
      oldStill ? `旧ホットキーが生きている: ${oldStill.trim()}` : "",
    );

    // --- T6: 再起動しても設定が残る
    killApp();
    await sleep(1000);
    const restartHookLine = await startApp();
    cdp = await connectSettingsPage();
    await cdp.detectKeyTransport();
    const persisted = readConfig();
    // --- T6b: 再起動後、保存された組み合わせでフックが張られる (キー送出に依らない)
    //   「効く」(T6) は合成キーが要るが、Rust が保存値を読んでフックを張ったことは
    //   起動ログのホットキー表示で分かる。
    record(
      "T6b 再起動後、保存された 左Ctrl+F14 でフックが設置される",
      persisted.hotkey_vk === VK.F14 && restartHookLine.includes("左 Ctrl + F14"),
      `config: vk=0x${persisted.hotkey_vk.toString(16)} / 起動ログ: ${restartHookLine.trim()}`,
      false,
    );
    markLog();
    sendKeys(`down:A2,down:7D,sleep:800,up:7D,up:A2`, app.pid);
    const afterRestart = await waitForFired(6000);
    await settleRecording();
    record(
      "T6 再起動後も 左Ctrl+F14 が効く",
      Boolean(afterRestart) && persisted.hotkey_vk === VK.F14,
      `config: vk=0x${persisted.hotkey_vk.toString(16)} mods=${JSON.stringify(persisted.hotkey_mods)}`,
    );

    // --- T7: 捕獲を Esc で取り消すと設定が変わらない
    markLog();
    const before = readConfig();
    await cdp.eval(`document.getElementById("hotkey-capture").click()`);
    await waitForLog("キー捕獲モード: 開始", 5000);
    await cdp.key("down", "Escape", VK.ESC);
    await cdp.key("up", "Escape", VK.ESC);
    await sleep(800);
    const after = readConfig();
    const cancelled = await waitForLog("キー捕獲モード: 終了", 3000);
    record(
      "T7 Esc で捕獲を取り消すと設定は変わらない",
      Boolean(cancelled) &&
        after.hotkey_vk === before.hotkey_vk &&
        JSON.stringify(after.hotkey_mods) === JSON.stringify(before.hotkey_mods),
      `vk=0x${after.hotkey_vk.toString(16)} mods=${JSON.stringify(after.hotkey_mods)}`,
      false,
    );

    // --- T10: 使えないキーは黙って失敗せず、理由を出して捕獲を続ける
    //
    // DOM 方式では「押したのに何も起きない」が一番ありがちな見え方になる。
    // 断る経路が生きていることを、Rust の判定ごと通しで確かめる。
    markLog();
    const beforeReject = readConfig();
    await cdp.eval(`document.getElementById("hotkey-capture").click()`);
    // 開始の確認は必須。ここを見ないと、前のテストが捕獲を開いたままだった
    // 場合に click が「畳んで終わり」になり、そのまま素通りで緑になる
    // (実測でこの取り違えが起きた)。
    const rejectCapStart = await waitForLog("キー捕獲モード: 開始", 5000);
    await cdp.key("down", "Enter", 0x0d);
    await cdp.key("up", "Enter", 0x0d);
    await sleep(600);
    const rejectBanner = await cdp.eval(
      `(document.getElementById("error")?.hidden === false) && document.getElementById("error").textContent`,
    );
    const afterReject = readConfig();
    const stillCapturing = await cdp.eval(
      `document.getElementById("hotkey-label").dataset.capturing === "true"`,
    );
    record(
      "T10 使えないキー (Enter) は理由が出て、設定は変わらず捕獲は続く",
      Boolean(rejectCapStart) &&
        Boolean(rejectBanner) &&
        /Enter/.test(String(rejectBanner)) &&
        stillCapturing === true &&
        afterReject.hotkey_vk === beforeReject.hotkey_vk,
      `開始=${Boolean(rejectCapStart)} / 帯="${rejectBanner}" / 捕獲継続=${stillCapturing}`,
      false,
    );
    // 捕獲を畳んでから次へ。開いたままだと T9 の 20 秒放置の間ずっと
    // フックが消音され、押しても録音が始まらない。
    await cdp.key("down", "Escape", VK.ESC);
    await cdp.key("up", "Escape", VK.ESC);
    await sleep(400);
    // --- T9: フックが無言で外されても自力で復帰する
    //
    // 起動直後にフックが OS に外されている run が実測で再現する。番犬が
    // 15 秒ごとに生存確認して再設置するので、しばらく置いてから押せば効くはず。
    // ここは「押した直後に効く」ではなく「放置したあとでも効く」を見る。
    markLog();
    if (!NO_INJECTION) await sleep(20000);
    sendKeys(`down:A2,down:7D,sleep:800,up:7D,up:A2`, app.pid);
    const afterIdle = await waitForFired(6000);
    const reinstalled = readNewLog().includes("再設置");
    record(
      "T9 20 秒放置したあともホットキーが効く (フック生存確認)",
      Boolean(afterIdle),
      reinstalled ? "この run ではフックが外れており、番犬が再設置した" : "フックは外れなかった",
    );

    // --- T8: 二重起動しない (フックが 2 本刺さると 1 回の押下で録音が 2 回始まる)
    const second = spawn(EXE, [], { stdio: "ignore" });
    await sleep(4000);
    const alive = spawnSync("powershell.exe", [
      "-NoProfile",
      "-Command",
      "(Get-Process -Name nox-voice -ErrorAction SilentlyContinue | Measure-Object).Count",
    ], { encoding: "utf8" }).stdout.trim();
    record(
      "T8 二重起動しても常駐は 1 プロセスだけ",
      alive === "1",
      `起動中の nox-voice.exe = ${alive} 個 (2 個目の exitCode=${second.exitCode})`,
      false,
    );

    // T11 / T12 は録音の確定 (停止側のログ) と小窓の表示区間を要する。
    // 録音デバイスの無いモードでは測れないので実行しない。**測っていないことは
    // 最後の集計に書く** (SKIP の行を積むと run 全体が落ちるが、ここは
    // モードを指定した時点で測らないと決めてある)。
    if (NO_AUDIO || NO_INJECTION) {
      console.log("--    T11 / T12 は録音と合成キーを要するので、このモードでは実行しない");
    } else {
      // --- T11 / T12: 単独 Alt のホットキーとフォーカス診断 (2026-08-29 の回帰)
      //
      // 何を検証しているか:
      //   T11 = トリガーが Alt 単独 (既定の右 Alt) でも PTT が壊れないこと。
      //         フックは**トリガーを離すたび**に VK_NONAME を 1 打撒く
      //         (break_lone_alt)。押下の辺では撒かない — 低レベルフックは
      //         前景アプリより先に走るので、押下時に撒くとダミーが Alt-down を
      //         追い越し、アプリから見た押下〜離しの間が空のままになる
      //         (2026-08-29 に一度そう書いて効かなかった。design.md の Q7)。
      //         この打鍵が自分のホットキー解釈へ混ざると、押しても録音が
      //         始まらなくなる — つまり修正そのものの陰性側の確認。
      //         撒いたことは `[focus] 単独 Alt 対策の打鍵 累計 N 回` で見る
      //         (フックからはログを出せないので、数字を後から読む形にしてある)。
      //         この行は**録音の停止側** (request_finalize) で出る。撒く辺が
      //         離しなので、開始時に読むと必ず「撒く前」の値になるため。
      //         したがって計測は「録音確定を待ってから読む」順でなければならない。
      //   T12 = 小窓の表示・非表示でキーボードフォーカスが動かないこと。
      //         focus_probe が違反を見つけたら warn を出すので、その不在を見る。
      //
      // 何を検証**できていないか** (ここが本題なので必ず読むこと):
      //   - **ブラウザの入力欄でキャレットが残るか**は測れていない。元の不具合は
      //     「Chrome がツールバーへフォーカスを移し、ページの caret が消える」で、
      //     これは Chrome の中の話なので Win32 の API (GetForegroundWindow /
      //     GetGUIThreadInfo) からは見えない。DOM の blur を見るしかなく、
      //     それには実ブラウザと実ページが要る。実機確認の手順は
      //     docs/design.md の Q7 節を参照。
      //   - 貼付 (Ctrl+V) 経路の診断も出ない。この E2E は API キーを与えないので
      //     STT が失敗し、注入まで到達しない。
      //   - 合成キーがフックへ届かない環境では T11 は SKIP になる (T1 と同じ較正)。
      //     T12 は録音を伴わないので、フックに依らず判定できる。
      killApp();
      await sleep(1000);
      const altCfg = {
        ...readConfig(),
        hotkey_vk: VK.RALT,
        hotkey_mods: [],
        // 小窓を出さないと表示・非表示の区間が測れない (T12 の前提)。
        overlay_enabled: true,
      };
      fs.writeFileSync(CONFIG, JSON.stringify(altCfg, null, 2));
      await startApp();
      markLog();
      const breaksBefore = lastLoneAltBreaks(readNewLog());
      sendKeys(`down:A5,sleep:800,up:A5`, app.pid);
      const altStarted = await waitForLog(STARTED, 6000);
      const altFinalized = await waitForLog("録音確定", 8000);
      const altLog = readNewLog();
      const breaksAfter = lastLoneAltBreaks(altLog);
      record(
        "T11 単独 右Alt の PTT が効き、離しでダミーキーが 1 打撒かれる",
        Boolean(altStarted && altFinalized) && breaksAfter > breaksBefore,
        !altStarted
          ? "右 Alt を押しても録音が始まらない (撒いたダミーキーが自分の解釈へ混ざっている可能性)"
          : !altFinalized
            ? "右 Alt を離しても録音が確定しない"
            : breaksAfter > breaksBefore
              ? `ダミーキー累計 ${breaksBefore} → ${breaksAfter}`
              : `録音は通ったがダミーキーが撒かれていない (累計 ${breaksAfter} のまま。`
                + "撒く辺が離しから外れたか、停止側のログが出ていない)",
      );

      // 小窓の区間で違反 warn が出ていないこと。
      // 出ている場合はその行をそのまま detail に載せる — 「動いた」だけでは
      // どこで動いたのか分からないため。
      const focusViolation = altLog
        .split(/\r?\n/)
        .find((l) => l.includes("[focus]") && l.includes("フォーカスが動きました"));
      const sawOverlayProbe = altLog.includes("[focus] オーバーレイ表示");
      record(
        "T12 小窓の表示・非表示でキーボードフォーカスが動かない",
        sawOverlayProbe && !focusViolation,
        focusViolation
          ? `違反: ${focusViolation.trim()}`
          : sawOverlayProbe
            ? "オーバーレイ表示の区間を計測し、違反なし"
            : "診断行が 1 行も出ていない (NOX_VOICE_LOG=debug が効いていないか、小窓が無効)",
        false,
      );
    }

  } finally {
    killApp();
    vite?.kill();
    await sleep(300);
    restoreConfig();
  }

  const failed = results.filter((r) => r.verdict === "FAIL");
  const skipped = results.filter((r) => r.verdict === "SKIP");
  const passed = results.filter((r) => r.verdict === "PASS");
  console.log(
    `\n=== PASS ${passed.length} / FAIL ${failed.length} / SKIP ${skipped.length} (全 ${results.length}) ===`,
  );
  if (NO_AUDIO) {
    console.log(
      "録音デバイス無しモード: 録音の確定 (T1 の後半) と T11 / T12 は測っていない。実機で測り直すこと。",
    );
  }
  if (NO_INJECTION) {
    console.log(
      "合成キー無しモード: キー送出を要するテスト (SKIP の行) は測っていない。合成入力がフックへ届く環境か実機で測り直すこと。",
    );
  } else if (skipped.length) {
    console.log("SKIP は合格ではない。合成入力がフックへ届く環境で測り直すこと。");
  }
  // 合成キー無しモードの SKIP は、モードを指定した時点で測らないと決めたもの。
  process.exit(failed.length === 0 && (skipped.length === 0 || NO_INJECTION) ? 0 : 1);
}

main().catch((e) => {
  console.error(`E2E が異常終了: ${e.stack || e}`);
  killApp();
  vite?.kill();
  restoreConfig();
  process.exit(2);
});
