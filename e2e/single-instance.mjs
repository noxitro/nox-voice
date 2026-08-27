// 多重起動防止の E2E。実 exe を 2 プロセス起動して「本当に 1 個しか残らないか」を見る。
//
// 単体テストで測れるのは判定ロジックだけで、
// 「CreateMutexW を run() の 1 行目に置いたので起動競合が消えた」は
// **実プロセスでしか確かめられない**。ここはその穴を埋めるためにある。
//
// 前提と副作用:
// - `src-tauri/target/debug/nox-voice.exe` が要る (cargo build)。
// - **開始時に既存の nox-voice.exe を全部落とす**。利用者がアプリを使っている
//   間は走らせないこと (ホットキー E2E と同じ流儀)。
// - vite は不要。ここではウィンドウの中身を一切触らず、プロセス数とログだけを見る。
// - 設定ファイルには触らない (書き換えない)。
//
// 使い方: node e2e/single-instance.mjs
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const REPO = path.resolve(import.meta.dirname, "..");
const EXE = path.join(REPO, "src-tauri", "target", "debug", "nox-voice.exe");
const LOG = path.join(process.env.LOCALAPPDATA, "com.noxitro.nox-voice", "logs", "nox-voice.log");

/** 1 個目が起動しきる (= フック設置ログが出る) のを待つ上限。 */
const BOOT_TIMEOUT = 30000;
/** 2 個目が自分で終了するのを待つ時間。
 *
 * ミューテックス判定自体は run() の 1 行目なので一瞬で終わるが、その後の
 * 「既存インスタンスへの通知」が最悪 `NOTIFY_TIMEOUT_MS` (3 秒) かかる
 * (1 個目のメインスレッドが詰まっている場合。疑似 E2E で 3002 ms を実測)。
 * ここをそれより短くすると、**まだ終了処理中の 2 個目を数えて
 * 「二重起動している」と誤判定する**。実測値に対して余裕を持たせる。
 * instance.rs 側の `NOTIFY_TIMEOUT_MS` を変えたらここも見直すこと。 */
const SETTLE = 6000;
/** 同時起動を試す回数。競合は確率的なので 1 回では意味がない。 */
const RACE_ROUNDS = Number(process.env.NOX_E2E_RACE_ROUNDS ?? 5);

const results = [];
let logOffset = 0;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function record(name, ok, detail) {
  const verdict = ok ? "PASS" : "FAIL";
  results.push({ name, verdict, detail });
  console.log(`${verdict}  ${name}${detail ? `\n        ${detail}` : ""}`);
  if (!ok) {
    const tail = readNewLog().split(/\r?\n/).filter(Boolean).slice(-15);
    console.log(tail.map((l) => `        | ${l}`).join("\n"));
  }
}

function markLog() {
  logOffset = fs.existsSync(LOG) ? fs.statSync(LOG).size : 0;
}

function readNewLog() {
  if (!fs.existsSync(LOG)) return "";
  const buf = fs.readFileSync(LOG);
  return buf.subarray(Math.min(logOffset, buf.length)).toString("utf8");
}

async function waitForLog(needle, timeout) {
  const deadline = Date.now() + timeout;
  for (;;) {
    const line = readNewLog()
      .split(/\r?\n/)
      .find((l) => l.includes(needle));
    if (line) return line;
    if (Date.now() > deadline) return null;
    await sleep(200);
  }
}

/** 生きている nox-voice.exe の数。 */
function aliveCount() {
  const out = spawnSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-Command",
      "(Get-Process -Name nox-voice -ErrorAction SilentlyContinue | Measure-Object).Count",
    ],
    { encoding: "utf8" },
  ).stdout.trim();
  return Number(out);
}

