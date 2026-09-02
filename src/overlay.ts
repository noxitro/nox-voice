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
  /** 画面質問モードでの録音か。小窓の色分けに使う (overlay.css)。 */
  screen_ask: boolean;
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
  /** Rust 側 `inject::ClipboardState` の snake_case 表現。 */
  clipboard_state: string;
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
  screenAsk = false,
  pendingClipboard = false,
) {
  const pill = el("pill");
  if (pill) {
    pill.dataset.state = state;
    // 普段の書き取りと画面質問は同じアイコン (mic / spinner) なので、
    // 色を変えないと押し間違いに気づけない (実際に紛らわしいと報告があった)。
    pill.dataset.mode = screenAsk && (state === "recording" || state === "processing")
      ? "screen_ask"
      : "";
    // クリップボードのみモードと画面質問モードは**貼り付けをしない**設計
    // (design.md)。チェックだけ光らせて消えると、クリップボードに答えが
    // 待っていることに気付かないまま「何も起きなかった」と見える
    // (実際にこの順で報告があった)。貼付済みと見分けが付く絵にして、
    // 消えるまでの時間も Rust 側 (OVERLAY_CLIPBOARD_LINGER) で長くしてある。
    pill.dataset.pending = state === "done" && pendingClipboard ? "clipboard" : "";
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
        setState(
          "recording",
          event.payload.screen_ask ? "画面に質問" : "録音中",
          undefined,
          event.payload.screen_ask,
        );
        startElapsed();
        break;
      case "processing":
        stopElapsed();
        setState(
          "processing",
          event.payload.screen_ask ? "画面を確認中…" : "認識中…",
          undefined,
          event.payload.screen_ask,
        );
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
    // 貼付済みなら完了はテキストを出さない (PRODUCT.md「成功は静かに」)。
    // 何が挿入されたかは履歴 (main.ts の履歴区画) でいつでも確認できる。
    //
    // ただしクリップボードどまり (クリップボードのみモード・画面質問モードは
    // 設計上ここに必ず入る) は話が別: 貼付というもう一段の作業が**まだ残って
    // いる**ので、静かに消えると「クリップボードに答えが用意された」こと
    // 自体に気付けない。ここだけは一言添える。
    const pending = !event.payload.injected;
    setState("done", pending ? "コピーしました" : "", undefined, false, pending);
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
