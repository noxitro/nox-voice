// 実機 E2E: ホットキーの「一覧から選ぶ」を**本物の Rust 相手に**往復させる。
//
// 疑似 E2E (e2e/sim-ui.mjs) はモックなので、コマンド名・引数名の綴りがずれて
// いても緑になる (実際に list_hotkey_keys 等は sim-ui では一度も Rust に届かない)。
// このハーネスは release ビルドの nox-voice.exe を起動し、WebView2 の CDP から
// 実際の DOM を操作して、結果を**アプリのログ** (Rust 側の事実) で確かめる。
// マウスには一切触れない (CDP はページへ直接注入する)。
//
// 見ているもの: P0/P1 実変更がログに出る / P2 重複は Dropped で一覧が開いたまま
// 理由が出る / P3 別用途への設定 / P4 ページに例外が無い。
//
// 副作用 (実行前に承知すること):
//   - nox-voice.exe を強制終了し、自分で起動し、終了時にまた止める
//   - **本物の config.json を書き換える** (P2 が意図的に重複を作る)。終了時に
//     「貼り付け」「クリップボードのみ」の 2 用途だけを診断前の値へ戻す。
//     スナップショット丸ごとの書き戻しはしない — 一度それで別の変更を消した
//   - 作業中のマシンでは走らせない。走らせたあとは自分でアプリを起動し直すこと
//   - 設定ファイルがまだ無いマシン (CI のランナーなど) では、診断で作られた
//     設定ファイルを終了時に消して「無い」状態へ戻す
//
// 測る exe は `NOX_E2E_EXE` で差し替えられる (既定は release ビルド)。
//
// 実行: npm run e2e:picker-live
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

import { describeCdpFailure } from "./cdp-diagnose.mjs";

const REPO = path.resolve(import.meta.dirname, "..");
const EXE = process.env.NOX_E2E_EXE || path.join(REPO, "src-tauri", "target", "release", "nox-voice.exe");
const LOG = `${process.env.LOCALAPPDATA}\\com.noxitro.nox-voice\\logs\\nox-voice.log`;
const PORT = 9333;
const F13 = 124, F14 = 125;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const results = [];
const record = (name, ok, detail) => { results.push({ name, ok }); console.log(`${ok ? "PASS" : "FAIL"}  ${name}\n        ${detail}`); };

function killApp() {
  spawnSync("taskkill.exe", ["/F", "/IM", "nox-voice.exe"], { encoding: "utf8" });
  spawnSync("powershell.exe", ["-NoProfile", "-Command",
    "Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | " +
    "Where-Object { $_.CommandLine -like '*com.noxitro.nox-voice*' } | " +
    "ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }"], { encoding: "utf8" });
}
const logSize = () => (fs.existsSync(LOG) ? fs.statSync(LOG).size : 0);
// `pos` はバイト数。文字列にしてから slice すると日本語 (1 文字 3 バイト) で
// 位置が先へずれ、直近の行を読み飛ばす。バイト列のまま切ってから復号する。
const logSince = (pos) => (fs.existsSync(LOG) ? fs.readFileSync(LOG).subarray(pos).toString("utf8") : "");

class Cdp {
  constructor(ws) { this.ws = ws; this.id = 0; this.pending = new Map(); this.events = [];
    ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(typeof ev.data === "string" ? ev.data : ev.data.toString());
      if (msg.id && this.pending.has(msg.id)) { this.pending.get(msg.id)(msg); this.pending.delete(msg.id); }
      else if (msg.method) this.events.push(msg);
    });
  }
  send(method, params = {}) {
    const id = ++this.id;
    return new Promise((resolve, reject) => {
      this.pending.set(id, (m) => (m.error ? reject(new Error(m.error.message)) : resolve(m.result)));
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => reject(new Error(`${method} timeout`)), 15000);
    });
  }
  async eval(expr) {
    const r = await this.send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true, userGesture: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? "例外");
    return r.result?.value;
  }
}

