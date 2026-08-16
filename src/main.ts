import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Rust 側 `session::Status` と対応。 */
type Status = "idle" | "recording" | "processing";

interface StatusPayload {
  status: Status;
  message: string | null;
}

/** Rust 側 `session::SessionSummary` と対応。 */
interface SessionSummary {
  wav_bytes: number;
  sample_rate: number;
  duration_ms: number;
  started_at_ms: number;
  target_hwnd: number;
  target_process: string;
  target_title: string;
}

/** Rust 側 `pipeline::FormatOutcome` と対応 (serde の内部タグ表現)。 */
type FormatOutcome =
  | { kind: "formatted" }
  | { kind: "raw_fallback"; reason: string }
  | { kind: "disabled" };

/** Rust 側 `ResultPayload` と対応。 */
interface ResultPayload {
  raw_text: string;
  text: string;
  outcome: FormatOutcome;
  degraded: boolean;
  stt_ms: number;
  format_ms: number;
  total_ms: number;
  target_process: string;
  target_hwnd: number;
  duration_ms: number;
}

/** Rust 側 `config::ConfigView` と対応。**キーの実体は含まれない。** */
interface ConfigView {
  groq_key_set: boolean;
  groq_key_source: "env" | "config" | "none";
  groq_key_preview: string;
  gemini_key_set: boolean;
  gemini_key_source: "env" | "config" | "none";
  gemini_key_preview: string;
  language: string;
  dictionary: string[];
  formatting_enabled: boolean;
  stt_model: string;
  format_model: string;
}

const STATUS_LABEL: Record<Status, string> = {
  idle: "待機中",
  recording: "録音中",
  processing: "処理中",
};

const el = <T extends HTMLElement>(id: string) =>
  document.getElementById(id) as T | null;

/** 直近の結果。コピーボタンが参照する。 */
let lastResult: ResultPayload | null = null;

function renderStatus(status: Status, message?: string | null) {
  const card = el("status-card");
  if (card) card.dataset.status = status;
  const text = el("status-text");
  if (text) text.textContent = STATUS_LABEL[status] ?? status;
  if (message) showError(message);
}

function showError(message: string) {
  const node = el("error");
  if (!node) return;
  node.textContent = message;
  node.hidden = false;
}

function clearError() {
  const node = el("error");
  if (!node) return;
  node.textContent = "";
  node.hidden = true;
}

function renderSession(s: SessionSummary) {
  const list = el("session");
  if (!list) return;
  const rows: [string, string][] = [
    ["長さ", `${(s.duration_ms / 1000).toFixed(2)} 秒`],
    ["WAV サイズ", `${s.wav_bytes.toLocaleString()} バイト`],
    ["形式", `${s.sample_rate} Hz / mono / 16bit`],
    [
      "挿入先",
      s.target_hwnd === 0
        ? "不明(前景ウィンドウを特定できず)"
        : `${s.target_process} (hwnd 0x${s.target_hwnd
            .toString(16)
            .toUpperCase()})`,
    ],
    ["ウィンドウ", s.target_title || "(タイトルなし)"],
    ["開始時刻", new Date(s.started_at_ms).toLocaleTimeString()],
  ];

  list.replaceChildren(
    ...rows.flatMap(([key, value]) => {
      const dt = document.createElement("dt");
      dt.textContent = key;
      const dd = document.createElement("dd");
      dd.textContent = value;
      return [dt, dd];
    }),
  );
}

function renderResult(r: ResultPayload) {
  lastResult = r;

  const empty = el("result-empty");
  if (empty) empty.hidden = true;
  const box = el("result");
  if (box) box.hidden = false;

  const formatted = el("text-formatted");
  if (formatted) formatted.textContent = r.text;
  const raw = el("text-raw");
  if (raw) raw.textContent = r.raw_text;

  // 整形の結末をバッジで示す。劣化モードは理由まで出す (R2 / R5)。
  const badge = el("outcome-badge");
  const note = el<HTMLElement>("degraded-note");
  if (badge) {
    badge.hidden = false;
    switch (r.outcome.kind) {
      case "formatted":
        badge.textContent = "整形済み";
        badge.dataset.kind = "formatted";
        break;
      case "raw_fallback":
        badge.textContent = "劣化モード";
        badge.dataset.kind = "degraded";
        break;
      case "disabled":
        badge.textContent = "整形オフ";
        badge.dataset.kind = "disabled";
        break;
    }
  }
  if (note) {
    if (r.outcome.kind === "raw_fallback") {
      note.textContent = `整形できなかったため生転写を採用しました: ${r.outcome.reason}`;
      note.hidden = false;
    } else {
      note.hidden = true;
    }
  }

  const timings = el("timings");
  if (timings) {
    timings.textContent =
      `録音 ${(r.duration_ms / 1000).toFixed(2)} 秒 / ` +
      `転写 ${r.stt_ms} ms / 整形 ${r.format_ms} ms / 計 ${r.total_ms} ms`;
  }
}

