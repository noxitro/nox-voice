import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * オーバーレイ小窓の表示ロジック。
 *
 * ここはイベントを受けて見た目を変えるだけ。**入力は一切受け取らない**
 * (ウィンドウ側でクリックスルーとフォーカス無効を設定済み)。
 *
 * **ウィンドウタイトルを console へ出さない。** Rust 側でタイトルを
 * ログに残さない方針 (design.md) は、小窓の webview でも同じ。
 */

type Status = "idle" | "recording" | "processing";

/** 状態の出どころ。オーバーレイは録音由来だけを映す。 */
type StatusOrigin = "recording" | "background";

/** 録音開始時に掴んだ相手ウィンドウ (Rust: `session::TargetView`)。 */
interface TargetPayload {
  hwnd: number;
  /** false = 前景を掴めなかった。 */
  known: boolean;
  app_name: string;
  title: string;
  /** アイコン (data URI)。キャッシュが当たったときだけ入る。 */
  icon: string | null;
  /** 画面質問モードで複数モニタのときだけ入る。 */
  monitor: string | null;
}

/** アプリの表示名とアイコンが揃ったときの後追い (Rust: `nox://target-icon`)。 */
interface TargetIconPayload {
  hwnd: number;
  app_name: string;
  title: string;
  icon: string | null;
}

interface StatusPayload {
  status: Status;
  message: string | null;
  origin: StatusOrigin;
  /** 画面質問モードでの録音か。小窓の色分けに使う (overlay.css)。 */
  screen_ask: boolean;
  /** 録音中・処理中に出す相手ウィンドウ。出さない場面では null。 */
  target: TargetPayload | null;
}

interface ErrorPayload {
  message: string;
  origin: StatusOrigin;
}

/** 整形の結末 (Rust: `pipeline::FormatOutcome`)。 */
interface FormatOutcome {
  /**
   * `fallback_formatted` = 主 (Gemini) が落ちて控え (Groq) が整形した。
   *
   * **小窓は何も出さない。** 出力は良好で利用者に打つ手が無いため
   * (`degraded` が false のまま来る)。履歴にはバッジと理由が残る。
   */
  kind: "formatted" | "fallback_formatted" | "raw_fallback" | "disabled";
  /** `raw_fallback` なら失敗理由、`fallback_formatted` なら**主の**失敗理由。 */
  reason?: string;
}

interface ResultPayload {
  text: string;
  stt_ms: number;
  format_ms: number;
  total_ms: number;
  outcome: FormatOutcome;
  degraded: boolean;
  injected: boolean;
  inject_outcome: string;
  /** Rust 側 `inject::ClipboardState` の snake_case 表現。 */
  clipboard_state: string;
}

/** 小窓の見た目 1 状態分。引数が増えたので位置引数ではなく名前で渡す。 */
interface View {
  state: "recording" | "processing" | "done" | "error";
  label: string;
  detail?: string;
  screenAsk?: boolean;
  pendingClipboard?: boolean;
  /** 整形が落ちて生転写のまま届いたか (R2 の劣化モード)。 */
  degraded?: boolean;
  target?: TargetPayload | null;
}

const el = <T extends HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

/** 録音開始時刻 (経過秒の表示用)。 */
let recordingStartedAt: number | null = null;
let elapsedTimer: number | undefined;

/**
 * いま相手として出している HWND。
 *
 * 遅れて届くアイコン (`nox://target-icon`) を突き合わせるのに使う。
 * **一致しない到着は捨てる** — 連続して別のアプリへ喋ったとき、前の録音の
 * アイコンが後から届いて今の表示を書き換えるのを防ぐ。
 */
let shownTargetHwnd: number | null = null;