async function connect() {
  const deadline = Date.now() + 30000;
  let lastError = null;
  for (;;) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      for (const t of list.filter((x) => x.type === "page" && x.webSocketDebuggerUrl)) {
        const ws = new WebSocket(t.webSocketDebuggerUrl);
        await new Promise((res, rej) => { ws.addEventListener("open", res, { once: true }); ws.addEventListener("error", rej, { once: true }); });
        const cdp = new Cdp(ws);
        await cdp.send("Runtime.enable");
        if (await cdp.eval(`Boolean(document.getElementById("settings-form"))`)) return cdp;
        ws.close();
      }
    } catch (e) { lastError = e; }
    if (Date.now() > deadline) throw new Error(`設定画面へ接続できない\n${await describeCdpFailure(PORT, lastError)}`);
    await sleep(400);
  }
}

/** 一覧を開き、F キーのカテゴリを探して vk を押し、確定する。戻り値は画面の観測。 */
async function pickViaList(cdp, prefix, vk) {
  return cdp.eval(`(async () => {
    const pickBtn = document.getElementById(${JSON.stringify(prefix + "-pick")});
    const picker = document.getElementById(${JSON.stringify(prefix + "-picker")});
    if (!pickBtn || !picker) return { error: "一覧の器が無い" };
    pickBtn.click();
    await new Promise((r) => setTimeout(r, 400));
    if (picker.hidden) return { error: "一覧が開かない" };
    // カテゴリを順に開き、目当ての vk のボタンが現れるところで止める。
    let clicksToKey = 1; // 「一覧から選ぶ」ぶん
    let keyBtn = picker.querySelector('.picker-key[data-vk="' + ${vk} + '"]');
    if (!keyBtn) {
      for (const cat of picker.querySelectorAll(".picker-cat")) {
        cat.click(); clicksToKey++;
        await new Promise((r) => setTimeout(r, 120));
        keyBtn = picker.querySelector('.picker-key[data-vk="' + ${vk} + '"]');
        if (keyBtn) break;
      }
    }
    if (!keyBtn) return { error: "vk " + ${vk} + " のボタンが一覧に無い" };
    keyBtn.click(); clicksToKey++;
    await new Promise((r) => setTimeout(r, 300));
    const noteBefore = picker.querySelector(".picker-note")?.textContent ?? "";
    const apply = picker.querySelector(".picker-apply");
    const applyDisabled = apply?.disabled ?? true;
    apply?.click();
    await new Promise((r) => setTimeout(r, 900));
    return {
      clicksToKey, noteBefore, applyDisabled,
      pickerOpenAfter: !picker.hidden,
      noteAfter: picker.querySelector(".picker-note")?.textContent ?? "",
      label: document.getElementById(${JSON.stringify(prefix + "-label")})?.textContent ?? "",
      errorBand: (() => { const e = document.getElementById("error"); return e && !e.hidden ? e.textContent.slice(0, 120) : ""; })(),
    };
  })()`);
}

if (!fs.existsSync(EXE)) {
  console.error(`exe が無い: ${EXE}\nnpm run release を先に通すこと。`);
  process.exit(2);
}
killApp();
await sleep(1200);
// 設定ファイルの現在値を控えておき、最後に戻す (この診断は本当に設定を書き換える)。
// 無いときは null。終了時に、診断で作られた設定ファイルを消す。
const cfgPath = `${process.env.APPDATA}\\com.noxitro.nox-voice\\config.json`;
const cfgBackup = fs.existsSync(cfgPath) ? fs.readFileSync(cfgPath, "utf8") : null;