function killAll() {
  spawnSync("taskkill.exe", ["/F", "/IM", "nox-voice.exe"], { encoding: "utf8" });
  // WebView2 の生き残りは次回起動が相乗りするので巻き取る (自アプリのものだけ)。
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

function launch() {
  return spawn(EXE, [], { env: { ...process.env, RUST_LOG: "info" }, stdio: "ignore" });
}

/** 終了コードを待つ (終了しなければ null)。 */
function waitExit(proc, timeout) {
  return new Promise((resolve) => {
    if (proc.exitCode !== null) return resolve(proc.exitCode);
    const timer = setTimeout(() => resolve(null), timeout);
    proc.once("exit", (code) => {
      clearTimeout(timer);
      resolve(code);
    });
  });
}

async function main() {
  if (!fs.existsSync(EXE)) {
    console.error(`exe が無い: ${EXE}\ncargo build を先に通すこと。`);
    process.exit(2);
  }

  // 利用者のアプリが動いていると「既に 1 個いる」状態から始まってしまい、
  // どのテストも意味を失う。必ず 0 個から始める。
  killAll();
  await sleep(1000);
  if (aliveCount() !== 0) {
    console.error("既存の nox-voice.exe を落とせなかった。手動で終了してから再実行すること。");
    process.exit(2);
  }

  try {
    // --- S1: 1 個目は普通に起動する (これが通らないと以降は測れない)
    markLog();
    const first = launch();
    const booted = await waitForLog("キーボードフックを設置", BOOT_TIMEOUT);
    record("S1 1 個目は普通に起動する", Boolean(booted), booted ?? "フック設置のログが出ない");
    if (!booted) throw new Error("1 個目が起動しない。以降は測定不能。");

    // --- S2: 起動済みのところへ 2 個目 → 2 個目だけが終了する
    markLog();
    const second = launch();
    const secondExit = await waitExit(second, SETTLE);
    await sleep(500);
    const aliveAfter = aliveCount();
    record(
      "S2 起動済みのところへ 2 個目を起動しても 1 プロセスだけ残る",
      aliveAfter === 1 && secondExit === 0,
      `プロセス数=${aliveAfter} / 2 個目の exitCode=${secondExit}`,
    );

    // --- S3: 2 個目は「なぜ終了したか」をログに残す
    //
    // 無言で消えると「たまに起動しない謎のアプリ」にしか見えない。
    // 利用者が後から追えることまで含めて仕様。
    const bailout = readNewLog()
      .split(/\r?\n/)
      .find((l) => l.includes("nox-voice は既に起動しています"));
    record(
      "S3 終了した 2 個目がログに理由を残す",
      Boolean(bailout),
      bailout ?? "「既に起動しています」の行が無い",
    );

    // --- S4: 既存インスタンスへ通知が届き、ウィンドウを出す導線が生きている
    //
    // ミューテックスで即 exit すると、プラグインが担っていた
    // 「既存のウィンドウを前に出す」が失われかねない。2 個目が終了前に
    // WM_COPYDATA を送っているので、1 個目のコールバックのログが出るはず。
    // 待ちは明示する。S2/S3 の待ちに相乗りすると、将来そちらの待ち方が
    // 変わったときにここだけ間欠的に落ちるようになる。
    const shown = await waitForLog("既存のウィンドウを表示します", 5000);
    record(
      "S4 2 個目の起動で既存インスタンスがウィンドウを出す (WM_COPYDATA が届く)",
      Boolean(shown),
      shown ?? "既存インスタンス側のコールバックが走っていない",
    );

    // --- S5: 起動競合 (ほぼ同時に 2 個) でも「ちょうど 1 個」が生き残る
    //
    // ここが今回の本題。プラグインだけだと、1 個目が隠しウィンドウを
    // 作り終える前に 2 個目が判定へ来て**両方生き残る**。
    // 一度きりでは再現しないので複数回まわす。
    //
    // **判定は「1 以下」ではなく「ちょうど 1」でなければならない。**
    // `<= 1` にすると、互いに譲り合って**両方 exit する** regression
    // (= アプリが起動しなくなる、二重起動より重い故障) を緑で通してしまう。
    // 「多重起動しない」と「ちゃんと起動する」は別の主張で、両方要る。
    //
    // さらに「1 個生きている」だけでは、**起動途中で固まった 1 個**と
    // 区別がつかない。ミューテックスは run() の 1 行目なので、
    // 「弾かれずに進んだが初期化に失敗した」プロセスもプロセス数 1 に見える。
    // 生き残りがフック設置ログまで到達したことを毎回確認する。
    killAll();
    await sleep(1000);
    const rounds = [];
    for (let i = 0; i < RACE_ROUNDS; i += 1) {
      markLog();
      // 間を空けずに 2 個投げる。片方は必ずミューテックスで弾かれるはず。
      const a = launch();
      const b = launch();
      await waitExit(a, SETTLE);
      await waitExit(b, SETTLE);
      // 生き残りが起動を完了したか (= フックまで到達したか)。
      const booted = await waitForLog("キーボードフックを設置", BOOT_TIMEOUT);
      // 敗者が理由を残したか。無言で消える経路が復活したらここで気づく。
      const bailed = readNewLog().includes("nox-voice は既に起動しています");
      const alive = aliveCount();
      rounds.push({ i: i + 1, alive, booted: Boolean(booted), bailed });
      killAll();
      await sleep(800);
    }
    const bad = rounds.filter((r) => !(r.alive === 1 && r.booted && r.bailed));
    record(
      `S5 同時起動 ${RACE_ROUNDS} 回すべてで「ちょうど 1 個」が起動しきる`,
      bad.length === 0,
      rounds
        .map(
          (r) =>
            `#${r.i}: 生存=${r.alive} / 起動完了=${r.booted ? "済" : "未"} / 敗者のログ=${
              r.bailed ? "有" : "無"
            }`,
        )
        .join("\n        ") +
        "\n        (生存 2 = 競合が塞げていない / 生存 0 = 両方が譲り合って落ちた" +
        " / 起動完了が未 = 1 個残ったが起動途中で固まっている)",
    );

    void first;
  } finally {
    killAll();
  }

  const failed = results.filter((r) => r.verdict === "FAIL");
  console.log(
    `\n=== PASS ${results.length - failed.length} / FAIL ${failed.length} (全 ${results.length}) ===`,
  );
  process.exit(failed.length === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(`E2E が異常終了: ${e.stack || e}`);
  killAll();
  process.exit(2);
});
