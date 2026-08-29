// メインウィンドウの疑似 E2E (simulated E2E)。**アプリを起動しない**。
//
// # 何をするか
//
// vite dev サーバに headless Edge で入り、`e2e/sim-ui-harness.html` 経由で
// `index.html` の DOM と `src/main.ts` をそのまま動かして、DOM を叩く。
// Rust 側は `window.__TAURI_INTERNALS__` の差し替えで模擬する
// (戻り値の型は ConfigView と各 #[tauri::command] に合わせてある)。
//
// # 実機 E2E (e2e/hotkey.mjs) との違い
//
// | | 実機 (hotkey.mjs) | 疑似 (これ) |
// |---|---|---|
// | アプリ | nox-voice.exe を起動 | 起動しない |
// | webview | WebView2 | headless Edge |
// | Rust | 本物 | モック |
// | キー入力 | SendInput の合成キー | 送らない (イベントをモックから発火) |
// | 前提 | 利用者の常駐アプリを taskkill する | 何も落とさない |
//
// **確かめられないこと**: invoke が本当に Rust へ届くか / コマンド名・引数名が
// Rust 側と一致しているか (モックなので綴りを間違えても通る) / グローバル
// ホットキーの捕獲 / WebView2 固有の描画差 / 実ウィンドウの最小サイズ制約。
// ここが緑でも、実機 E2E の代わりにはならない。**穴の位置が違うだけ**。
//
// # 副作用
//
// - vite が 5199 で上がっていなければ起動する。**終わっても止めない**
//   (利用者が見た目の確認に使っているため)。自分で起こした場合も止めない。
// - nox-voice.exe には触れない。taskkill もしない。
//
// 使い方: node e2e/sim-ui.mjs
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const REPO = path.resolve(import.meta.dirname, "..");
const VITE_PORT = Number(process.env.NOX_SIM_VITE_PORT ?? 5199);
const BASE = `http://localhost:${VITE_PORT}`;
const HARNESS = `${BASE}/e2e/sim-ui-harness.html`;
// 毎回ポートを変える。固定にすると前回の生き残りブラウザへ繋いでしまう。
const CDP_PORT = 9800 + Math.floor(Math.random() * 150);

const EDGE_CANDIDATES = [
  "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
  "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
];

const results = [];
let browser = null;
let profileDir = null;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function record(name, ok, detail) {
  const verdict = ok ? "PASS" : "FAIL";
  results.push({ name, verdict, detail });
  console.log(`${verdict}  ${name}${detail ? `\n        ${detail}` : ""}`);
}

/** 判定を 1 か所に集める。`fn` は `{ ok, detail }` を返す。 */
async function check(name, fn) {
  try {
    const { ok, detail } = await fn();
    record(name, ok, detail);
  } catch (e) {
    record(name, false, `例外: ${e.message ?? e}`);
  }
}

// --- CDP ---------------------------------------------------------------------

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
  /** ページ内で式を評価して値を返す。await も通る。 */
  async eval(expr) {
    const r = await this.send("Runtime.evaluate", {
      expression: expr,
      awaitPromise: true,
      returnByValue: true,
    });
    if (r.exceptionDetails) {
      throw new Error(
        r.exceptionDetails.exception?.description ?? JSON.stringify(r.exceptionDetails),
      );
    }
    return r.result.value;
  }
  /** 非同期の本体を書きやすくする糖衣。`body` は値を return する。 */
  run(body) {
    return this.eval(`(async () => { ${body} })()`);
  }
}

// --- vite --------------------------------------------------------------------

const alive = (url) => fetch(url).then(() => true).catch(() => false);

/**
 * vite を確保する。**止めない**。
 *
 * 呼び出し元が見た目の確認に使っていることがあるので、こちらの都合で
 * 落とさない。落ちていたときだけ起こす。
 */
async function ensureVite() {
  if (await alive(`${BASE}/`)) return "既存の vite に相乗り";
  // npm.cmd は shell 経由でないと Node 20+ で EINVAL になる。
  spawn("npx.cmd", ["vite", "--port", String(VITE_PORT), "--strictPort"], {
    cwd: REPO,
    stdio: "ignore",
    shell: true,
    detached: true,
  }).unref();
  const deadline = Date.now() + 40000;
  for (;;) {
    if (await alive(`${BASE}/`)) return `vite を ${VITE_PORT} で起動した (止めずに残す)`;
    if (Date.now() > deadline) throw new Error(`vite が ${VITE_PORT} で上がらない`);
    await sleep(500);
  }
}

// --- ブラウザ ----------------------------------------------------------------

function edgePath() {
  const hit = EDGE_CANDIDATES.find((p) => fs.existsSync(p));
  if (!hit) throw new Error(`msedge.exe が見つからない: ${EDGE_CANDIDATES.join(" / ")}`);
  return hit;
}

/**
 * headless Edge を起こす。**利用者のブラウザには触らない**。
 *
 * 使い捨てのプロファイルを渡すのが要点。既定プロファイルを使うと、
 * 開いている利用者の Edge に相乗りして CDP が開かず、しかも履歴やタブに
 * 手を出すことになる。
 */
async function startBrowser() {
  profileDir = fs.mkdtempSync(path.join(os.tmpdir(), "nox-sim-ui-"));
  browser = spawn(
    edgePath(),
    [
      "--headless=new",
      "--disable-gpu",
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-extensions",
      `--remote-debugging-port=${CDP_PORT}`,
      `--user-data-dir=${profileDir}`,
      "about:blank",
    ],
    { stdio: "ignore" },
  );
  const deadline = Date.now() + 30000;
  for (;;) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
      const page = list.find((t) => t.type === "page" && t.webSocketDebuggerUrl);
      if (page) return await Cdp.connect(page.webSocketDebuggerUrl);
    } catch {
      /* まだ上がっていない */
    }
    if (Date.now() > deadline) throw new Error("Edge の CDP が開かない");
    await sleep(300);
  }
}

