import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * オーバーレイ小窓の表示ロジック。
 *
 * ここはイベントを受けて見た目を変えるだけ。**入力は一切受け取らない**
 * (ウィンドウ側でクリックスルーとフォーカス無効を設定済み)。
 */

type Status = "idle" | "recording" | "processing";

/** 状態の出どころ。オーバーレイは録音由来だけを映す。 */
type StatusOrigin = "recording" | "background";

interface StatusPayload {
  status: Status;
  message: string | null;
  origin: StatusOrigin;
}

interface ErrorPayload {
  message: string;
  origin: StatusOrigin;
}

interface ResultPayload {
  text: string;
  stt_ms: number;
  format_ms: number;
  total_ms: number;
  degraded: boolean;
  injected: boolean;
  inject_outcome: string;
}

const el = <T extends HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

/** 録音開始時刻 (経過秒の表示用)。 */
let recordingStartedAt: number | null = null;
let elapsedTimer: number | undefined;

function setState(
  state: "recording" | "processing" | "done" | "error",
  label: string,
  detail?: string,
) {
  const pill = el("pill");
  if (pill) {
    pill.dataset.state = state;
    pill.hidden = false;
  }
  const labelEl = el("label");
  if (labelEl) labelEl.textContent = label;

  // アイコンは overlay.html に描いた SVG を CSS が data-state で出し分ける。
  // ここで textContent を書くと、その SVG を消してしまう。

  const detailEl = el("detail");
  if (detailEl) {
    detailEl.textContent = detail ?? "";
    detailEl.hidden = !detail;
  }

  const meter = el("meter");
  if (meter) meter.hidden = state !== "recording";
  const elapsed = el("elapsed");
  if (elapsed && state !== "recording") elapsed.textContent = "";

  // 疎通確認: この invoke が通れば IPC が生きている。
  // capability に "overlay" が無いと拒否され、ログに何も出ない。
  void invoke("overlay_rendered", { state }).catch(() => {
    /* 疎通確認なので失敗しても表示は続ける */
  });
}

function startElapsed() {
  recordingStartedAt = Date.now();
  window.clearInterval(elapsedTimer);
  const tick = () => {
    if (recordingStartedAt === null) return;
    const seconds = (Date.now() - recordingStartedAt) / 1000;
    const elapsed = el("elapsed");
    if (elapsed) elapsed.textContent = `${seconds.toFixed(1)}s`;
  };
  tick();
  elapsedTimer = window.setInterval(tick, 100);
}

function stopElapsed() {
  recordingStartedAt = null;
  window.clearInterval(elapsedTimer);
}

/** ミリ秒を「0.4s」形式に。 */
function seconds(ms: number): string {
  return `${(ms / 1000).toFixed(1)}s`;
}

window.addEventListener("DOMContentLoaded", async () => {
  await listen<StatusPayload>("nox://status", (event) => {
    // 再転写などの裏方作業は映さない。映すと完了イベントが来ず
    // 「認識中…」で固まる。
    if (event.payload.origin !== "recording") {
      void invoke("overlay_rendered", {
        state: `${event.payload.status}(裏方なので無視)`,
      }).catch(() => {});
      return;
    }

    switch (event.payload.status) {
      case "recording":
        setState("recording", "録音中");
        startElapsed();
        break;
      case "processing":
        stopElapsed();
        setState("processing", "認識中…");
        break;
      case "idle":
        stopElapsed();
        // 結果イベントが続くので、ここでは消さない。
        // 畳むのは Rust 側 (overlay::hide_after)。
        void invoke("overlay_rendered", { state: "idle(結果待ち)" }).catch(() => {});
        break;
    }
  });

  await listen<number>("nox://level", (event) => {
    const fill = el("meter-fill");
    // 幅ではなく scaleX。毎フレームのレイアウト再計算を避ける。
    if (fill) fill.style.transform = `scaleX(${Math.min(1, Math.max(0, event.payload))})`;
  });

  await listen<ResultPayload>("nox://result", (event) => {
    const r = event.payload;
    // 挿入されたテキストの先頭を見せる。何が入ったか一目で分かるように。
    const preview = r.text.replace(/\s+/g, " ").trim().slice(0, 24);
    const timing = `転写 ${seconds(r.stt_ms)} / 整形 ${seconds(r.format_ms)}`;
    const label = preview ? `「${preview}${r.text.length > 24 ? "…" : ""}」` : "完了";
    // 小窓を畳むのは Rust 側 (overlay::hide_after)。
    // webview に持たせると、次の録音で出した直後に前回のタイマーが消してしまう。
    setState("done", label, r.degraded ? `${timing}(整形なし)` : timing);
  });

  await listen<ErrorPayload>("nox://error", (event) => {
    // 裏方 (再転写など) のエラーは映さない。録音中の表示を奪ってしまう。
    if (event.payload.origin !== "recording") return;
    const first = event.payload.message.split("\n")[0] ?? "エラー";
    setState("error", first.slice(0, 30), "詳細は履歴から確認できます");
  });

  // 待受が張れたことを Rust 側へ知らせる。ここが届いていれば
  // capability が正しく、イベントも invoke も通っている。
  await invoke("overlay_ready", { listeners: 4 });
});
