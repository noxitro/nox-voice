import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Rust 側 `session::Status` と対応。 */
type Status = "idle" | "recording" | "processing";

interface StatusPayload {
  status: Status;
  message: string | null;
  origin: "recording" | "background";
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

/** Rust 側 `inject::InjectOutcome` と対応。 */
type InjectOutcome =
  | "injected"
  | "disabled"
  | "empty_text"
  | "aborted_focus_changed"
  | "aborted_target_unknown"
  | "aborted_modifier_stuck"
  | "clipboard_busy"
  | "clipboard_failed"
  | "send_failed";

/**
 * Rust 側 `inject::ClipboardState` と対応。
 * 「復元した」と「ユーザーが別のものをコピーした」を区別する
 * (後者で「Ctrl+V で貼れます」と案内すると嘘になる)。
 */
type ClipboardState =
  | "untouched"
  | "holds_injected_text"
  | "restored_original"
  | "replaced_by_user"
  | "lost";

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
  injected: boolean;
  inject_outcome: InjectOutcome;
  clipboard_state: ClipboardState;
  lost_clipboard_formats: string[];
}

/** Rust 側 `history::SessionRow` と対応。 */
interface SessionRow {
  id: number;
  started_at_ms: number;
  duration_ms: number;
  target_process: string;
  target_hwnd: number;
  raw_text: string | null;
  formatted_text: string | null;
  outcome: string;
  outcome_reason: string | null;
  stt_ms: number | null;
  format_ms: number | null;
  inject_outcome: string | null;
  clipboard_state: string | null;
  has_audio: boolean;
  created_at_ms: number;
}

/** Rust 側 `style::StyleProfile` と対応。 */
interface StyleProfile {
  process: string;
  title_contains: string | null;
  instruction: string;
}

/**
 * スタイルプロファイルを 1 行 1 件のテキストに変換する。
 * `プロセス名 | 指示` または `プロセス名 | タイトル条件 | 指示`。
 */
function styleProfilesToText(profiles: StyleProfile[]): string {
  return profiles
    .map((p) =>
      p.title_contains
        ? `${p.process} | ${p.title_contains} | ${p.instruction}`
        : `${p.process} | ${p.instruction}`,
    )
    .join("\n");
}

/** 上の逆変換。壊れた行は落とす(Rust 側でも空欄は弾かれる)。 */
function parseStyleProfiles(text: string): StyleProfile[] {
  return text
    .split(/\r?\n/)
    .map((line) => line.split("|").map((part) => part.trim()))
    .flatMap((parts): StyleProfile[] => {
      if (parts.length === 2 && parts[0] && parts[1]) {
        return [{ process: parts[0], title_contains: null, instruction: parts[1] }];
      }
      if (parts.length >= 3 && parts[0] && parts[2]) {
        return [
          {
            process: parts[0],
            title_contains: parts[1] || null,
            // 指示自体に「|」が入っていても失わないよう繋ぎ直す。
            instruction: parts.slice(2).join(" | "),
          },
        ];
      }
      return [];
    });
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
  injection_enabled: boolean;
  deep_context: boolean;
  style_profiles: StyleProfile[];
  local_stt_mode: "off" | "fallback" | "only";
  start_hidden: boolean;
  hotkey_vk: number;
  hotkey_label: string;
  overlay_enabled: boolean;
  history_enabled: boolean;
  history_retention_days: number;
  restore_delay_ms: number;
  stt_model: string;
  format_model: string;
}

/**
 * 注入結果の説明。`injected` は「Ctrl+V を送出した」という意味で、
 * 相手アプリに貼られた保証ではない (Rust 側 inject.rs のモジュール doc)。
 */
const INJECT_LABEL: Record<InjectOutcome, string> = {
  injected: "貼り付けを送出しました",
  disabled: "自動貼り付けは無効です",
  empty_text: "貼り付けるテキストがありません",
  aborted_focus_changed: "挿入先が変わったため中止(Ctrl+V で貼り付け可)",
  aborted_target_unknown: "挿入先を特定できず中止(Ctrl+V で貼り付け可)",
  aborted_modifier_stuck: "修飾キー押下中のため中止(Ctrl+V で貼り付け可)",
  clipboard_busy: "クリップボードが使用中で貼り付けできませんでした",
  clipboard_failed: "クリップボードへの書き込みに失敗しました",
  send_failed: "キー入力の送出に失敗(Ctrl+V で貼り付け可)",
};