function stopBrowser() {
  browser?.kill();
  if (profileDir) {
    try {
      fs.rmSync(profileDir, { recursive: true, force: true });
    } catch {
      /* 消せなくても実害は無い (temp 配下) */
    }
  }
}

/** ハーネスを読み込み直して、初期化が終わるまで待つ。 */
async function load(cdp, query = "") {
  await cdp.send("Page.navigate", { url: `${HARNESS}${query}` });
  const deadline = Date.now() + 40000;
  for (;;) {
    const ready = await cdp.eval("Boolean(window.__NOX_MOCK__ && window.__NOX_MOCK__.ready)").catch(() => false);
    if (ready) return;
    if (Date.now() > deadline) {
      const logs = await cdp.eval("document.body.innerHTML.slice(0, 400)").catch(() => "");
      throw new Error(`ハーネスが初期化されない: ${logs}`);
    }
    await sleep(150);
  }
}

async function setViewport(cdp, width, height) {
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width,
    height,
    deviceScaleFactor: 1,
    mobile: false,
  });
}

// --- 検査 --------------------------------------------------------------------

const SECTIONS = ["home", "history", "transcribe", "dictionary", "paste", "hotkeys", "sound", "app"];

/** 旧 UI (1 枚フォーム) が持っていた設定項目。保存パッチから落ちてはいけない。 */
const REQUIRED_PATCH_KEYS = [
  "language",
  "formatting_enabled",
  "injection_enabled",
  "history_enabled",
  "deep_context",
  "overlay_enabled",
  "start_hidden",
  "keep_transcript_in_clipboard",
  "local_stt_mode",
  "dictionary",
  "style_profiles",
  "sound_enabled",
  "sound_volume",
  "start_sound",
  "cancel_sound",
  "start_sound_path",
  "cancel_sound_path",
  "history_retention_days",
  "typing_speed_chars_per_min",
  "restore_delay_ms",
];