const app = spawn(EXE, [], {
  env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}`, NOX_VOICE_LOG: "debug" },
  detached: true, stdio: "ignore",
});
app.unref();

try {
  const cdp = await connect();
  await cdp.eval(`window.__TAURI_INTERNALS__.invoke("show_window", {})`).catch(() => {});
  await sleep(1200);
  await cdp.eval(`document.querySelector('.nav-item[data-section="hotkeys"]')?.click()`);
  await sleep(300);

  // --- P0: 貼り付け用をいったん F15 へ (開発機の設定は F13 のことが多く、
  //   F13→F13 では Rust が「変更なし」と正しく判断してログが出ない。
  //   どの初期値からでも実際に変わる操作にする)
  const F15 = 126;
  let pos = logSize();
  let r = await pickViaList(cdp, "hotkey", F15);
  let log = logSince(pos);
  record("P0 貼り付け用を一覧から F15 に変更 (実往復)", !r.error && r.label === "F15" && /ホットキーを変更 \[貼り付け\]: F15/.test(log),
    r.error ?? `ラベル="${r.label}" / ログ=${/\[貼り付け\]: F15/.test(log) ? "変更あり" : "変更なし"}`);

  // --- P1: 貼り付け用を一覧から F13 に戻す (list_hotkey_keys / describe / set の実往復)
  pos = logSize();
  r = await pickViaList(cdp, "hotkey", F13);
  log = logSince(pos);
  const p1ok = !r.error && !r.pickerOpenAfter && r.label === "F13" && /ホットキーを変更 \[貼り付け\]: F13/.test(log);
  record("P1 貼り付け用を一覧から F13 に設定 (Rust まで届く)", p1ok,
    r.error ?? `到達クリック=${r.clicksToKey} / ラベル="${r.label}" / 一覧閉じた=${!r.pickerOpenAfter} / ログ=${/\[貼り付け\]: F13/.test(log) ? "変更あり" : "変更なし"}`);

  // --- P2: クリップボード用にわざと F13 → 重複で無効化され、一覧は開いたまま理由が出る
  pos = logSize();
  r = await pickViaList(cdp, "clipboard-hotkey", F13);
  log = logSince(pos);
  const p2ok = !r.error && r.pickerOpenAfter && (r.noteAfter.includes("同じ") || r.errorBand.includes("同じ"))
    && !/ホットキーを変更 \[クリップボードのみ\]: F13/.test(log);
  record("P2 重複 (F13) は無効化され、一覧は開いたまま理由が出る", p2ok,
    r.error ?? `一覧開いたまま=${r.pickerOpenAfter} / 注記="${r.noteAfter.slice(0, 60)}" / 帯="${r.errorBand.slice(0, 60)}"`);
  await cdp.eval(`document.querySelector('#clipboard-hotkey-picker .picker-close')?.click()`).catch(() => {});
  await sleep(200);

  // --- P3: クリップボード用を F14 に
  pos = logSize();
  r = await pickViaList(cdp, "clipboard-hotkey", F14);
  log = logSince(pos);
  const p3ok = !r.error && !r.pickerOpenAfter && r.label === "F14" && /ホットキーを変更 \[クリップボードのみ\]: F14/.test(log);
  record("P3 クリップボード用を一覧から F14 に設定", p3ok,
    r.error ?? `ラベル="${r.label}" / ログ=${/\[クリップボードのみ\]: F14/.test(log) ? "変更あり" : "変更なし"}`);

  // --- P4: ページに例外が出ていない
  const errs = cdp.events.filter((e) => e.method === "Runtime.exceptionThrown");
  record("P4 往復のあいだページに例外が無い", errs.length === 0,
    errs.length === 0 ? "例外なし" : errs.map((e) => e.params.exceptionDetails?.exception?.description?.slice(0, 120)).join(" | "));
} finally {
  // 診断は本物の設定を書き換える (P2 が意図的にクリップボード用を無効化する)。
  // **スナップショットの書き戻しはしない** — 前回それで F14 が消えた。
  // 診断前の値を丸ごと戻すのではなく、この診断が触った 2 用途だけを
  // 既知の正しい値へ明示的に戻す。アプリは止めてあるので JSON 直書きで安全。
  killApp();
  await sleep(600);
  if (cfgBackup === null) {
    fs.rmSync(cfgPath, { force: true });
    console.log("診断前は設定ファイルが無かったので、診断で作られたものを消しました");
  } else {
    const cfg = JSON.parse(fs.readFileSync(cfgPath, "utf8"));
    const before = JSON.parse(cfgBackup);
    cfg.hotkey_vk = before.hotkey_vk; cfg.hotkey_mods = before.hotkey_mods;
    cfg.clipboard_hotkey_vk = before.clipboard_hotkey_vk; cfg.clipboard_hotkey_mods = before.clipboard_hotkey_mods;
    fs.writeFileSync(cfgPath, JSON.stringify(cfg, null, 2));
    console.log(`ホットキーを診断前の値へ戻しました (貼り付け vk=${cfg.hotkey_vk} / クリップボード vk=${cfg.clipboard_hotkey_vk})`);
  }
}

const failed = results.filter((x) => !x.ok).length;
console.log(`\n=== PASS ${results.length - failed} / FAIL ${failed} (全 ${results.length}) ===`);
process.exit(failed ? 1 : 0);