/** クリップボードの終状態の説明。触っていない場合は何も出さない。 */
const CLIPBOARD_LABEL: Record<ClipboardState, string> = {
  untouched: "",
  holds_injected_text: "クリップボードに入っています (Ctrl+V で貼り付け可)",
  restored_original: "クリップボードは元に戻しました",
  replaced_by_user: "クリップボードは新しくコピーされた内容のままです",
  lost: "元のクリップボード内容を復元できませんでした",
};

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

  // 注入の結末。成功時は静かに、中止・失敗時は理由を出す。
  const injectNote = el<HTMLElement>("inject-note");
  if (injectNote) {
    const label = INJECT_LABEL[r.inject_outcome] ?? r.inject_outcome;
    const clipboard = CLIPBOARD_LABEL[r.clipboard_state];
    injectNote.textContent = clipboard ? `${label} / ${clipboard}` : label;
    // 手を動かす必要がある状態なら目立たせる。
    const needsAction =
      r.clipboard_state === "holds_injected_text" ||
      r.clipboard_state === "lost" ||
      r.lost_clipboard_formats.length > 0;
    injectNote.dataset.kind = needsAction ? "warn" : r.injected ? "ok" : "warn";
    injectNote.hidden = r.inject_outcome === "disabled";
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
  const injection = el<HTMLInputElement>("injection-enabled");
  if (injection) injection.checked = view.injection_enabled;
  const restoreDelay = el<HTMLInputElement>("restore-delay");
  if (restoreDelay) restoreDelay.value = String(view.restore_delay_ms);
  const historyEnabled = el<HTMLInputElement>("history-enabled");
  if (historyEnabled) historyEnabled.checked = view.history_enabled;
  const retention = el<HTMLInputElement>("history-retention");
  if (retention) retention.value = String(view.history_retention_days);
  const dictionary = el<HTMLTextAreaElement>("dictionary");
  if (dictionary) dictionary.value = view.dictionary.join("\n");
  const deepContext = el<HTMLInputElement>("deep-context");
  if (deepContext) deepContext.checked = view.deep_context;
  const overlayEnabled = el<HTMLInputElement>("overlay-enabled");
  if (overlayEnabled) overlayEnabled.checked = view.overlay_enabled;
  const startHidden = el<HTMLInputElement>("start-hidden");
  if (startHidden) startHidden.checked = view.start_hidden;
  const localMode = el<HTMLSelectElement>("local-stt-mode");
  if (localMode) localMode.value = view.local_stt_mode;
  const hotkeyLabel = el("hotkey-label");
  if (hotkeyLabel) {
    hotkeyLabel.textContent = view.hotkey_label;
    delete hotkeyLabel.dataset.capturing;
  }
  const styles = el<HTMLTextAreaElement>("style-profiles");
  if (styles) styles.value = styleProfilesToText(view.style_profiles);
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
  const injection = el<HTMLInputElement>("injection-enabled");
  const restoreDelay = el<HTMLInputElement>("restore-delay");
  const historyEnabled = el<HTMLInputElement>("history-enabled");
  const retention = el<HTMLInputElement>("history-retention");
  const dictionary = el<HTMLTextAreaElement>("dictionary");
  const deepContext = el<HTMLInputElement>("deep-context");
  const overlayEnabled = el<HTMLInputElement>("overlay-enabled");
  const startHidden = el<HTMLInputElement>("start-hidden");
  const localMode = el<HTMLSelectElement>("local-stt-mode");
  const styles = el<HTMLTextAreaElement>("style-profiles");

  // 入力欄が空 = 「変更しない」。誤って既存キーを消さないため未指定で送る。
  const patch: Record<string, unknown> = {
    language: language?.value ?? "",
    formatting_enabled: formatting?.checked ?? true,
    injection_enabled: injection?.checked ?? true,
    history_enabled: historyEnabled?.checked ?? true,
    deep_context: deepContext?.checked ?? false,
    overlay_enabled: overlayEnabled?.checked ?? true,
    start_hidden: startHidden?.checked ?? true,
    local_stt_mode: localMode?.value ?? "fallback",
    // 空行は Rust 側で落とされる。
    dictionary: (dictionary?.value ?? "").split(/\r?\n/),
    style_profiles: parseStyleProfiles(styles?.value ?? ""),
  };
  const days = Number(retention?.value);
  if (Number.isFinite(days) && days >= 0) patch.history_retention_days = days;

  // 形式が違ってパースできなかった行は黙って消える。何行落としたかを伝える。
  const styleText = styles?.value ?? "";
  const styleLineCount = styleText
    .split(/\r?\n/)
    .filter((line) => line.trim() !== "").length;
  const ignoredStyleLines =
    styleLineCount - (patch.style_profiles as StyleProfile[]).length;
  // 数値として読めないときは送らない (Rust 側の範囲でクランプされる)。
  const delay = Number(restoreDelay?.value);
  if (Number.isFinite(delay) && delay > 0) patch.restore_delay_ms = delay;
  if (groq?.value) patch.groq_api_key = groq.value;
  if (gemini?.value) patch.gemini_api_key = gemini.value;

  try {
    const view = await invoke<ConfigView>("set_config", { patch });
    renderConfig(view);
    // 入力欄には残さない (画面に平文で残る時間を最小にする)。
    if (groq) groq.value = "";
    if (gemini) gemini.value = "";
    if (note) {
      note.textContent =
        ignoredStyleLines > 0
          ? `保存しました(文体の設定 ${ignoredStyleLines} 行は形式が違うため無視しました)`
          : "保存しました";
    }
  } catch (e) {
    if (note) note.textContent = `保存に失敗しました: ${e}`;
  }
  // 無視した行があるときは、読む時間を長めに取る。
  window.setTimeout(
    () => {
      if (note) note.textContent = "";
    },
    ignoredStyleLines > 0 ? 8000 : 2500,
  );
}