async function copyText(kind: "formatted" | "raw", button: HTMLButtonElement) {
  if (!lastResult) return;
  const text = kind === "formatted" ? lastResult.text : lastResult.raw_text;
  const original = button.textContent;
  try {
    await navigator.clipboard.writeText(text);
    button.textContent = "コピーしました";
  } catch (e) {
    button.textContent = "コピー失敗";
    showError(`クリップボードへコピーできませんでした: ${e}`);
  }
  window.setTimeout(() => {
    button.textContent = original;
  }, 1500);
}

function keyStateLabel(view: ConfigView, which: "groq" | "gemini"): string {
  const set = which === "groq" ? view.groq_key_set : view.gemini_key_set;
  const source = which === "groq" ? view.groq_key_source : view.gemini_key_source;
  const preview =
    which === "groq" ? view.groq_key_preview : view.gemini_key_preview;
  if (!set) return " — 未設定";
  if (source === "env") return ` — 環境変数から (${preview})`;
  return ` — 設定済み (${preview})`;
}

function renderConfig(view: ConfigView) {
  const language = el<HTMLInputElement>("language");
  if (language) language.value = view.language;
  const formatting = el<HTMLInputElement>("formatting-enabled");
  if (formatting) formatting.checked = view.formatting_enabled;
  const groqState = el("groq-state");
  if (groqState) groqState.textContent = keyStateLabel(view, "groq");
  const geminiState = el("gemini-state");
  if (geminiState) geminiState.textContent = keyStateLabel(view, "gemini");
}

async function saveSettings(event: Event) {
  event.preventDefault();
  const note = el("settings-note");
  const groq = el<HTMLInputElement>("groq-key");
  const gemini = el<HTMLInputElement>("gemini-key");
  const language = el<HTMLInputElement>("language");
  const formatting = el<HTMLInputElement>("formatting-enabled");

  // 入力欄が空 = 「変更しない」。誤って既存キーを消さないため未指定で送る。
  const patch: Record<string, unknown> = {
    language: language?.value ?? "",
    formatting_enabled: formatting?.checked ?? true,
  };
  if (groq?.value) patch.groq_api_key = groq.value;
  if (gemini?.value) patch.gemini_api_key = gemini.value;

  try {
    const view = await invoke<ConfigView>("set_config", { patch });
    renderConfig(view);
    // 入力欄には残さない (画面に平文で残る時間を最小にする)。
    if (groq) groq.value = "";
    if (gemini) gemini.value = "";
    if (note) note.textContent = "保存しました";
  } catch (e) {
    if (note) note.textContent = `保存に失敗しました: ${e}`;
  }
  window.setTimeout(() => {
    if (note) note.textContent = "";
  }, 2500);
}

window.addEventListener("DOMContentLoaded", async () => {
  document.querySelectorAll<HTMLButtonElement>("button.copy").forEach((btn) => {
    btn.addEventListener("click", () => {
      const kind = btn.dataset.copy === "raw" ? "raw" : "formatted";
      void copyText(kind, btn);
    });
  });
  el("settings-form")?.addEventListener("submit", (e) => void saveSettings(e));

  await listen<StatusPayload>("nox://status", (event) => {
    if (event.payload.status === "recording") clearError();
    renderStatus(event.payload.status, event.payload.message);
  });
  await listen<SessionSummary>("nox://session", (event) =>
    renderSession(event.payload),
  );
  await listen<ResultPayload>("nox://result", (event) =>
    renderResult(event.payload),
  );
  await listen<string>("nox://error", (event) => showError(event.payload));

  // 初期表示は Rust 側の現在値に合わせる (イベントを取り逃していても正しく出る)。
  try {
    renderStatus(await invoke<Status>("get_status"));
    const session = await invoke<SessionSummary | null>("get_last_session");
    if (session) renderSession(session);
    const result = await invoke<ResultPayload | null>("get_last_result");
    if (result) renderResult(result);
    renderConfig(await invoke<ConfigView>("get_config"));
  } catch (e) {
    showError(`状態の取得に失敗しました: ${e}`);
  }
});
