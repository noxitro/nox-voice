import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** vite が package.json の版を埋め込む (vite.config.ts の define)。 */
declare const __APP_VERSION__: string;

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
  | "clipboard_only"
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
  /**
   * 適用された文体プロファイル。`null` は「文体の選択を通っていない経路」
   * (画面質問モード)、空文字は「どれにも当たらなかった」。
   *
   * 今の画面では使っていないが、**Rust 側が送っている以上ここに書く**。
   * 型が実体より狭いと、次に触る人が「送られていない」と読み違える。
   */
  style_profile: string | null;
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
  /**
   * 適用された文体プロファイル。
   *
   * `null` は「記録していない」(この列より前の行 / 対象外の経路)、
   * 空文字は「どれにも当たらなかった」。**同じ表示にしない** —
   * 未記録を「未適用」と読ませると、拡充の効果を測り違える。
   */
  style_profile?: string | null;
}

/**
 * Rust 側 `style::StyleProfile` と対応。
 *
 * `id` / `user_edited` は**表示と往復のためだけに持つ**。意味づけ
 * (編集の印を立てる / 削除を覚える) は Rust 側の `Config::apply` が行う。
 * フロントで印を作ると、リロードや実装の取り違えで消えた瞬間に
 * ユーザーの編集がアプリ更新で上書きされる。
 */
interface StyleProfile {
  process: string;
  title_contains: string | null;
  instruction: string;
  /** 同梱既定の安定 id。ユーザーが作ったものは空 (旧設定では未定義)。 */
  id?: string;
  /** 既定由来だがユーザーが書き換えたか。 */
  user_edited?: boolean;
}

/** 提案から作る行の初期指示。空のまま保存すると Rust 側で落とされる。 */
const SUGGESTED_INSTRUCTION = "言いよどみと言い直しを取り除き、読みやすく整える";

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
  /** ホットキーの修飾キー VK 一覧。空なら単独キー。 */
  hotkey_mods: number[];
  hotkey_label: string;
  /** 録音中の取り消しキーの表示名 (「Esc」など)。設定ファイルからのみ変更できる。 */
  cancel_label: string;
  /** クリップボードのみモードのトリガー (0 = 未設定)。 */
  clipboard_hotkey_vk: number;
  clipboard_hotkey_mods: number[];
  /** 未設定なら空文字。 */
  clipboard_hotkey_label: string;
  /** 画面質問モードが有効か。**キーの割り当てとは別**。 */
  screen_ask_enabled: boolean;
  /** 画面質問モードのトリガー (0 = 未設定)。 */
  screen_ask_hotkey_vk: number;
  screen_ask_hotkey_mods: number[];
  /** 未設定なら空文字。**無効でも割り当て済みのキーは入る**。 */
  screen_ask_hotkey_label: string;
  sound_enabled: boolean;
  sound_volume: number;
  start_sound: string;
  start_sound_path: string;
  cancel_sound: string;
  cancel_sound_path: string;
  overlay_enabled: boolean;
  history_enabled: boolean;
  history_retention_days: number;
  restore_delay_ms: number;
  keep_transcript_in_clipboard: boolean;
  /** 節約時間の見積もりに使う打鍵速度 (文字/分)。 */
  typing_speed_chars_per_min: number;
  stt_model: string;
  format_model: string;
}

/**
 * 注入結果の説明。`injected` は「Ctrl+V を送出した」という意味で、
 * 相手アプリに貼られた保証ではない (Rust 側 inject.rs のモジュール doc)。
 */