/** 履歴の結末バッジ。 */
const OUTCOME_BADGE: Record<string, string> = {
  formatted: "整形済",
  raw_fallback: "劣化",
  disabled: "整形オフ",
  untranscribed: "未転写",
};

/** 現在表示している履歴。追記読み込みで伸びる。 */
let historyRows: SessionRow[] = [];
/** 展開中の行 ID。 */
let expandedId: number | null = null;

function showHistoryError(message: string | null) {
  const node = el("history-error");
  if (!node) return;
  if (message === null) {
    node.textContent = "";
    node.hidden = true;
    return;
  }
  node.textContent = message;
  node.hidden = false;
}

function historyPreview(row: SessionRow): string {
  const text = row.formatted_text || row.raw_text || "";
  const oneLine = text.replace(/\s+/g, " ").trim();
  if (!oneLine) return row.has_audio ? "(未転写の録音)" : "(テキストなし)";
  return oneLine.length > 60 ? `${oneLine.slice(0, 60)}…` : oneLine;
}

function buildHistoryItem(row: SessionRow): HTMLLIElement {
  const li = document.createElement("li");
  li.className = "history-item";
  li.dataset.id = String(row.id);

  const head = document.createElement("button");
  head.type = "button";
  head.className = "history-head";
  head.setAttribute("aria-expanded", String(expandedId === row.id));

  const meta = document.createElement("span");
  meta.className = "history-meta";
  meta.textContent = `${new Date(row.started_at_ms).toLocaleString()} · ${
    row.target_process
  }`;

  const badge = document.createElement("span");
  badge.className = "badge";
  badge.dataset.kind =
    row.outcome === "formatted"
      ? "formatted"
      : row.outcome === "untranscribed"
        ? "untranscribed"
        : "degraded";
  badge.textContent = OUTCOME_BADGE[row.outcome] ?? row.outcome;

  const preview = document.createElement("span");
  preview.className = "history-preview";
  preview.textContent = historyPreview(row);

  const topLine = document.createElement("span");
  topLine.className = "history-topline";
  topLine.append(meta, badge);
  head.append(topLine, preview);
  head.addEventListener("click", () => {
    expandedId = expandedId === row.id ? null : row.id;
    renderHistory();
  });
  li.append(head);

  if (expandedId === row.id) {
    li.append(buildHistoryDetail(row));
  }
  return li;
}