async function main() {
  console.log(await ensureVite());
  const cdp = await startBrowser();
  await setViewport(cdp, 1280, 900);
  await load(cdp);
  record("U0 ハーネスが初期化される (index.html の DOM + main.ts + Tauri モック)", true, HARNESS);

  // --- U1: `#hotkey-capture` はどの区画を見ていても掴めて、押せば効く
  //
  // ハーネスの前提そのもの。区画ごとに DOM を作り直す実装にすると、
  // ホットキー区画を開いていない間は要素が居らず、実機 E2E は
  // 「設定画面の page を見つけられない」で 30 秒待って落ちる。
  await check("U1 別区画 (通知音) を表示中でも #hotkey-capture を掴んで click できる", async () => {
    const r = await cdp.run(`
      document.querySelector('.nav-item[data-section="sound"]').click();
      const beforeVisible = document.getElementById("section-sound").hidden === false;
      const btn = document.getElementById("hotkey-capture");
      const foundWhileHidden = Boolean(btn) && document.getElementById("section-hotkeys").hidden === true;
      btn.click();
      await new Promise((r) => setTimeout(r, 150));
      return {
        beforeVisible,
        foundWhileHidden,
        capturedCmd: window.__NOX_MOCK__.count("start_hotkey_capture"),
        mode: window.__NOX_MOCK__.lastArgs("start_hotkey_capture"),
        hotkeysVisible: document.getElementById("section-hotkeys").hidden === false,
        ariaCurrent: [...document.querySelectorAll('.nav-item[aria-current="page"]')].map((b) => b.dataset.section),
      };
    `);
    return {
      ok:
        r.beforeVisible &&
        r.foundWhileHidden &&
        r.capturedCmd === 1 &&
        r.mode?.mode === "inject" &&
        r.hotkeysVisible &&
        r.ariaCurrent.length === 1 &&
        r.ariaCurrent[0] === "hotkeys",
      detail: JSON.stringify(r),
    };
  });

  // --- U2: 捕獲の表示遷移 (カウントダウン → 経過 → 確定)
  //
  // 2026-08-28 から捕獲は **DOM の keydown / keyup** で行う。ここは
  // 「イベントを流したら表示が変わるか」ではなく、**実際にキーを押して離す**
  // 形にしてある。押している最中の経過表示・全キーを離した瞬間の確定・
  // Rust へ渡る `code` の列まで、この 1 本で通しで見える
  // (実機では届かない Win キー等は原理的にここでも試せない)。
  await check("U2 捕獲: キーを押して離すと確定する (DOM の keydown/keyup)", async () => {
    const r = await cdp.run(`
      const label = document.getElementById("hotkey-label");
      const button = document.getElementById("hotkey-capture");
      const countdown = { text: label.textContent, capturing: label.dataset.capturing, button: button.textContent };
      const key = (type, code) =>
        window.dispatchEvent(new KeyboardEvent(type, { code, bubbles: true, cancelable: true }));

      // 押していく。1 打ごとに describe_hotkey_codes へ往復して表示名をもらう。
      key("keydown", "ControlLeft");
      await new Promise((r) => setTimeout(r, 60));
      const afterCtrl = label.textContent;
      key("keydown", "F14");
      await new Promise((r) => setTimeout(r, 60));
      const progress = { text: label.textContent, capturing: label.dataset.capturing };

      // 途中で 1 つ離しただけでは確定しない (既存仕様: すべて離した瞬間)。
      window.__NOX_MOCK__.config.hotkey_vk = 0x7d;
      key("keyup", "ControlLeft");
      await new Promise((r) => setTimeout(r, 60));
      const halfReleased = { text: label.textContent, finished: window.__NOX_MOCK__.count("finish_hotkey_capture") };

      key("keyup", "F14");
      await new Promise((r) => setTimeout(r, 250));
      return {
        countdown,
        afterCtrl,
        progress,
        halfReleased,
        sentCodes: window.__NOX_MOCK__.lastArgs("finish_hotkey_capture"),
        settled: { text: label.textContent, capturing: label.dataset.capturing, button: button.textContent },
        homeHint: document.getElementById("home-hotkey").textContent,
      };
    `);
    return {
      ok:
        /キーを押してください/.test(r.countdown.text) &&
        r.countdown.capturing === "true" &&
        r.countdown.button === "キャンセル" &&
        r.afterCtrl === "左 Ctrl (離すと確定)" &&
        r.progress.text === "左 Ctrl + F14 (離すと確定)" &&
        r.progress.capturing === "true" &&
        r.halfReleased.finished === 0 &&
        r.halfReleased.text === "左 Ctrl + F14 (離すと確定)" &&
        JSON.stringify(r.sentCodes?.codes) === JSON.stringify(["ControlLeft", "F14"]) &&
        r.settled.text === "左 Ctrl + F14" &&
        r.settled.capturing === undefined &&
        r.settled.button === "キーを押して設定" &&
        r.homeHint === "左 Ctrl + F14",
      detail: JSON.stringify(r),
    };
  });

  // --- U2c: 捕獲ボタンを押した「直後」のキーが素通りしない
  //
  // # なぜこのテストが要るのか (2026-08-28、実機で踏んだ)
  //
  // 捕獲の開始は invoke = IPC で、呼んでから応答が返るまでに必ず空白がある。
  // 「捕獲中」フラグと blur を `await` の**後**に置くと、その往復の間だけ
  // **キーを受け付けられない無防備な時間帯**ができる。そこで押された Space は
  // preventDefault されず、フォーカスを持ったままの捕獲ボタンを再発火させ、
  // 2 回目の toggle が「捕獲中なら畳む」に入って**捕獲が即座に取り消される**。
  // 実機では「space を押すとウィンドウのほうにフォーカスされる」と見えた。
  //
  // ここでは IPC に 300ms の遅延を注入し、**click の応答を待たずに**キーを
  // 押す。タイマーや待ち時間で誤魔化していないことを、この 1 本で固定する。
  await check("U2c 捕獲ボタンの click 直後 (IPC 完了前) に押したキーも取りこぼさない", async () => {
    const r = await cdp.run(`
      // 前の捕獲が残っていれば畳んでおく (区画を移ると畳まれる)。
      document.querySelector('.nav-item[data-section="home"]').click();
      await new Promise((r) => setTimeout(r, 200));
      // ホットキー区画を開いてから掴む。**非表示の要素はフォーカスできない**ので、
      // 隠れたままボタンを focus() しても実機の状況を再現できない
      // (呼び出し元のブラウザ実測は「ボタンにフォーカス → click」だった)。
      document.querySelector('.nav-item[data-section="hotkeys"]').click();
      await new Promise((r) => setTimeout(r, 200));
      window.__NOX_MOCK__.reset();
      window.__NOX_MOCK__.delays.start_hotkey_capture = 300;

      const button = document.getElementById("hotkey-capture");
      button.focus();
      const focusedBefore = document.activeElement.id;
      button.click();

      // **応答を待たない**。ここが要点。実機のユーザーも待たない。
      const down = new KeyboardEvent("keydown", { code: "Space", bubbles: true, cancelable: true });
      window.dispatchEvent(down);
      const up = new KeyboardEvent("keyup", { code: "Space", bubbles: true, cancelable: true });
      window.dispatchEvent(up);
      const duringIpc = {
        downPrevented: down.defaultPrevented,
        upPrevented: up.defaultPrevented,
        activeElement: document.activeElement === document.body ? "BODY" : document.activeElement.id,
      };

      // IPC が返りきるまで待ってから、捕獲が生きているか見る。
      await new Promise((r) => setTimeout(r, 700));
      const label = document.getElementById("hotkey-label");
      window.__NOX_MOCK__.delays.start_hotkey_capture = 0;
      return {
        focusedBefore,
        duringIpc,
        starts: window.__NOX_MOCK__.count("start_hotkey_capture"),
        cancels: window.__NOX_MOCK__.count("cancel_hotkey_capture"),
        capturing: label.dataset.capturing,
        button: button.textContent,
        activeAfter: document.activeElement === document.body ? "BODY" : document.activeElement.id,
      };
    `);
    return {
      ok:
        r.focusedBefore === "hotkey-capture" &&
        // IPC の最中でもキーは捕獲側が食う (ボタンの既定動作へ行かせない)。
        r.duringIpc.downPrevented === true &&
        r.duringIpc.upPrevented === true &&
        // 捕獲中はどの要素もフォーカスを持たない。
        r.duringIpc.activeElement === "BODY" &&
        r.activeAfter === "BODY" &&
        // ボタンが再発火していない = 開始 1 回・取り消し 0 回。
        r.starts === 1 &&
        r.cancels === 0 &&
        r.capturing === "true" &&
        r.button === "キャンセル",
      detail: JSON.stringify(r),
    };
  });

  // --- U2b: 使えない組み合わせは理由が出て、捕獲は続く (押し直せる)
  //
  // 黙って失敗しないこと。DOM 方式では「押したのに何も起きない」が
  // 一番ありがちな見え方になるので、断る側の経路を明示的に押さえる。
  await check("U2b 使えないキーは理由を出し、捕獲は続いて押し直せる", async () => {
    const r = await cdp.run(`
      // 直前のテストが捕獲を開いたままにしている。区画を移ると畳まれる。
      document.querySelector('.nav-item[data-section="home"]').click();
      await new Promise((r) => setTimeout(r, 200));
      document.getElementById("hotkey-capture").click();
      await new Promise((r) => setTimeout(r, 150));
      const label = document.getElementById("hotkey-label");
      const key = (type, code) =>
        window.dispatchEvent(new KeyboardEvent(type, { code, bubbles: true, cancelable: true }));

      key("keydown", "Enter");
      await new Promise((r) => setTimeout(r, 60));
      key("keyup", "Enter");
      await new Promise((r) => setTimeout(r, 250));
      const banner = document.getElementById("error");
      const rejected = {
        text: label.textContent,
        capturing: label.dataset.capturing,
        error: banner ? banner.textContent : "",
        errorHidden: banner ? banner.hidden : null,
      };

      // 2 度目の拒否でも終わらないこと。Space 単独は
      // is_allowed_hotkey が許さない (押しっぱなしで入力先へ流れる) ので、
      // ユーザーが素朴に踏みやすい拒否経路。
      key("keydown", "Space");
      await new Promise((r) => setTimeout(r, 60));
      key("keyup", "Space");
      await new Promise((r) => setTimeout(r, 250));
      const rejectedTwice = {
        text: label.textContent,
        capturing: label.dataset.capturing,
        error: banner ? banner.textContent : "",
      };

      // 捕獲は続いている。押し直せば普通に決まる。
      key("keydown", "ControlLeft");
      key("keydown", "F13");
      await new Promise((r) => setTimeout(r, 60));
      key("keyup", "F13");
      key("keyup", "ControlLeft");
      await new Promise((r) => setTimeout(r, 250));
      return { rejected, rejectedTwice, settled: label.textContent };
    `);
    return {
      ok:
        r.rejected.text === "キーを押してください…" &&
        r.rejected.capturing === "true" &&
        r.rejected.errorHidden === false &&
        /Enter/.test(r.rejected.error) &&
        // 2 度目の拒否でも捕獲は生きている。
        r.rejectedTwice.text === "キーを押してください…" &&
        r.rejectedTwice.capturing === "true" &&
        /Space/.test(r.rejectedTwice.error) &&
        r.settled === "左 Ctrl + F13",
      detail: JSON.stringify(r),
    };
  });

  // --- U2c: 修飾キー単独を割り当てると注意が出る (2026-08-29 の不具合の名残)
  //
  // 単独 Alt はフックが握り潰さないので入力先アプリにも流れ、Chrome は
  // それを「メニューを開け」と読む (docs/design.md の Q7)。Rust 側に
  // break_lone_alt という保険はあるが、選ばない方が確実なので UI で薦め直す。
  // 弾かない = 出るのは注意だけ、という形をここで固定する。
  await check("U2d 修飾キー単独のホットキーには注意が出て、普通のキーでは消える", async () => {
    const r = await cdp.run(`
      const block = document.getElementById("hotkey-lone-mod-warn");
      const text = document.getElementById("hotkey-lone-mod-warn-text");
      // 捕獲を通さず設定値だけ差し替え、renderConfig を通す
      // (capturingMode が無いときの nox://hotkey-captured は get_config へ落ちる)。
      const apply = async (vk, mods, label) => {
        window.__NOX_MOCK__.config.hotkey_vk = vk;
        window.__NOX_MOCK__.config.hotkey_mods = mods;
        window.__NOX_MOCK__.config.hotkey_label = label;
        window.__NOX_MOCK__.emit("nox://hotkey-captured", null);
        await new Promise((r) => setTimeout(r, 250));
        return { hidden: block.hidden, text: text.textContent };
      };
      const lone = await apply(0xa5, [], "右 Alt");
      const combo = await apply(0xa5, [0xa2], "左 Ctrl + 右 Alt");
      const plain = await apply(0x7c, [], "F13");
      return { lone, combo, plain };
    `);
    return {
      ok:
        r.lone.hidden === false &&
        /右 Alt/.test(r.lone.text) &&
        // 修飾キーが付けば「単独」ではないので出ない。
        r.combo.hidden === true &&
        r.plain.hidden === true,
      detail: JSON.stringify(r),
    };
  });

  // --- U3: 区画切替で aria-current が動き、可視区画はちょうど 1 つ
  await check("U3 全 8 区画で「可視はちょうど 1 つ」「aria-current もちょうど 1 つ」", async () => {
    const r = await cdp.run(`
      const out = [];
      for (const s of ${JSON.stringify(SECTIONS)}) {
        document.querySelector('.nav-item[data-section="' + s + '"]').click();
        await new Promise((r) => setTimeout(r, 30));
        const visible = [...document.querySelectorAll(".section")].filter((n) => !n.hidden).map((n) => n.dataset.section);
        const current = [...document.querySelectorAll('.nav-item[aria-current="page"]')].map((b) => b.dataset.section);
        const savebar = document.getElementById("savebar").hidden;
        out.push({ s, visible, current, savebarHidden: savebar });
      }
      return out;
    `);
    const bad = r.filter(
      (x) => x.visible.length !== 1 || x.visible[0] !== x.s || x.current.length !== 1 || x.current[0] !== x.s,
    );
    // 保存バーは入力欄のある区画だけ。ホットキーは押した瞬間に保存されるので出さない。
    const formless = ["home", "hotkeys"];
    const barWrong = r.filter((x) => x.savebarHidden !== formless.includes(x.s));
    return {
      ok: bad.length === 0 && barWrong.length === 0,
      detail:
        bad.length || barWrong.length
          ? `区画: ${JSON.stringify(bad)} / 保存バー: ${JSON.stringify(barWrong)}`
          : `8 区画すべて可視 1・aria-current 1、保存バーは ${formless.join("/")} で非表示`,
    };
  });

  // --- U4: 保存パッチに旧 UI の全項目が入る
  await check("U4 保存パッチに旧 UI の全フィールドが含まれる", async () => {
    const r = await cdp.run(`
      window.__NOX_MOCK__.lastPatch = null;
      document.getElementById("settings-form").requestSubmit();
      await new Promise((r) => setTimeout(r, 250));
      return { patch: window.__NOX_MOCK__.lastPatch, note: document.getElementById("settings-note").textContent };
    `);
    const keys = Object.keys(r.patch ?? {});
    const missing = REQUIRED_PATCH_KEYS.filter((k) => !keys.includes(k));
    return {
      ok: missing.length === 0 && r.note === "保存しました",
      detail:
        missing.length === 0
          ? `${keys.length} 項目 / note="${r.note}" / style_profiles=${r.patch.style_profiles.length} 件`
          : `欠落: ${missing.join(", ")}`,
    };
  });

  // --- U4b: 認識と整形で「画面コンテキスト」「画面質問モード」を入れて保存すると、
  //   実際の保存ボタンを押したあとチェックが画面に残る (往復する)。
  //
  // 過去のモックは set_config でパッチを内部 config に反映していなかったため、
  // `renderConfig` が元の false で上書きし、この不具合を検出できなかった。
  // (実機では Rust が反映するので別の壊れ方だが、E2E が盲点だった。)
  await check("U4b 認識と整形: deep_context / screen_ask_enabled のチェックが保存後も残る", async () => {
    const r = await cdp.run(`
      document.querySelector('.nav-item[data-section="transcribe"]').click();
      await new Promise((r) => setTimeout(r, 50));
      const dc = document.getElementById("deep-context");
      const sa = document.getElementById("screen-ask-enabled");
      dc.checked = true; sa.checked = true;
      document.querySelector('#savebar button[type="submit"]').click();
      await new Promise((r) => setTimeout(r, 300));
      return {
        deepAfterSave: document.getElementById("deep-context").checked,
        screenAskAfterSave: document.getElementById("screen-ask-enabled").checked,
        cfgDeep: window.__NOX_MOCK__.config.deep_context,
        cfgScreenAsk: window.__NOX_MOCK__.config.screen_ask_enabled,
        note: document.getElementById("settings-note").textContent,
      };
    `);
    return {
      ok:
        r.deepAfterSave === true &&
        r.screenAskAfterSave === true &&
        r.cfgDeep === true &&
        r.cfgScreenAsk === true &&
        r.note === "保存しました",
      detail: JSON.stringify(r),
    };
  });

  // --- U5: list_sound_presets が失敗したら、音関連はパッチから外れる
  //
  // 実装者が「ロジックは書いたが、実際に失敗させた往復は試せていない」と
  // 明示した箇所。select が空のまま value="" を送ると serde が SoundPreset を
  // 弾き、**set_config が丸ごと失敗して音と無関係な項目まで保存できなくなる**。
  await check("U5 list_sound_presets 失敗時、音関連 4 項目だけがパッチから外れる", async () => {
    await load(cdp, "?fail=list_sound_presets");
    const r = await cdp.run(`
      const err = document.getElementById("error");
      const before = { errorHidden: err.hidden, errorText: err.textContent,
        startOptions: document.getElementById("start-sound").options.length,
        cancelOptions: document.getElementById("cancel-sound").options.length };
      window.__NOX_MOCK__.lastPatch = null;
      document.getElementById("settings-form").requestSubmit();
      await new Promise((r) => setTimeout(r, 250));
      return { before, patch: window.__NOX_MOCK__.lastPatch, note: document.getElementById("settings-note").textContent };
    `);
    const keys = Object.keys(r.patch ?? {});
    const soundKeys = ["start_sound", "cancel_sound", "start_sound_path", "cancel_sound_path"];
    const leaked = soundKeys.filter((k) => keys.includes(k));
    const missingOthers = REQUIRED_PATCH_KEYS.filter(
      (k) => !soundKeys.includes(k) && !keys.includes(k),
    );
    return {
      ok:
        r.before.errorHidden === false &&
        /通知音の一覧を取得できません/.test(r.before.errorText) &&
        r.before.startOptions === 0 &&
        r.before.cancelOptions === 0 &&
        leaked.length === 0 &&
        missingOthers.length === 0 &&
        /音の一覧を取得できていない/.test(r.note),
      detail: `選択肢=${r.before.startOptions}/${r.before.cancelOptions} 個, 漏れた音項目=${JSON.stringify(leaked)}, 巻き添え=${JSON.stringify(missingOthers)}, note="${r.note}"`,
    };
  });

  // --- U6/U7/U8: 連動する有効・無効と出し分け
  await load(cdp);
  await check("U6 通知音 OFF で音関連コントロールが一括で無効になる (値は消えない)", async () => {
    const ids = [
      "sound-volume",
      "start-sound",
      "cancel-sound",
      "start-sound-path",
      "cancel-sound-path",
      "start-sound-preview",
      "cancel-sound-preview",
    ];
    const r = await cdp.run(`
      const ids = ${JSON.stringify(ids)};
      const box = document.getElementById("sound-enabled");
      const read = () => Object.fromEntries(ids.map((id) => [id, document.getElementById(id).disabled]));
      const valuesBefore = { start: document.getElementById("start-sound").value, volume: document.getElementById("sound-volume").value };
      box.checked = false; box.dispatchEvent(new Event("change"));
      const off = read();
      const valuesAfter = { start: document.getElementById("start-sound").value, volume: document.getElementById("sound-volume").value };
      box.checked = true; box.dispatchEvent(new Event("change"));
      return { off, on: read(), valuesBefore, valuesAfter };
    `);
    const allOff = ids.every((id) => r.off[id] === true);
    const allOn = ids.every((id) => r.on[id] === false);
    const kept = JSON.stringify(r.valuesBefore) === JSON.stringify(r.valuesAfter);
    return {
      ok: allOff && allOn && kept,
      detail: `OFF で無効=${allOff} / 戻すと有効=${allOn} / 値の保持=${kept} (${JSON.stringify(r.valuesAfter)})`,
    };
  });

  await check("U7 カスタム WAV を選んだときだけパス欄が出る", async () => {
    const r = await cdp.run(`
      const out = {};
      for (const kind of ["start", "cancel"]) {
        const select = document.getElementById(kind + "-sound");
        const row = document.getElementById(kind + "-sound-custom");
        select.value = "chime"; select.dispatchEvent(new Event("change"));
        const preset = row.hidden;
        select.value = "custom"; select.dispatchEvent(new Event("change"));
        const custom = row.hidden;
        select.value = "silent"; select.dispatchEvent(new Event("change"));
        out[kind] = { hiddenForPreset: preset, hiddenForCustom: custom, hiddenAfterBack: row.hidden };
      }
      return out;
    `);
    const ok = ["start", "cancel"].every(
      (k) => r[k].hiddenForPreset === true && r[k].hiddenForCustom === false && r[k].hiddenAfterBack === true,
    );
    return { ok, detail: JSON.stringify(r) };
  });

  await check("U8 復元ディレイは「クリップボードに残す」ON の間だけ無効 + 理由を出す", async () => {
    const r = await cdp.run(`
      const keep = document.getElementById("keep-transcript");
      const delay = document.getElementById("restore-delay");
      const note = document.getElementById("restore-delay-note");
      const read = () => ({ disabled: delay.disabled, noteHidden: note.hidden, note: note.textContent, value: delay.value });
      keep.checked = true; keep.dispatchEvent(new Event("change"));
      const on = read();
      keep.checked = false; keep.dispatchEvent(new Event("change"));
      const off = read();
      return { on, off, describedBy: delay.getAttribute("aria-describedby") };
    `);
    return {
      ok:
        r.on.disabled === true &&
        r.on.noteHidden === false &&
        r.on.note.length > 0 &&
        r.off.disabled === false &&
        r.off.noteHidden === true &&
        r.on.value === r.off.value &&
        r.describedBy === "restore-delay-note",
      detail: JSON.stringify(r),
    };
  });

  // --- U9: 履歴の検索デバウンスとページング
  await check("U9 検索は 200ms デバウンスされ、1 回だけ問い合わせる", async () => {
    const r = await cdp.run(`
      document.querySelector('.nav-item[data-section="history"]').click();
      window.__NOX_MOCK__.reset();
      const input = document.getElementById("history-search");
      for (const s of ["あ", "あい", "あいう", "あいうえ"]) {
        input.value = s;
        input.dispatchEvent(new Event("input"));
        await new Promise((r) => setTimeout(r, 30));
      }
      const during = window.__NOX_MOCK__.count("get_history");
      await new Promise((r) => setTimeout(r, 400));
      return { during, after: window.__NOX_MOCK__.count("get_history"), args: window.__NOX_MOCK__.lastArgs("get_history") };
    `);
    return {
      ok: r.during === 0 && r.after === 1 && r.args?.query === "あいうえ" && r.args?.beforeId === null,
      detail: `打鍵中=${r.during} 回 / 落ち着いた後=${r.after} 回 / ${JSON.stringify(r.args)}`,
    };
  });

  await check("U9b 「もっと読む」は最後の行の id から続きを取り、追記する", async () => {
    const r = await cdp.run(`
      const input = document.getElementById("history-search");
      input.value = ""; input.dispatchEvent(new Event("input"));
      await new Promise((r) => setTimeout(r, 400));
      const first = document.querySelectorAll("#history-list .history-item").length;
      const moreHidden = document.getElementById("history-more").hidden;
      window.__NOX_MOCK__.reset();
      document.getElementById("history-more").click();
      await new Promise((r) => setTimeout(r, 250));
      const args = window.__NOX_MOCK__.lastArgs("get_history");
      const second = document.querySelectorAll("#history-list .history-item").length;
      // 50 件未満が返ったら「もっと読む」は消える (最後まで来た合図)。
      window.__NOX_MOCK__.historyReturn = 3;
      document.getElementById("history-more").click();
      await new Promise((r) => setTimeout(r, 250));
      const third = document.querySelectorAll("#history-list .history-item").length;
      const endHidden = document.getElementById("history-more").hidden;
      window.__NOX_MOCK__.historyReturn = 50;
      return { first, moreHidden, args, second, third, endHidden };
    `);
    return {
      ok:
        r.first === 50 &&
        r.moreHidden === false &&
        r.args?.beforeId === 951 &&
        r.second === 100 &&
        r.third === 103 &&
        r.endHidden === true,
      detail: `1 ページ目=${r.first} / beforeId=${r.args?.beforeId} / 2 ページ目=${r.second} / 端まで=${r.third} 件で「もっと読む」非表示=${r.endHidden}`,
    };
  });

  // --- U10: エラー帯は区画を切り替えても残る
  //
  // 区画の中に置くと、別の区画へ移った瞬間に失敗の報せが消える。
  await check("U10 エラー帯は区画を切り替えても残る", async () => {
    const r = await cdp.run(`
      window.__NOX_MOCK__.emit("nox://error", { message: "テスト用の失敗", origin: "recording" });
      await new Promise((r) => setTimeout(r, 50));
      const shown = { hidden: document.getElementById("error").hidden, text: document.getElementById("error").textContent };
      const after = [];
      for (const s of ["app", "home", "hotkeys"]) {
        document.querySelector('.nav-item[data-section="' + s + '"]').click();
        await new Promise((r) => setTimeout(r, 30));
        after.push({ s, hidden: document.getElementById("error").hidden, text: document.getElementById("error").textContent });
      }
      // 録音が始まったら消える (次の録音まで古い失敗を残さない)。
      window.__NOX_MOCK__.emit("nox://status", { status: "recording", message: null, origin: "recording" });
      await new Promise((r) => setTimeout(r, 50));
      return { shown, after, cleared: document.getElementById("error").hidden };
    `);
    return {
      ok:
        r.shown.hidden === false &&
        r.shown.text === "テスト用の失敗" &&
        r.after.every((a) => a.hidden === false && a.text === "テスト用の失敗") &&
        r.cleared === true,
      detail: JSON.stringify(r),
    };
  });


  // --- U12: アプリ別の文体が行単位で編集でき、由来が見える
  //
  // 旧 UI はテキストエリア 1 枚で、形式が違う行は黙って捨てられていた。
  // 既定が 30 件近くになる以上、行として並び、由来 (既定 / 編集済み /
  // 自作) が見えないと、どれを触ってよいのか分からない。
  await load(cdp);
  await check("U12 文体は行単位で並び、既定 / 編集済み / 自作の由来が出る", async () => {
    const r = await cdp.run(`
      document.querySelector('.nav-item[data-section="dictionary"]').click();
      await new Promise((r) => setTimeout(r, 40));
      const rows = [...document.querySelectorAll("#style-list .style-row")];
      return {
        count: rows.length,
        badges: rows.map((row) => row.querySelector(".badge").dataset.kind),
        labels: rows.map((row) => row.querySelector(".badge").textContent),
        values: rows.map((row) => ({
          process: row.querySelector(".style-process").value,
          title: row.querySelector(".style-title").value,
          instruction: row.querySelector(".style-instruction").value,
        })),
        emptyHidden: document.getElementById("style-empty").hidden,
        // 一覧そのものは区画に残り続ける (DOM から消さない)。
        listPresent: Boolean(document.getElementById("style-list")),
      };
    `);
    return {
      ok:
        r.count === 3 &&
        JSON.stringify(r.badges) === JSON.stringify(["bundled", "edited", "mine"]) &&
        r.values[1].title === "Gmail" &&
        r.values[2].process === "myapp.exe" &&
        r.emptyHidden === true &&
        r.listPresent,
      detail: JSON.stringify(r),
    };
  });

  // --- U12b: 追加・削除・編集が保存パッチへそのまま乗る
  //
  // ここが落ちると、画面には出ているのに保存されない行が生まれる。
  await check("U12b 行の追加・編集・削除が保存パッチに反映される (id と印も往復)", async () => {
    const r = await cdp.run(`
      // 1 件消して、1 件足して、既定の指示を書き換える。
      const rows = () => [...document.querySelectorAll("#style-list .style-row")];
      rows()[2].querySelector(".style-remove").click();
      document.getElementById("style-add").click();
      await new Promise((r) => setTimeout(r, 30));
      const added = rows()[rows().length - 1];
      added.querySelector(".style-process").value = "figma.exe";
      added.querySelector(".style-instruction").value = "デザインへのコメント";
      rows()[0].querySelector(".style-instruction").value = "書き換えた指示";

      window.__NOX_MOCK__.lastPatch = null;
      document.getElementById("settings-form").requestSubmit();
      await new Promise((r) => setTimeout(r, 250));
      return {
        patch: window.__NOX_MOCK__.lastPatch?.style_profiles,
        note: document.getElementById("settings-note").textContent,
      };
    `);
    const sent = r.patch ?? [];
    const slack = sent.find((p) => p.id === "chat.slack");
    const gmail = sent.find((p) => p.id === "web.gmail");
    const figma = sent.find((p) => p.process === "figma.exe");
    return {
      ok:
        sent.length === 3 &&
        // 消した自作の行は送られない。
        !sent.some((p) => p.process === "myapp.exe") &&
        slack?.instruction === "書き換えた指示" &&
        // 既定の id は往復する (これが無いと Rust 側が編集を追えない)。
        slack?.id === "chat.slack" &&
        gmail?.user_edited === true &&
        gmail?.title_contains === "Gmail" &&
        // 追加分は id 無し = ユーザー作成。
        figma?.id === "" &&
        figma?.title_contains === null &&
        r.note === "保存しました",
      detail: `${JSON.stringify(sent)} / note="${r.note}"`,
    };
  });

  // --- U12c: 書きかけの行は保存されず、黙って消えない
  await check("U12c 入力が足りない行は送らず、その旨を伝える", async () => {
    await load(cdp);
    const r = await cdp.run(`
      document.getElementById("style-add").click();
      await new Promise((r) => setTimeout(r, 30));
      window.__NOX_MOCK__.lastPatch = null;
      document.getElementById("settings-form").requestSubmit();
      await new Promise((r) => setTimeout(r, 250));
      return {
        sent: window.__NOX_MOCK__.lastPatch?.style_profiles.length,
        note: document.getElementById("settings-note").textContent,
      };
    `);
    return {
      ok: r.sent === 3 && /保存していません/.test(r.note),
      detail: `送った件数=${r.sent} / note="${r.note}"`,
    };
  });

  // --- U13: 履歴からの提案とワンクリック作成
  await check("U13 提案から行を作ると、プロセス名入りの行が増えて候補が消える", async () => {
    await load(cdp);
    const r = await cdp.run(`
      document.querySelector('.nav-item[data-section="dictionary"]').click();
      await new Promise((r) => setTimeout(r, 60));
      const before = {
        note: document.getElementById("style-suggest-note").textContent,
        items: [...document.querySelectorAll("#style-suggest-list .suggest-item")].length,
        rows: document.querySelectorAll("#style-list .style-row").length,
      };
      document.querySelector("#style-suggest-list .suggest-add").click();
      await new Promise((r) => setTimeout(r, 40));
      const rows = [...document.querySelectorAll("#style-list .style-row")];
      const last = rows[rows.length - 1];
      return {
        before,
        items: [...document.querySelectorAll("#style-suggest-list .suggest-item")].length,
        rows: rows.length,
        process: last.querySelector(".style-process").value,
        instruction: last.querySelector(".style-instruction").value,
        badge: last.querySelector(".badge").dataset.kind,
      };
    `);
    return {
      ok:
        r.before.items === 2 &&
        /87/.test(r.before.note) &&
        r.items === 1 &&
        r.rows === r.before.rows + 1 &&
        r.process === "figma.exe" &&
        r.instruction.length > 0 &&
        r.badge === "mine",
      detail: JSON.stringify(r),
    };
  });

  // --- U13b: 0 件・履歴オフ・取得失敗を言い分ける
  //
  // 全部「提案なし」に丸めると、機能が壊れているようにしか見えない。
  await check("U13b 提案は 0 件・履歴オフ・履歴が空・失敗を言い分ける", async () => {
    const cases = [
      ["履歴オフ", `{ history_enabled: false, total_sessions: 0, items: [] }`, /履歴が無効/],
      ["履歴が空", `{ history_enabled: true, total_sessions: 0, items: [] }`, /まだ履歴がありません/],
      ["全部設定済み", `{ history_enabled: true, total_sessions: 87, items: [] }`, /すべて設定済み/],
    ];
    const seen = [];
    for (const [name, payload] of cases) {
      // 保存すると覆われる範囲が変わるので、提案は取り直される。
      // その本番の経路をそのまま使う (テスト専用の入口を作らない)。
      const text = await cdp.run(`
        window.__NOX_MOCK__.styleSuggestions = ${payload};
        document.getElementById("settings-form").requestSubmit();
        await new Promise((r) => setTimeout(r, 300));
        return document.getElementById("style-suggest-note").textContent;
      `);
      seen.push({ name, text });
    }
    // 取得そのものが失敗したときは、0 件と混ぜず失敗として出す。
    await load(cdp, "?fail=get_style_suggestions");
    const failed = await cdp.eval(`document.getElementById("style-suggest-note").textContent`);
    seen.push({ name: "取得失敗", text: failed });
    const ok =
      cases.every(([name, , re]) => re.test(seen.find((s) => s.name === name).text)) &&
      /取得できません/.test(failed) &&
      new Set(seen.map((s) => s.text)).size === 4;
    return { ok, detail: JSON.stringify(seen) };
  });

  // --- U11: 狭い窓でも横あふれが無く、レール下端の状態カードが見える
  for (const [w, h] of [
    [420, 420],
    [940, 660],
  ]) {
    await check(`U11 ${w}×${h} で全区画に横あふれが無く、状態カードが見える`, async () => {
      await setViewport(cdp, w, h);
      await load(cdp);
      const r = await cdp.run(`
        const out = [];
        for (const s of ${JSON.stringify(SECTIONS)}) {
          document.querySelector('.nav-item[data-section="' + s + '"]').click();
          await new Promise((r) => setTimeout(r, 40));
          const doc = document.documentElement;
          const content = document.getElementById("content");
          const card = document.getElementById("status-card");
          const rect = card.getBoundingClientRect();
          out.push({
            s,
            docOverflow: doc.scrollWidth - doc.clientWidth,
            contentOverflow: content.scrollWidth - content.clientWidth,
            cardBottom: Math.round(rect.bottom),
            cardTop: Math.round(rect.top),
            cardWidth: Math.round(rect.width),
            viewportH: window.innerHeight,
          });
        }
        return out;
      `);
      const overflow = r.filter((x) => x.docOverflow > 1 || x.contentOverflow > 1);
      const cardHidden = r.filter(
        (x) => x.cardWidth <= 0 || x.cardTop < 0 || x.cardBottom > x.viewportH + 1,
      );
      return {
        ok: overflow.length === 0 && cardHidden.length === 0,
        detail:
          overflow.length || cardHidden.length
            ? `横あふれ: ${JSON.stringify(overflow)} / 状態カード: ${JSON.stringify(cardHidden)}`
            : `8 区画すべて横あふれ 0px、状態カードは ${r[0].cardTop}〜${r[0].cardBottom}px (画面高 ${r[0].viewportH}px) に収まる`,
      };
    });
  }

  const failed = results.filter((r) => r.verdict === "FAIL");
  console.log(`\n=== PASS ${results.length - failed.length} / FAIL ${failed.length} (全 ${results.length}) ===`);
  console.log("疑似テストなので、緑でも実機 E2E (e2e/hotkey.mjs) の代わりにはならない。");
  return failed.length === 0 ? 0 : 1;
}

main()
  .then((code) => {
    stopBrowser();
    process.exit(code);
  })
  .catch((e) => {
    console.error(`疑似 E2E が異常終了: ${e.stack || e}`);
    stopBrowser();
    process.exit(2);
  });