const INJECT_LABEL: Record<InjectOutcome, string> = {
  injected: "貼り付けを送出しました",
  clipboard_only: "クリップボードにコピーしました (Ctrl+V で貼り付け)",
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

/* --- 区画の切り替え -------------------------------------------------------
 *
 * **区画は DOM から消さない。`hidden` で見せ隠しするだけ。**
 *
 * E2E ハーネス (`e2e/hotkey.mjs`) は WebView2 の CDP から
 * `document.getElementById("hotkey-capture")` を直接 `.click()` し、
 * `#hotkey-label` の `textContent` を読む。さらに「この page が設定画面か」の
 * 判定自体を `#hotkey-capture` の存在で行っている。区画ごとに DOM を作り直す
 * 実装にすると、ホットキー区画を開いていない間はハーネスが page を見つけられず、
 * 接続段階で 30 秒待って落ちる。`hidden` なら要素は常に居るし、
 * `HTMLElement.click()` は非表示要素にも効く (合成イベントなので視認性を要求しない)。
 *
 * 副次的な利点として、区画を切り替えても入力途中の値・展開中の履歴行・
 * 捕獲中のホットキー表示が保たれる。 */
const SECTIONS = [
  "home",
  "history",
  "transcribe",
  "dictionary",
  "paste",
  "hotkeys",
  "sound",
  "app",
] as const;
type SectionId = (typeof SECTIONS)[number];

/** 保存ボタンを出す区画 (設定値の入力欄を持つ区画)。
 *  ホットキーは押した瞬間に保存されるので、保存バーを出すと嘘になる。 */
const SECTIONS_WITH_FORM: ReadonlySet<string> = new Set([
  "history",
  "transcribe",
  "dictionary",
  "paste",
  "sound",
  "app",
]);

const SECTION_STORAGE_KEY = "nox.section";

function isSectionId(value: string | null | undefined): value is SectionId {
  return SECTIONS.includes((value ?? "") as SectionId);
}

/**
 * 区画を切り替える。
 *
 * `focusHeading` は利用者の操作で切り替えたときだけ true にする。起動時や
 * イベント経由の切り替えでフォーカスを動かすと、入力中のフォーカスを奪う。
 */
function showSection(section: SectionId, focusHeading = false) {
  // 捕獲中に別区画へ移ると、Rust 側は捕獲を続けているのに「キーを押して
  // ください…」が見えなくなる。そのまま他アプリで打ったキーが
  // ホットキーとして保存されうるので、窓から離れたときと同じ扱いで畳む。
  // (別区画で保存すると renderConfig が捕獲中ラベルを上書きする問題も消える)
  if (capturingMode && section !== "hotkeys") void cancelHotkeyCapture();

  for (const node of document.querySelectorAll<HTMLElement>(".section")) {
    node.hidden = node.dataset.section !== section;
  }
  for (const button of document.querySelectorAll<HTMLButtonElement>(".nav-item")) {
    const active = button.dataset.section === section;
    if (active) {
      button.setAttribute("aria-current", "page");
    } else {
      button.removeAttribute("aria-current");
    }
  }

  const savebar = el("savebar");
  if (savebar) savebar.hidden = !SECTIONS_WITH_FORM.has(section);

  // 切り替えたら内容の先頭から読ませる。前の区画のスクロール位置が
  // 残っていると、開いた瞬間に見出しの無い途中が出る。
  el("content")?.scrollTo({ top: 0 });

  if (focusHeading) {
    // 見出しへフォーカスを移す。スクリーンリーダーに「どこへ来たか」を
    // 伝える手段が、区画切り替えでは他に無い。
    el(`title-${section}`)?.focus();
  }

  try {
    localStorage.setItem(SECTION_STORAGE_KEY, section);
  } catch {
    /* プライベートモード等で書けなくても、切り替え自体は成立させる */
  }
}

/** レールの初期化。矢印キーでも移動できるようにする。 */
function setupNav() {
  const buttons = [...document.querySelectorAll<HTMLButtonElement>(".nav-item")];
  for (const [index, button] of buttons.entries()) {
    button.addEventListener("click", () => {
      const target = button.dataset.section;
      if (isSectionId(target)) showSection(target, true);
    });
    button.addEventListener("keydown", (event) => {
      const step =
        event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
      if (step === 0) return;
      event.preventDefault();
      const next = buttons[(index + step + buttons.length) % buttons.length];
      next.focus();
    });
  }

  let initial: SectionId = "home";
  try {
    const saved = localStorage.getItem(SECTION_STORAGE_KEY);
    if (isSectionId(saved)) initial = saved;
  } catch {
    /* 読めなければホームから始める */
  }
  showSection(initial);
}

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
    // クリップボードのみモードは、ラベル自体が終状態を語っている。
    // ここで CLIPBOARD_LABEL を足すと同じことを 2 回言うことになる。
    const copyOnly = r.inject_outcome === "clipboard_only";
    const clipboard = copyOnly ? "" : CLIPBOARD_LABEL[r.clipboard_state];
    injectNote.textContent = clipboard ? `${label} / ${clipboard}` : label;
    // 手を動かす必要がある状態なら目立たせる。**指定どおりコピーしただけ**の
    // ときは警告にしない (毎回警告色だと、本当の失敗が埋もれる)。
    const needsAction =
      (!copyOnly && r.clipboard_state === "holds_injected_text") ||
      r.clipboard_state === "lost" ||
      r.lost_clipboard_formats.length > 0;
    injectNote.dataset.kind =
      needsAction ? "warn" : r.injected || copyOnly ? "ok" : "warn";
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

/**
 * 復元ディレイ欄を触れなくする理由 (無ければ `null` = 有効のまま)。
 *
 * 判定を純関数にしておく。「クリップボードに残す」を選んでいる間、
 * 復元そのものが行われないので待ち時間には意味が無い。値は消さない —
 * 設定を戻したときに前の値が復活してほしい。
 */
function restoreDelayDisabledReason(keepTranscript: boolean): string | null {
  return keepTranscript
    ? "「録音結果をクリップボードに残す」がオンの間は復元しないため使われません"
    : null;
}

/** チェック状態を見て、復元ディレイ欄の有効・無効を合わせる。 */
function syncRestoreDelayEnabled() {
  const keep = el<HTMLInputElement>("keep-transcript");
  const delay = el<HTMLInputElement>("restore-delay");
  const note = el<HTMLElement>("restore-delay-note");
  const reason = restoreDelayDisabledReason(keep?.checked ?? true);
  if (delay) delay.disabled = reason !== null;
  if (note) {
    note.textContent = reason ?? "";
    note.hidden = reason === null;
  }
}

/**
 * 画面質問モードの「今この機能は効くのか」を、ホットキー区画に出す。
 *
 * この機能は**有効化のトグル**と**キーの割り当て**の両方が揃わないと動かない。
 * 片方だけの状態は必ず起きる (先にキーを決める / 後で機能を切る) ので、
 * 「設定したのに動かない」を黙って作らないよう、足りない方を名指しする。
 *
 * トグルは「認識と整形」区画にあり保存ボタンで確定するが、キーの割り当ては
 * 捕獲 UI が即時保存する。**チェックを入れただけの未保存状態**でここを
 * 「有効」と書くと嘘になるので、文言は「保存すると効きます」に寄せる。
 */
function syncScreenAskEnabled() {
  const enabled = el<HTMLInputElement>("screen-ask-enabled")?.checked ?? false;
  const bound = (el("screen-ask-hotkey-label")?.textContent ?? "未設定") !== "未設定";
  const block = el<HTMLElement>("screen-ask-hotkey-state");
  const text = el<HTMLElement>("screen-ask-hotkey-state-text");
  if (!block || !text) return;

  let message: string | null = null;
  if (!enabled && !bound) {
    message =
      "この機能はまだ動きません。「認識と整形」でオンにし、ここでキーを割り当ててください。";
  } else if (!enabled) {
    message =
      "キーは割り当て済みですが、機能がオフです。「認識と整形」の画面質問モードをオンにして保存すると効きます。";
  } else if (!bound) {
    message = "機能はオンですが、キーが未設定です。キーを割り当てるまで発火しません。";
  }
  text.textContent = message ?? "";
  block.hidden = message === null;
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
  const keepTranscript = el<HTMLInputElement>("keep-transcript");
  if (keepTranscript) keepTranscript.checked = view.keep_transcript_in_clipboard;
  syncRestoreDelayEnabled();
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
  const typingSpeed = el<HTMLInputElement>("typing-speed");
  if (typingSpeed) typingSpeed.value = String(view.typing_speed_chars_per_min);
  const hotkeyLabel = el("hotkey-label");
  if (hotkeyLabel) {
    hotkeyLabel.textContent = view.hotkey_label;
    delete hotkeyLabel.dataset.capturing;
  }
  const clipboardLabel = el("clipboard-hotkey-label");
  if (clipboardLabel) {
    clipboardLabel.textContent = view.clipboard_hotkey_label || "未設定";
    delete clipboardLabel.dataset.capturing;
  }
  const cancelKeyLabel = el("cancel-key-label");
  if (cancelKeyLabel) cancelKeyLabel.textContent = view.cancel_label || "なし";

  // ホームの案内は実際の割り当てを写す。固定文にしておくと、キーを変えた
  // 利用者に嘘を教え続けることになる (旧 UI は「右 Ctrl」と書いたままだった)。
  const homeHotkey = el("home-hotkey");
  if (homeHotkey) homeHotkey.textContent = view.hotkey_label;
  const homeCancel = el("home-cancel");
  if (homeCancel) homeCancel.textContent = view.cancel_label || "(未設定)";
  const clipboardHint = el("home-clipboard-hint");
  const homeClipboardHotkey = el("home-clipboard-hotkey");
  if (homeClipboardHotkey) {
    homeClipboardHotkey.textContent = view.clipboard_hotkey_label;
  }
  // 未設定のときは行ごと出さない。「未設定 で録音すると」は読めない文になる。
  if (clipboardHint) clipboardHint.hidden = view.clipboard_hotkey_vk === 0;
  const clipboardClear = el<HTMLButtonElement>("clipboard-hotkey-clear");
  // 未設定のときに「解除」を押せても何も起きない。押せない方が状態が伝わる。
  if (clipboardClear) clipboardClear.disabled = view.clipboard_hotkey_vk === 0;
  const screenAskEnabled = el<HTMLInputElement>("screen-ask-enabled");
  if (screenAskEnabled) screenAskEnabled.checked = view.screen_ask_enabled;
  const screenAskLabel = el("screen-ask-hotkey-label");
  if (screenAskLabel) {
    screenAskLabel.textContent = view.screen_ask_hotkey_label || "未設定";
    delete screenAskLabel.dataset.capturing;
  }
  const screenAskClear = el<HTMLButtonElement>("screen-ask-hotkey-clear");
  if (screenAskClear) screenAskClear.disabled = view.screen_ask_hotkey_vk === 0;
  syncScreenAskEnabled();
  const soundEnabled = el<HTMLInputElement>("sound-enabled");
  if (soundEnabled) soundEnabled.checked = view.sound_enabled;
  const soundVolume = el<HTMLInputElement>("sound-volume");
  if (soundVolume) soundVolume.value = String(view.sound_volume);
  syncSoundVolumeLabel();
  const startSound = el<HTMLSelectElement>("start-sound");
  if (startSound) startSound.value = view.start_sound;
  const cancelSound = el<HTMLSelectElement>("cancel-sound");
  if (cancelSound) cancelSound.value = view.cancel_sound;
  const startPath = el<HTMLInputElement>("start-sound-path");
  if (startPath) startPath.value = view.start_sound_path;
  const cancelPath = el<HTMLInputElement>("cancel-sound-path");
  if (cancelPath) cancelPath.value = view.cancel_sound_path;
  syncSoundCustomVisible();
  syncSoundControlsEnabled();
  renderStyleProfiles(view.style_profiles);
  const groqState = el("groq-state");
  if (groqState) groqState.textContent = keyStateLabel(view, "groq");
  const geminiState = el("gemini-state");
  if (geminiState) geminiState.textContent = keyStateLabel(view, "gemini");
}

/* --- アプリ別の文体: 行単位の編集 -----------------------------------------
 *
 * 旧 UI はテキストエリア 1 枚で、形式が違う行は黙って捨てられていた。
 * 既定が 30 件近くになると、それを手で書き直すのは無理がある。
 *
 * **DOM を唯一の状態にする。** 別に配列を持って同期させると、
 * 「画面には出ているが保存されない行」がいつか必ず生まれる。 */

/** 由来のバッジ。既定を触ったかどうかが一目で分かるようにする。 */
function styleOrigin(profile: StyleProfile): { kind: string; label: string } {
  if (!profile.id) return { kind: "mine", label: "自分で追加" };
  if (profile.user_edited) return { kind: "edited", label: "既定 (編集済み)" };
  return { kind: "bundled", label: "既定" };
}

/** 1 行分の DOM を作る。 */
function styleRow(profile: StyleProfile): HTMLElement {
  const row = document.createElement("div");
  row.className = "style-row";
  // id と印は入力欄に出さない。ユーザーが触るものではないので、
  // 往復のためだけに dataset へ預ける。
  row.dataset.id = profile.id ?? "";
  row.dataset.userEdited = profile.user_edited ? "true" : "false";

  const head = document.createElement("div");
  head.className = "style-row-head";
  const origin = styleOrigin(profile);
  const badge = document.createElement("span");
  badge.className = "badge";
  badge.dataset.kind = origin.kind;
  badge.textContent = origin.label;
  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "danger style-remove";
  remove.textContent = "削除";
  // 何を消すのかを読み上げにも伝える。行が 30 個あると「削除」だけでは足りない。
  remove.setAttribute("aria-label", `${profile.process || "この行"} の文体を削除`);
  remove.addEventListener("click", () => {
    row.remove();
    syncStyleEmpty();
  });
  head.append(badge, remove);

  const conds = document.createElement("div");
  conds.className = "style-row-conds";
  const process = labelledInput("プロセス名", "style-process", profile.process, "slack.exe");
  const title = labelledInput(
    "タイトル条件 (任意)",
    "style-title",
    profile.title_contains ?? "",
    "Gmail",
  );
  conds.append(process.field, title.field);

  const instruction = document.createElement("label");
  instruction.className = "field";
  const instructionLabel = document.createElement("span");
  instructionLabel.textContent = "指示";
  const area = document.createElement("textarea");
  area.className = "style-instruction";
  area.rows = 2;
  area.value = profile.instruction;
  area.placeholder = "チャットの発言。簡潔な口語にする";
  instruction.append(instructionLabel, area);

  row.append(head, conds, instruction);
  return row;
}

function labelledInput(
  caption: string,
  className: string,
  value: string,
  placeholder: string,
): { field: HTMLLabelElement; input: HTMLInputElement } {
  const field = document.createElement("label");
  field.className = "field";
  const span = document.createElement("span");
  span.textContent = caption;
  const input = document.createElement("input");
  input.type = "text";
  input.className = className;
  input.value = value;
  input.placeholder = placeholder;
  input.autocomplete = "off";
  field.append(span, input);
  return { field, input };
}

/** 「1 件もありません」の出し分け。空の一覧を無言にしない。 */
function syncStyleEmpty() {
  const list = el("style-list");
  const empty = el("style-empty");
  if (!list || !empty) return;
  empty.hidden = list.querySelectorAll(".style-row").length > 0;
}

function renderStyleProfiles(profiles: StyleProfile[]) {
  const list = el("style-list");
  if (!list) return;
  list.replaceChildren(...profiles.map(styleRow));
  syncStyleEmpty();
}

/** 画面の行を読み取る。**空欄の行も含めて返す** (何行落ちたかを数えるため)。 */
function collectStyleProfiles(): StyleProfile[] {
  const list = el("style-list");
  if (!list) return [];
  return [...list.querySelectorAll<HTMLElement>(".style-row")].map((row) => {
    const value = (selector: string) =>
      row.querySelector<HTMLInputElement | HTMLTextAreaElement>(selector)?.value.trim() ?? "";
    const title = value(".style-title");
    return {
      process: value(".style-process"),
      title_contains: title === "" ? null : title,
      instruction: value(".style-instruction"),
      id: row.dataset.id ?? "",
      user_edited: row.dataset.userEdited === "true",
    };
  });
}

/** 行を足して、その場で編集できるようにする。 */
function addStyleRow(seed: Partial<StyleProfile> = {}) {
  const list = el("style-list");
  if (!list) return;
  const row = styleRow({
    process: seed.process ?? "",
    title_contains: seed.title_contains ?? null,
    instruction: seed.instruction ?? "",
    id: "",
    user_edited: false,
  });
  list.append(row);
  syncStyleEmpty();
  // 追加した行が画面外だと「押しても何も起きない」ように見える。
  row.scrollIntoView({ block: "nearest" });
  row.querySelector<HTMLInputElement>(seed.process ? ".style-instruction" : ".style-process")
    ?.focus();
}

/** Rust 側 `lib::StyleSuggestions` と対応。 */
interface StyleSuggestions {
  history_enabled: boolean;
  total_sessions: number;
  items: { process: string; sessions: number }[];
}

/**
 * 「よく使っているのに文体の指定が無いアプリ」を出す。
 *
 * **0 件と欠測を言い分ける。** 履歴オフ / 履歴が空 / 全部設定済み /
 * 取得失敗 はすべて別のことで、同じ「提案なし」に丸めると
 * 機能が壊れているようにしか見えない。
 */
async function loadStyleSuggestions() {
  const note = el("style-suggest-note");
  const list = el("style-suggest-list");
  if (!note || !list) return;
  list.replaceChildren();
  try {
    const data = await invoke<StyleSuggestions>("get_style_suggestions");
    if (!data.history_enabled) {
      note.textContent = "履歴が無効なので集計できません。提案には履歴が要ります";
      return;
    }
    if (data.total_sessions === 0) {
      note.textContent = "まだ履歴がありません。使っていくと、ここに候補が出ます";
      return;
    }
    if (data.items.length === 0) {
      note.textContent = `よく使うアプリはすべて設定済みです (履歴 ${data.total_sessions.toLocaleString()} 件から集計)`;
      return;
    }
    note.textContent = `履歴 ${data.total_sessions.toLocaleString()} 件のうち、文体の指定が無い挿入先です`;
    list.replaceChildren(
      ...data.items.map((item) => {
        const li = document.createElement("li");
        li.className = "suggest-item";
        const name = document.createElement("span");
        name.textContent = item.process;
        const count = document.createElement("span");
        count.className = "suggest-count";
        count.textContent = `${item.sessions.toLocaleString()} 回`;
        const add = document.createElement("button");
        add.type = "button";
        add.className = "suggest-add";
        add.textContent = "この行を作る";
        add.setAttribute("aria-label", `${item.process} の文体を追加`);
        add.addEventListener("click", () => {
          addStyleRow({ process: item.process, instruction: SUGGESTED_INSTRUCTION });
          // 作った候補は消す。押したのに残っていると、二重に足してしまう。
          li.remove();
        });
        li.append(name, count, add);
        return li;
      }),
    );
  } catch (e) {
    note.textContent = `提案を取得できません (${e})`;
  }
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
  const screenAsk = el<HTMLInputElement>("screen-ask-enabled");
  const overlayEnabled = el<HTMLInputElement>("overlay-enabled");
  const startHidden = el<HTMLInputElement>("start-hidden");
  const localMode = el<HTMLSelectElement>("local-stt-mode");
  const keepTranscript = el<HTMLInputElement>("keep-transcript");
  const soundEnabled = el<HTMLInputElement>("sound-enabled");
  const soundVolume = el<HTMLInputElement>("sound-volume");
  const startSound = el<HTMLSelectElement>("start-sound");
  const cancelSound = el<HTMLSelectElement>("cancel-sound");
  const startSoundPath = el<HTMLInputElement>("start-sound-path");
  const cancelSoundPath = el<HTMLInputElement>("cancel-sound-path");
  const typingSpeed = el<HTMLInputElement>("typing-speed");

  const styleRows = collectStyleProfiles();

  // 入力欄が空 = 「変更しない」。誤って既存キーを消さないため未指定で送る。
  const patch: Record<string, unknown> = {
    language: language?.value ?? "",
    formatting_enabled: formatting?.checked ?? true,
    injection_enabled: injection?.checked ?? true,
    history_enabled: historyEnabled?.checked ?? true,
    deep_context: deepContext?.checked ?? false,
    // 有効化のトグルは設定フォームにあるが、キーの割り当ては捕獲 UI が
    // 直接保存する。どちらか片方だけでは動かない (Rust 側 `Config` の doc)。
    screen_ask_enabled: screenAsk?.checked ?? false,
    overlay_enabled: overlayEnabled?.checked ?? true,
    start_hidden: startHidden?.checked ?? true,
    keep_transcript_in_clipboard: keepTranscript?.checked ?? true,
    local_stt_mode: localMode?.value ?? "fallback",
    // 空行は Rust 側で落とされる。
    dictionary: (dictionary?.value ?? "").split(/\r?\n/),
    // 入力が足りない行は Rust 側でも落とされるが、**何行落としたかを
    // 数えたい**ので、ここでも同じ条件で分けておく。
    style_profiles: styleRows.filter((p) => p.process !== "" && p.instruction !== ""),
  };
  // 音のプリセット一覧が取れなかったとき、select は空のまま = value は空文字。
  // これを送ると serde が `SoundPreset` として弾き、**set_config が丸ごと失敗して
  // 音と無関係な項目まで保存できなくなる**。取れていないものは送らない
  // (欠測を既定値で埋めない。埋めると、利用者の選択が黙って書き換わる)。
  const soundPresetsLoaded =
    (startSound?.options.length ?? 0) > 0 && (cancelSound?.options.length ?? 0) > 0;
  patch.sound_enabled = soundEnabled?.checked ?? true;
  patch.sound_volume = Number(soundVolume?.value ?? 60);
  if (soundPresetsLoaded) {
    patch.start_sound = startSound?.value;
    patch.cancel_sound = cancelSound?.value;
    patch.start_sound_path = startSoundPath?.value ?? "";
    patch.cancel_sound_path = cancelSoundPath?.value ?? "";
  }

  const days = Number(retention?.value);
  if (Number.isFinite(days) && days >= 0) patch.history_retention_days = days;
  // 0 を送ると節約時間がゼロ除算になる。読めない値は送らず Rust 側の現在値に任せる。
  const speed = Number(typingSpeed?.value);
  if (Number.isFinite(speed) && speed > 0) {
    patch.typing_speed_chars_per_min = speed;
  }

  // 書きかけの行 (プロセス名か指示が空) は保存されない。黙って消さずに伝える。
  const ignoredStyleLines = styleRows.length - (patch.style_profiles as StyleProfile[]).length;
  // 数値として読めないときは送らない (Rust 側の範囲でクランプされる)。
  const delay = Number(restoreDelay?.value);
  if (Number.isFinite(delay) && delay > 0) patch.restore_delay_ms = delay;
  if (groq?.value) patch.groq_api_key = groq.value;
  if (gemini?.value) patch.gemini_api_key = gemini.value;

  try {
    const view = await invoke<ConfigView>("set_config", { patch });
    renderConfig(view);
    // 打鍵速度を変えると節約時間の見積もりが変わる。数字を古いままにしない。
    void loadDashboardStats();
    // 文体を足した / 消した分だけ「覆われていないアプリ」が変わる。
    void loadStyleSuggestions();
    // 入力欄には残さない (画面に平文で残る時間を最小にする)。
    if (groq) groq.value = "";
    if (gemini) gemini.value = "";
    if (note) {
      // 送らなかったものは黙って落とさず、その場で言う
      // (「保存しました」とだけ出して音だけ変わっていない、が一番たちが悪い)。
      const skipped: string[] = [];
      if (ignoredStyleLines > 0) {
        skipped.push(
          `アプリ別の文体 ${ignoredStyleLines} 行は、プロセス名か指示が空のため保存していません`,
        );
      }
      if (!soundPresetsLoaded) {
        skipped.push("音の一覧を取得できていないため、開始音・取り消し音は変更していません");
      }
      note.textContent =
        skipped.length > 0 ? `保存しました(${skipped.join(" / ")})` : "保存しました";
    }
  } catch (e) {
    if (note) note.textContent = `保存に失敗しました: ${e}`;
  }
  // 読むべき但し書きがあるときは、読む時間を長めに取る。
  window.setTimeout(
    () => {
      if (note) note.textContent = "";
    },
    ignoredStyleLines > 0 || !soundPresetsLoaded ? 8000 : 2500,
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
    // 「クリップボードのみ」は指定どおりの結末なので警告色にしない。
    inject.dataset.kind =
      row.inject_outcome === "injected" || row.inject_outcome === "clipboard_only"
        ? "ok"
        : "warn";
    detail.append(inject);
  }

  // どの文体が効いたか。控えめに 1 行だけ (この行の主役は本文なので)。
  if (row.style_profile !== null && row.style_profile !== undefined) {
    const style = document.createElement("p");
    // `.warn` は 2 列グリッド (アイコン + 本文) なので使わない。
    style.className = "field-note";
    style.textContent =
      row.style_profile === ""
        ? "文体: 指定なし (当てはまるプロファイルがありませんでした)"
        : `文体: ${row.style_profile}`;
    detail.append(style);
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

/** Rust 側 `history::DashboardStats` と対応 (`daily_stats` は今の UI では使わない)。 */
interface DashboardStats {
  total_chars: number;
  total_sessions: number;
  total_recording_time_ms: number;
  /** 節約時間。打鍵より遅ければ負になりうるので、そのまま出す。 */
  time_saved_ms: number;
}

/** ミリ秒を「1 時間 23 分」の形にする。負なら符号を前に出す。 */
function formatDuration(ms: number): string {
  const sign = ms < 0 ? "-" : "";
  const total = Math.round(Math.abs(ms) / 1000);
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  if (hours > 0) return `${sign}${hours} 時間 ${minutes} 分`;
  if (minutes > 0) return `${sign}${minutes} 分 ${seconds} 秒`;
  return `${sign}${seconds} 秒`;
}

/**
 * 累計を出す。
 *
 * 取得に失敗したときは 0 を並べない。「まだ使っていない」と
 * 「集計できなかった」は別のことで、後者を前者に見せると
 * 履歴が消えたのかどうか分からなくなる (製品原則: 0 件と欠測を混同しない)。
 */
async function loadDashboardStats() {
  const list = el("stats");
  if (!list) return;
  const put = (rows: [string, string][]) => {
    list.replaceChildren(
      ...rows.flatMap(([key, value]) => {
        const dt = document.createElement("dt");
        dt.textContent = key;
        const dd = document.createElement("dd");
        dd.textContent = value;
        return [dt, dd];
      }),
    );
  };
  try {
    const stats = await invoke<DashboardStats>("get_dashboard_stats");
    put([
      ["節約時間", formatDuration(stats.time_saved_ms)],
      ["文字数", `${stats.total_chars.toLocaleString()} 文字`],
      ["回数", `${stats.total_sessions.toLocaleString()} 回`],
      ["録音時間", formatDuration(stats.total_recording_time_ms)],
    ]);
  } catch (e) {
    put([["集計", `取得できません (${e})`]]);
  }
}

/** 入力レベルのメーター (0.0..=1.0)。オーバーレイと同じイベントを見る。 */
function renderLevel(level: number) {
  const fill = el("level-fill");
  if (!fill) return;
  const clamped = Math.max(0, Math.min(1, level));
  fill.style.transform = `scaleX(${clamped.toFixed(3)})`;
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

/** ホットキー捕獲のイベント。`null` は取り消し・タイムアウト・失敗。 */
interface HotkeyCaptured {
  /** 組み合わせの表示名 (「左 Ctrl + Space」など)。 */
  label: string;
  /** 押している最中の経過表示。確定ではない。 */
  capturing?: boolean;
}

/** ホットキーの用途。Rust 側 `hotkey::HotkeyMode` と対応。 */
type HotkeyModeId = "inject" | "clipboard_only" | "screen_ask";

/** 用途ごとの DOM 要素 id。捕獲 UI はこの表だけを見て動く。 */
const HOTKEY_ELEMENTS: Record<HotkeyModeId, { label: string; button: string }> = {
  inject: { label: "hotkey-label", button: "hotkey-capture" },
  clipboard_only: {
    label: "clipboard-hotkey-label",
    button: "clipboard-hotkey-capture",
  },
  screen_ask: {
    label: "screen-ask-hotkey-label",
    button: "screen-ask-hotkey-capture",
  },
};

/** 捕獲中の用途 (`null` なら捕獲していない)。 */
let capturingMode: HotkeyModeId | null = null;
/** 残り秒のカウントダウン。 */
let captureCountdown: number | undefined;

function setHotkeyCapturing(mode: HotkeyModeId, active: boolean, seconds = 0) {
  capturingMode = active ? mode : null;
  window.clearInterval(captureCountdown);

  const ids = HOTKEY_ELEMENTS[mode];
  const label = el(ids.label);
  const button = el<HTMLButtonElement>(ids.button);
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

/** 捕獲中にキーが押されるたび、Rust 側から組み合わせの経過が流れてくる。 */
function showHotkeyCaptureProgress(labelText: string) {
  // カウントダウンの上書きを止めて、押している形を見せる。
  window.clearInterval(captureCountdown);
  const label = el(HOTKEY_ELEMENTS[capturingMode ?? "inject"].label);
  if (label) {
    label.dataset.capturing = "true";
    label.textContent = `${labelText} (離すと確定)`;
  }
}

async function cancelHotkeyCapture() {
  setHotkeyCapturing(capturingMode ?? "inject", false);
  await invoke("cancel_hotkey_capture");
  renderConfig(await invoke<ConfigView>("get_config"));
}

async function toggleHotkeyCapture(mode: HotkeyModeId) {
  // 捕獲は排他 (Rust 側も 1 セッションしか持たない)。別の用途のボタンを
  // 押したときは、いま進行中の捕獲を畳んでから始める。
  if (capturingMode) {
    const previous = capturingMode;
    await cancelHotkeyCapture();
    if (previous === mode) return;
  }
  // 捕獲は「押してから離すまで」を見せる操作なので、別の区画から
  // 呼ばれた場合 (トレイ導線・E2E の直接 click) でも表示を合わせる。
  showSection("hotkeys");
  try {
    const seconds = await invoke<number>("start_hotkey_capture", { mode });
    setHotkeyCapturing(mode, true, seconds);
    // ボタンがキーボードフォーカスを持ったままだと、組み合わせの一部として
    // 押した Space の離しでボタンが再度発火し、捕獲が即キャンセルされる。
    // 捕獲中はフォーカスを外して、キー入力をフック側に集中させる。
    if (document.activeElement instanceof HTMLElement) {
      document.activeElement.blur();
    }
  } catch (e) {
    setHotkeyCapturing(mode, false);
    showError(`${e}`);
  }
}

/** 用途に割り当てたホットキーを解除する (録音用は Rust 側が拒否する)。 */
async function clearHotkey(mode: HotkeyModeId) {
  try {
    renderConfig(await invoke<ConfigView>("clear_hotkey", { mode }));
  } catch (e) {
    showError(`ホットキーを解除できません: ${e}`);
  }
}

/** 音量つまみの数値表示を合わせる。 */
function syncSoundVolumeLabel() {
  const slider = el<HTMLInputElement>("sound-volume");
  const out = el("sound-volume-value");
  if (slider && out) out.textContent = slider.value;
}

/**
 * 通知音を切ったら、音の設定は触れなくする。
 *
 * 復元ディレイ欄と同じ扱い ([`syncRestoreDelayEnabled`])。効かない設定を
 * 触れるままにしておくと、いじった結果が出ないことの理由が分からない。
 * 値は消さない — 戻したときに前の選択が復活してほしい。
 */
function syncSoundControlsEnabled() {
  const enabled = el<HTMLInputElement>("sound-enabled")?.checked ?? true;
  const ids = [
    "sound-volume",
    "start-sound",
    "cancel-sound",
    "start-sound-path",
    "cancel-sound-path",
    "start-sound-preview",
    "cancel-sound-preview",
  ];
  for (const id of ids) {
    const control = el<
      HTMLInputElement | HTMLSelectElement | HTMLButtonElement
    >(id);
    if (control) control.disabled = !enabled;
  }
}

/** 「ファイルを指定」を選んだときだけパス欄を出す。 */
function syncSoundCustomVisible() {
  for (const kind of ["start", "cancel"] as const) {
    const select = el<HTMLSelectElement>(`${kind}-sound`);
    const row = el(`${kind}-sound-custom`);
    if (select && row) row.hidden = select.value !== "custom";
  }
}

/** プリセットの選択肢を Rust 側の定義から作る。 */
async function loadSoundPresets() {
  try {
    const presets = await invoke<{ id: string; label: string }[]>(
      "list_sound_presets",
    );
    for (const id of ["start-sound", "cancel-sound"]) {
      const select = el<HTMLSelectElement>(id);
      if (!select) continue;
      select.replaceChildren(
        ...presets.map((p) => {
          const option = document.createElement("option");
          option.value = p.id;
          option.textContent = p.label;
          return option;
        }),
      );
    }
  } catch (e) {
    showError(`通知音の一覧を取得できません: ${e}`);
  }
}

/** 保存前の値で試聴する。 */
async function previewSound(kind: "start" | "cancel") {
  const preset = el<HTMLSelectElement>(`${kind}-sound`)?.value ?? "soft_pop";
  const path = el<HTMLInputElement>(`${kind}-sound-path`)?.value ?? "";
  const volume = Number(el<HTMLInputElement>("sound-volume")?.value ?? 60);
  try {
    await invoke("preview_sound", { preset, path, volume });
  } catch (e) {
    showError(`試聴できません: ${e}`);
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  setupNav();
  const version = el("app-version");
  if (version) version.textContent = `v${__APP_VERSION__}`;

  document.querySelectorAll<HTMLButtonElement>("button.copy").forEach((btn) => {
    btn.addEventListener("click", () => {
      const kind = btn.dataset.copy === "raw" ? "raw" : "formatted";
      void copyText(kind, btn);
    });
  });
  el("settings-form")?.addEventListener("submit", (e) => void saveSettings(e));
  el("keep-transcript")?.addEventListener("change", syncRestoreDelayEnabled);
  el("hotkey-capture")?.addEventListener("click", () => void toggleHotkeyCapture("inject"));
  el("clipboard-hotkey-capture")?.addEventListener(
    "click",
    () => void toggleHotkeyCapture("clipboard_only"),
  );
  el("clipboard-hotkey-clear")?.addEventListener(
    "click",
    () => void clearHotkey("clipboard_only"),
  );
  el("screen-ask-hotkey-capture")?.addEventListener(
    "click",
    () => void toggleHotkeyCapture("screen_ask"),
  );
  el("screen-ask-hotkey-clear")?.addEventListener(
    "click",
    () => void clearHotkey("screen_ask"),
  );
  el("screen-ask-enabled")?.addEventListener("change", syncScreenAskEnabled);
  el("sound-volume")?.addEventListener("input", syncSoundVolumeLabel);
  el("sound-enabled")?.addEventListener("change", syncSoundControlsEnabled);
  el("start-sound")?.addEventListener("change", syncSoundCustomVisible);
  el("cancel-sound")?.addEventListener("change", syncSoundCustomVisible);
  el("start-sound-preview")?.addEventListener("click", () => void previewSound("start"));
  el("cancel-sound-preview")?.addEventListener("click", () => void previewSound("cancel"));
  await loadSoundPresets();

  // ウィンドウから離れたら捕獲をやめる。設定画面を離れたまま
  // 捕獲が続くと、他アプリで打ったキーがホットキーとして保存される。
  window.addEventListener("blur", () => {
    if (capturingMode) void cancelHotkeyCapture();
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden && capturingMode) void cancelHotkeyCapture();
  });
  el("style-add")?.addEventListener("click", () => addStyleRow());
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
    void loadDashboardStats();
  });
  await listen<number>("nox://level", (event) => renderLevel(event.payload));

  await listen<HotkeyCaptured | null>("nox://hotkey-captured", (event) => {
    const payload = event.payload;
    const mode = capturingMode;
    // 捕獲はもう畳まれている (blur・区画切替・タイムアウト)。ここで
    // inject 側だと決めつけると、触っていない方のラベルを残イベントが汚す。
    // どちらのラベルにも触らず、現在値を取り直すだけにする。
    if (!mode) {
      void invoke<ConfigView>("get_config").then(renderConfig);
      return;
    }
    if (!payload) {
      // 取り消し・タイムアウト・保存失敗。現在値を取り直して元に戻す。
      setHotkeyCapturing(mode, false);
      void invoke<ConfigView>("get_config").then(renderConfig);
      return;
    }
    if (payload.capturing) {
      // まだ押している最中。確定は「すべて離した瞬間」。
      showHotkeyCaptureProgress(payload.label);
      return;
    }
    setHotkeyCapturing(mode, false);
    // 確定した値はその場で反映する。設定の取り直しを待たせると、
    // 「押して離したのに表示が古いまま」の瞬間ができる (E2E もここを読む)。
    const label = el(HOTKEY_ELEMENTS[mode].label);
    if (label) label.textContent = payload.label;
    // 解除ボタンの活性など、ラベル以外も追って揃える。
    void invoke<ConfigView>("get_config").then(renderConfig);
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

  // トレイ・通知からの「履歴を開く」。区画ごと履歴へ運ぶ。
  // 見出しへフォーカスは移さない (利用者が別の入力中かもしれない)。
  await listen("nox://show-history", () => {
    showSection("history");
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
    await loadDashboardStats();
    await loadStyleSuggestions();
  } catch (e) {
    showError(`状態の取得に失敗しました: ${e}`);
  }
});