function buildHistoryDetail(row: SessionRow): HTMLElement {
  const detail = document.createElement("div");
  detail.className = "history-detail";

  if (row.outcome_reason) {
    const reason = document.createElement("p");
    reason.className = "degraded-note";
    reason.textContent = row.outcome_reason;
    detail.append(reason);
  }

  // R5: 生転写と整形結果を並置して、欠落やハルシネーションを照合できるようにする。
  const addBlock = (title: string, text: string | null) => {
    if (text === null) return;
    const block = document.createElement("div");
    block.className = "text-block";
    const h = document.createElement("h3");
    h.textContent = title;
    const pre = document.createElement("pre");
    pre.className = "text";
    pre.textContent = text;
    block.append(h, pre);
    detail.append(block);
  };
  addBlock("整形後", row.formatted_text);
  addBlock("生転写", row.raw_text);

  // 貼付の結末を出す。不達だった行を見つけて再貼付するのに要る。
  if (row.inject_outcome) {
    const inject = document.createElement("p");
    inject.className = "inject-note";
    const label = INJECT_LABEL[row.inject_outcome as InjectOutcome] ?? row.inject_outcome;
    const clipboard = row.clipboard_state
      ? CLIPBOARD_LABEL[row.clipboard_state as ClipboardState]
      : "";
    inject.textContent = clipboard ? `${label} / ${clipboard}` : label;
    inject.dataset.kind = row.inject_outcome === "injected" ? "ok" : "warn";
    detail.append(inject);
  }

  const actions = document.createElement("div");
  actions.className = "history-actions";

  const hasText = Boolean(row.formatted_text || row.raw_text);
  if (hasText) {
    // コピーも再貼付も同じ経路 (Rust 側の copy_history_entry) に通す。
    // navigator.clipboard だと履歴除外フォーマットが付かず、発話が
    // Win+V 履歴やクラウドクリップボードへ流れてしまう。
    const copy = document.createElement("button");
    copy.type = "button";
    copy.textContent = "コピー";
    copy.title = "クリップボードに入れます (Win+V 履歴には残しません)";
    copy.addEventListener("click", () => {
      void invoke("copy_history_entry", { id: row.id }).catch((e) => {
        showHistoryError(`コピーに失敗しました: ${e}`);
      });
    });
    actions.append(copy);
  }

  if (row.has_audio && row.outcome === "untranscribed") {
    const retry = document.createElement("button");
    retry.type = "button";
    retry.textContent = "再転写";
    retry.addEventListener("click", () => {
      retry.disabled = true;
      retry.textContent = "再転写中…";
      // バックエンドにも進行中ガードがあるので、連打しても二重には走らない。
      void invoke("retranscribe_history_entry", { id: row.id }).catch((e) => {
        showHistoryError(`再転写を開始できません: ${e}`);
        retry.disabled = false;
        retry.textContent = "再転写";
      });
    });
    actions.append(retry);
  }

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "danger";
  remove.textContent = "削除";
  remove.addEventListener("click", () => {
    if (!window.confirm("この履歴を削除しますか?")) return;
    void invoke("delete_history_entry", { id: row.id }).catch((e) => {
      showHistoryError(`削除に失敗しました: ${e}`);
    });
  });
  actions.append(remove);

  detail.append(actions);
  return detail;
}

/** 履歴の読み込みに失敗しているか。0 件と区別する。 */
let historyLoadFailed = false;

function renderHistory() {
  const list = el("history-list");
  const empty = el("history-empty");
  if (!list) return;
  list.replaceChildren(...historyRows.map(buildHistoryItem));
  // 「読めなかった」を「0 件」と表示しない。消えたと誤解させる。
  if (empty) {
    empty.hidden = historyRows.length > 0 || historyLoadFailed;
    empty.textContent = historyQuery
      ? `「${historyQuery}」に一致する履歴はありません`
      : "履歴はまだありません";
  }
}

const HISTORY_PAGE = 50;

/** 現在の検索語。 */
let historyQuery = "";
/** 入力のたびに問い合わせないための遅延。 */
let searchDebounce: number | undefined;

