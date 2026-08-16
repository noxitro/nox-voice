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

const STATUS_LABEL: Record<Status, string> = {
  idle: "待機中",
  recording: "録音中",
  processing: "処理中",
};

let statusCard: HTMLElement | null = null;
let statusText: HTMLElement | null = null;
let sessionList: HTMLElement | null = null;
let errorEl: HTMLElement | null = null;

function renderStatus(status: Status, message?: string | null) {
  if (statusCard) statusCard.dataset.status = status;
  if (statusText) statusText.textContent = STATUS_LABEL[status] ?? status;
  if (message) showError(message);
}

function showError(message: string) {
  if (!errorEl) return;
  errorEl.textContent = message;
  errorEl.hidden = false;
}

function clearError() {
  if (!errorEl) return;
  errorEl.textContent = "";
  errorEl.hidden = true;
}

function renderSession(s: SessionSummary) {
  if (!sessionList) return;
  const rows: [string, string][] = [
    ["長さ", `${(s.duration_ms / 1000).toFixed(2)} 秒`],
    ["WAV サイズ", `${s.wav_bytes.toLocaleString()} バイト`],
    ["形式", `${s.sample_rate} Hz / mono / 16bit`],
    [
      "挿入先",
      s.target_hwnd === 0
        ? "不明(前景ウィンドウを特定できず)"
        : `${s.target_process} (hwnd 0x${s.target_hwnd.toString(16).toUpperCase()})`,
    ],
    ["ウィンドウ", s.target_title || "(タイトルなし)"],
    ["開始時刻", new Date(s.started_at_ms).toLocaleTimeString()],
  ];

  sessionList.replaceChildren(
    ...rows.flatMap(([key, value]) => {
      const dt = document.createElement("dt");
      dt.textContent = key;
      const dd = document.createElement("dd");
      dd.textContent = value;
      return [dt, dd];
    }),
  );
}

window.addEventListener("DOMContentLoaded", async () => {
  statusCard = document.querySelector("#status-card");
  statusText = document.querySelector("#status-text");
  sessionList = document.querySelector("#session");
  errorEl = document.querySelector("#error");

  await listen<StatusPayload>("nox://status", (event) => {
    if (event.payload.status !== "idle" || !event.payload.message) clearError();
    renderStatus(event.payload.status, event.payload.message);
  });

  await listen<SessionSummary>("nox://session", (event) => {
    renderSession(event.payload);
  });

  await listen<string>("nox://error", (event) => {
    showError(event.payload);
  });

  // 初期表示は Rust 側の現在値に合わせる (イベントを取り逃していても正しく出る)。
  try {
    renderStatus(await invoke<Status>("get_status"));
    const last = await invoke<SessionSummary | null>("get_last_session");
    if (last) renderSession(last);
  } catch (e) {
    showError(`状態の取得に失敗しました: ${e}`);
  }
});