function setState(view: View) {
  const {
    state,
    label,
    detail,
    screenAsk = false,
    pendingClipboard = false,
    degraded = false,
    target = null,
  } = view;

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
    // 整形が落ちて生転写のまま届いたことを言う (PRODUCT.md「無言の
    // フォールバックをしない」)。**ここを出さないと、API が落ちている間
    // ずっと「成功」の顔で生転写が貼られ続ける** — 実際 2026-09-05 に
    // Gemini が 503 を返し続けた日、7 回連続でそうなっていた。
    pill.dataset.degraded = state === "done" && degraded ? "true" : "";
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

  renderTarget(state, target);

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

/**
 * 相手ウィンドウの行を描く。
 *
 * 完了・エラーでは出さない (done の見た目は変えない / 終わった後に
 * 「どこに貼るか」を言う意味が無い)。
 *
 * **同じ相手なら組み直さない。** 録音中に届いたアイコンと改善後の表示名は、
 * 処理中への遷移で送られてくる状態イベント (開始時の値を持っている) に
 * 上書きされてはいけない。
 */
function renderTarget(view: View["state"], target: TargetPayload | null) {
  const row = el("target");
  const hint = el("hint");
  const image = el<HTMLImageElement>("target-img");
  const app = el("target-app");
  const title = el("target-title");
  if (!row || !hint || !image || !app || !title) return;

  const show = target !== null && (view === "recording" || view === "processing");
  if (!show || !target) {
    shownTargetHwnd = null;
    row.hidden = true;
    hint.hidden = true;
    return;
  }

  hint.textContent = target.monitor ?? "";
  hint.hidden = !target.monitor;

  if (shownTargetHwnd === target.hwnd && !row.hidden) {
    return; // 同じ相手。アイコンと改善後の名前をそのまま残す。
  }
  shownTargetHwnd = target.hwnd;

  // アイコンは相手ごとに取り直す。前の相手の絵を一瞬でも残さない
  // (src はそのままでよい。data-icon を外した時点で透明になり、
  // 次の到着では src を書いてから立て直す)。
  row.dataset.icon = "";
  // キャッシュが当たった分は状態イベントに同梱されている。ここで即座に
  // 出せば、後追い (nox://target-icon) との到着順に依存しない。
  if (target.known && target.icon) {
    image.src = target.icon;
    row.dataset.icon = "true";
  }

  if (target.known) {
    row.dataset.unknown = "";
    app.textContent = target.app_name;
    title.textContent = target.title;
  } else {
    // 前景を掴めなかった。**黙って何も言わない**より、貼れないかもしれない
    // と先に言う (PRODUCT.md「失敗は騒がしく」)。
    row.dataset.unknown = "true";
    app.textContent = "挿入先を特定できません";
    title.textContent = "";
  }
  // 画面質問モードで前景が取れないときは、行そのものは出さず補足だけ残す
  // (読むのはモニタであって、貼付先の話ではない)。
  row.hidden = !target.known && target.monitor !== null;
}

/**
 * 完了時の見出し。
 *
 * 貼付済みの成功は無言のまま (PRODUCT.md「成功は静かに」)。**劣化だけは
 * 貼付済みでも喋る** — 生転写が入ったことは、黙っていると気づけない。
 */
function degradedLabel(degraded: boolean, pending: boolean): string {
  if (degraded) {
    return pending ? "整形なし · コピーしました" : "整形なし · 生のまま貼りました";
  }
  return pending ? "コピーしました" : "";
}

/**
 * 失敗理由を 1 行に詰める。
 *
 * 理由は `Gemini のサーバエラー (503): {"error": ...}` のように API の
 * 応答本文が続く。小窓に JSON を出しても読めないので、最初の `:` で切る。
 */
function shortReason(reason: string | undefined): string | undefined {
  const head = (reason ?? "").split(":")[0]?.trim();
  return head ? head : undefined;
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
        setState({
          state: "recording",
          label: event.payload.screen_ask ? "画面に質問" : "録音中",
          screenAsk: event.payload.screen_ask,
          target: event.payload.target,
        });
        startElapsed();
        break;
      case "processing":
        stopElapsed();
        setState({
          state: "processing",
          label: event.payload.screen_ask ? "画面を確認中…" : "認識中…",
          screenAsk: event.payload.screen_ask,
          target: event.payload.target,
        });
        break;
      case "idle":
        stopElapsed();
        // 結果イベントが続くので、ここでは消さない。畳むのは Rust 側。
        // **キャンセルだけは結果イベントが来ない。** そのため Rust 側の
        // cancel_recording が hide_after ではなく hide で即座に畳む。
        // ここを「何もしない」に保つ以上、あちらを遅らせると録音中の顔が
        // 残り続けるので、片方だけ変えないこと。
        void invoke("overlay_rendered", { state: "idle(結果待ち)" }).catch(() => {});
        break;
    }
  });

  // 相手アプリの表示名とアイコンの後追い。録音開始を待たせないために
  // 状態イベントとは別便で来る (Rust 側 `resolve_target_icon`)。
  await listen<TargetIconPayload>("nox://target-icon", (event) => {
    // 今出している相手のものだけ反映する。古い録音の遅延到着は捨てる。
    if (shownTargetHwnd === null || event.payload.hwnd !== shownTargetHwnd) return;
    const row = el("target");
    const app = el("target-app");
    const title = el("target-title");
    const image = el<HTMLImageElement>("target-img");
    if (!row || !app || !title || !image || row.dataset.unknown === "true") return;

    if (event.payload.app_name) app.textContent = event.payload.app_name;
    title.textContent = event.payload.title;
    if (event.payload.icon) {
      image.src = event.payload.icon;
      // 属性を書いた直後に効かせると遷移が始まらない。次のフレームで立てる。
      requestAnimationFrame(() => {
        row.dataset.icon = "true";
      });
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
    const degraded = event.payload.degraded;
    setState({
      state: "done",
      label: degradedLabel(degraded, pending),
      // 理由を出す。「整形されなかった」だけだと、直せるもの (キーが無い)
      // なのか待てば直るもの (混雑) なのか判断できない。
      detail: degraded ? shortReason(event.payload.outcome.reason) : undefined,
      pendingClipboard: pending,
      degraded,
    });
  });

  await listen<ErrorPayload>("nox://error", (event) => {
    // 裏方 (再転写など) のエラーは映さない。録音中の表示を奪ってしまう。
    if (event.payload.origin !== "recording") return;
    const first = event.payload.message.split("\n")[0] ?? "エラー";
    setState({
      state: "error",
      label: first.slice(0, 30),
      detail: "詳細は履歴から確認できます",
    });
  });

  // 待受が張れたことを Rust 側へ知らせる。ここが届いていれば
  // capability が正しく、イベントも invoke も通っている。
  await invoke("overlay_ready", { listeners: 5 });
});