interface StorageStats {
  failed_bytes: number;
  failed_files: number;
  untranscribed_rows: number;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

interface LocalSttStatus {
  state: "ready" | "not_compiled" | "model_missing";
  path?: string;
  bytes?: number;
  expected_path?: string;
}

async function loadLocalSttStatus() {
  const text = el("local-stt-text");
  const button = el<HTMLButtonElement>("local-stt-download");
  try {
    const status = await invoke<LocalSttStatus>("get_local_stt_status");
    if (text) {
      text.textContent =
        status.state === "ready"
          ? `モデル準備済み (${formatBytes(status.bytes ?? 0)})`
          : status.state === "not_compiled"
            ? "このビルドにはローカル認識が含まれていません"
            : "モデル未ダウンロード";
    }
    // ビルドに含まれていないならダウンロードしても使えない。
    if (button) button.disabled = status.state === "not_compiled";
  } catch (e) {
    if (text) text.textContent = `状態を取得できません (${e})`;
    if (button) button.disabled = true;
  }
}

async function loadStorageStats() {
  const text = el("storage-text");
  const button = el<HTMLButtonElement>("storage-clear");
  try {
    const stats = await invoke<StorageStats>("get_storage_stats");
    if (text) {
      text.textContent =
        stats.failed_files === 0
          ? "退避した録音: なし"
          : `退避した録音: ${stats.failed_files} 件 / ${formatBytes(stats.failed_bytes)}` +
            `(未転写 ${stats.untranscribed_rows} 件)`;
    }
    if (button) button.disabled = stats.untranscribed_rows === 0;
  } catch (e) {
    if (text) text.textContent = `退避した録音: 集計できません (${e})`;
    if (button) button.disabled = true;
  }
}

async function loadHistory(append = false) {
  const more = el<HTMLButtonElement>("history-more");
  try {
    const beforeId =
      append && historyRows.length > 0
        ? historyRows[historyRows.length - 1].id
        : null;
    const rows = await invoke<SessionRow[]>("get_history", {
      limit: HISTORY_PAGE,
      beforeId,
      query: historyQuery || null,
    });
    historyRows = append ? [...historyRows, ...rows] : rows;
    historyLoadFailed = false;
    showHistoryError(null);
    renderHistory();
    if (more) more.hidden = rows.length < HISTORY_PAGE;
  } catch (e) {
    // 「読めなかった」を「0 件」と表示しない。消えたと誤解させる。
    historyLoadFailed = true;
    if (!append) historyRows = [];
    renderHistory();
    showHistoryError(`履歴を読み込めませんでした: ${e}`);
    if (more) more.hidden = true;
  }
}

/** ホットキー捕獲の結果。`null` は取り消し。 */
interface HotkeyCaptured {
  vk: number;
  label: string;
}

/** 捕獲モードに入っているか (UI 側の見た目用)。 */
let capturingHotkey = false;
/** 残り秒のカウントダウン。 */
let captureCountdown: number | undefined;

function setHotkeyCapturing(active: boolean, seconds = 0) {
  capturingHotkey = active;
  window.clearInterval(captureCountdown);

  const label = el("hotkey-label");
  const button = el<HTMLButtonElement>("hotkey-capture");
  if (label) {
    if (active) {
      label.dataset.capturing = "true";
    } else {
      delete label.dataset.capturing;
    }
  }
  if (button) button.textContent = active ? "キャンセル" : "キーを押して設定";

  if (!active) return;

  // 残り時間を出す。捕獲はグローバルなので、入りっぱなしだと
  // 他アプリで打ったキーを拾ってしまう。時間が見えている方が安全。
  let remaining = seconds;
  const tick = () => {
    if (label) {
      label.textContent =
        remaining > 0 ? `キーを押してください… (${remaining})` : "キーを押してください…";
    }
    remaining -= 1;
    if (remaining < 0) window.clearInterval(captureCountdown);
  };
  tick();
  captureCountdown = window.setInterval(tick, 1000);
}

async function cancelHotkeyCapture() {
  setHotkeyCapturing(false);
  await invoke("cancel_hotkey_capture");
  renderConfig(await invoke<ConfigView>("get_config"));
}

async function toggleHotkeyCapture() {
  if (capturingHotkey) {
    await cancelHotkeyCapture();
    return;
  }
  try {
    const seconds = await invoke<number>("start_hotkey_capture");
    setHotkeyCapturing(true, seconds);
  } catch (e) {
    setHotkeyCapturing(false);
    showError(`${e}`);
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  document.querySelectorAll<HTMLButtonElement>("button.copy").forEach((btn) => {
    btn.addEventListener("click", () => {
      const kind = btn.dataset.copy === "raw" ? "raw" : "formatted";
      void copyText(kind, btn);
    });
  });
  el("settings-form")?.addEventListener("submit", (e) => void saveSettings(e));
  el("hotkey-capture")?.addEventListener("click", () => void toggleHotkeyCapture());

  // ウィンドウから離れたら捕獲をやめる。設定画面を離れたまま
  // 捕獲が続くと、他アプリで打ったキーがホットキーとして保存される。
  window.addEventListener("blur", () => {
    if (capturingHotkey) void cancelHotkeyCapture();
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden && capturingHotkey) void cancelHotkeyCapture();
  });
  el("history-more")?.addEventListener("click", () => void loadHistory(true));
  el<HTMLInputElement>("history-search")?.addEventListener("input", (e) => {
    historyQuery = (e.target as HTMLInputElement).value.trim();
    // 打つたびに DB を叩かない。
    window.clearTimeout(searchDebounce);
    searchDebounce = window.setTimeout(() => void loadHistory(), 200);
  });
  el("local-stt-download")?.addEventListener("click", () => {
    const button = el<HTMLButtonElement>("local-stt-download");
    if (button) {
      button.disabled = true;
      button.textContent = "ダウンロード中…";
    }
    void invoke("download_local_model").catch((e) => {
      showError(`ダウンロードを開始できません: ${e}`);
      if (button) {
        button.disabled = false;
        button.textContent = "モデルをダウンロード";
      }
    });
  });
  el("storage-clear")?.addEventListener("click", () => {
    if (
      !window.confirm(
        "未転写の録音をすべて削除しますか? 音声ファイルも消えます。この操作は取り消せません。",
      )
    ) {
      return;
    }
    void invoke("delete_untranscribed")
      .then(() => loadStorageStats())
      .catch((e) => showHistoryError(`削除に失敗しました: ${e}`));
  });
  el("history-clear")?.addEventListener("click", () => {
    if (!window.confirm("履歴をすべて削除しますか? この操作は取り消せません。")) {
      return;
    }
    void invoke("clear_history").catch((e) => {
      showHistoryError(`削除に失敗しました: ${e}`);
    });
  });

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
  await listen<{ message: string; origin: string }>("nox://error", (event) =>
    showError(event.payload.message),
  );
  await listen("nox://history", () => {
    void loadHistory();
    void loadStorageStats();
  });

  await listen<HotkeyCaptured | null>("nox://hotkey-captured", (event) => {
    setHotkeyCapturing(false);
    const label = el("hotkey-label");
    if (event.payload && label) {
      label.textContent = event.payload.label;
    } else {
      // 取り消し・失敗。現在値へ戻す。
      void invoke<ConfigView>("get_config").then(renderConfig);
    }
  });

  // トレイ・通知からの「履歴を開く」。設定を畳んで履歴まで運ぶ。
  await listen<{
    downloaded: number;
    total: number | null;
    done: boolean;
    failed?: boolean;
  }>("nox://model-progress", (event) => {
      const text = el("local-stt-text");
      const button = el<HTMLButtonElement>("local-stt-download");
      const { downloaded, total, done } = event.payload;
      if (done) {
        if (button) {
          button.disabled = false;
          button.textContent = "モデルをダウンロード";
        }
        void loadLocalSttStatus();
        return;
      }
      if (text) {
        text.textContent = total
          ? `ダウンロード中… ${formatBytes(downloaded)} / ${formatBytes(total)}` +
            `(${Math.round((downloaded / total) * 100)}%)`
          : `ダウンロード中… ${formatBytes(downloaded)}`;
      }
  });

  await listen("nox://show-history", () => {
    const settings = document.querySelector<HTMLDetailsElement>("details.settings");
    if (settings) settings.open = false;
    el("history-list")?.scrollIntoView({ behavior: "smooth", block: "start" });
    void loadHistory();
  });

  // 初期表示は Rust 側の現在値に合わせる (イベントを取り逃していても正しく出る)。
  try {
    renderStatus(await invoke<Status>("get_status"));
    const session = await invoke<SessionSummary | null>("get_last_session");
    if (session) renderSession(session);
    const result = await invoke<ResultPayload | null>("get_last_result");
    if (result) renderResult(result);
    renderConfig(await invoke<ConfigView>("get_config"));
    await loadHistory();
    await loadStorageStats();
    await loadLocalSttStatus();
  } catch (e) {
    showError(`状態の取得に失敗しました: ${e}`);
  }
});
