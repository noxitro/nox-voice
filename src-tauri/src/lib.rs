//! nox-voice — Windows 11 向け AI 音声入力常駐アプリ。
//!
//! M1 の範囲: ホットキー PTT + 音声録音 + トレイ常駐。
//! このファイルは配線のみを担当し、実装は各モジュールにある。
//!
//! ```text
//! [フックスレッド]      低レベルキーボードフック → HotkeyEvent (時刻つき)
//!     ▼ bounded channel
//! [コントローラスレッド] PttInterpreter で解釈 → 録音の開始 / 停止要求
//!     │                 ここは常に軽く保ち、即イベントループへ戻る
//!     ▼ channel (FinalizeJob)
//! [ファイナライズワーカー] リサンプル → WAV 化 → 状態遷移とイベント発火
//!                          M2 以降の STT / 整形 / 注入もここに載る
//! ```
//!
//! # コントローラを軽く保つ理由
//!
//! コントローラがブロックしている間、フックが送るキーイベントはキューに溜まる。
//! イベントには発生時刻が入っているので**判定自体は狂わない**が、
//! 録音の開始 / 停止が体感できるほど遅れる。したがって秒単位になりうる処理
//! (リサンプル、そして M2 の STT 呼び出し) はワーカーへ逃がす。
//!
//! 各モジュール: `foreground.rs` (録音開始時の前景 HWND 確定 / R7 用)、
//! `audio.rs` (cpal 録音 → 16kHz mono WAV)、
//! `session.rs` (`RecordingSession` — M2 の STT への受け渡し)、
//! `tray.rs` (トレイ常駐と状態表示)。

mod audio;
mod config;
mod context;
mod dictionary;
mod foreground;
mod format;
mod history;
mod hotkey;
mod inject;
mod instance;
mod local_stt;
mod overlay;
mod pipeline;
mod screen;
mod session;
mod sound;
mod stt;
mod style;
mod tray;

#[cfg(test)]
mod http_test_server;

use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use serde::Serialize;
use tauri::menu::MenuItem;
use tauri::{AppHandle, Emitter, Manager, RunEvent, Wry};

use audio::Recorder;
use config::{ConfigPatch, ConfigStore, ConfigView};
use format::{GeminiFormatter, ScreenAnswerer};
use history::{DashboardStats, HistoryStore, SessionDraft, SessionRow};
use hotkey::{HookHandle, HotkeyAction, HotkeyMode, PttInterpreter, HOTKEY_SLOTS, TAP_THRESHOLD};
use inject::{ClipboardState, InjectOutcome, InjectTarget};
use pipeline::FormatOutcome;
use session::{
    PendingRecording, RecordingSession, SessionSummary, Status, StatusOrigin, StatusPayload,
    TargetWindow,
};
use stt::GroqStt;

/// 状態変化の通知イベント。
const EVENT_STATUS: &str = "nox://status";
/// 録音完了の通知イベント (WAV のメタ情報)。
const EVENT_SESSION: &str = "nox://session";
/// 転写・整形の結果イベント。
const EVENT_RESULT: &str = "nox://result";
/// ユーザーに見せるべきエラーの通知イベント。
const EVENT_ERROR: &str = "nox://error";
/// 履歴が更新されたことの通知イベント (UI が再読込する)。
const EVENT_HISTORY: &str = "nox://history";
/// 入力レベル (0.0..=1.0)。オーバーレイのメーター用に間引いて送る。
const EVENT_LEVEL: &str = "nox://level";
/// 設定 UI へキー捕獲の結果を返すイベント。
const EVENT_HOTKEY_CAPTURED: &str = "nox://hotkey-captured";
/// 履歴を開くよう UI へ促すイベント (トレイ・通知からの導線)。
const EVENT_SHOW_HISTORY: &str = "nox://show-history";

/// 保持期限を定期執行する間隔。
const RETENTION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// キー捕獲の制限時間。
///
/// 捕獲はフックがグローバルなので、他アプリで打った最初のキーまで拾ってしまう。
/// 押し忘れたまま放置されると、次に触ったキーがホットキーとして保存され、
/// 以後そのキーで録音とトグルが暴発する。必ず時間で畳む。
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 結果を見せてから小窓を畳むまで。
const OVERLAY_RESULT_LINGER: std::time::Duration = std::time::Duration::from_millis(1_600);
/// エラー表示を残す時間 (読む時間が要る)。
const OVERLAY_ERROR_LINGER: std::time::Duration = std::time::Duration::from_secs(5);

/// 画面質問モードの Gemini 呼び出しを見切る時間。
///
/// 整形の 20 秒 ([`format::FORMAT_TIMEOUT`]) より長い。整形は落ちても
/// R2 の劣化モード (生転写) があるので短く見切ってよいが、**画面質問には
/// 劣化先が無い** — 落ちれば「答えられませんでした」しか返せない。
/// しかも入力に画像 1 枚が乗り、出力は一覧になりうるので素で遅い。
const SCREEN_ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// 入力レベルを送る間隔。20fps。
///
/// 音声コールバックから直接送ると 1 秒に何百回も IPC を叩くことになる。
/// 見た目に必要なのは 20fps 程度なので、別スレッドで間引く。
const LEVEL_EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// エラー通知のペイロード。
///
/// `origin` が要るのは、**裏方のエラーを小窓に出させない**ため。
#[derive(Debug, Clone, Serialize)]
pub struct ErrorPayload {
    pub message: String,
    pub origin: StatusOrigin,
}

/// 転写・整形の結果。フロントと (M4 の) 履歴が使う。
#[derive(Debug, Clone, Serialize)]
pub struct ResultPayload {
    /// STT の生転写。整形結果と並置して見せる (design.md R5)。
    pub raw_text: String,
    /// 実際に採用されたテキスト。
    pub text: String,
    pub outcome: FormatOutcome,
    /// R2 の劣化モードで動いたか。
    pub degraded: bool,
    pub stt_ms: u64,
    pub format_ms: u64,
    /// WAV 生成完了から結果確定までの総時間。
    pub total_ms: u64,
    pub target_process: String,
    pub target_hwnd: isize,
    /// 録音の実尺。
    pub duration_ms: u64,
    /// Ctrl+V を送出したか。**貼られた保証ではない** ([`inject`] のモジュール doc)。
    pub injected: bool,
    pub inject_outcome: InjectOutcome,
    /// 終了時点でクリップボードに何が入っているか。
    ///
    /// 「復元した」と「ユーザーが別のものをコピーした」を区別する。
    /// 後者で「Ctrl+V で貼れます」と案内すると嘘になるため。
    pub clipboard_state: ClipboardState,
    /// R6: 退避できずに失われたクリップボード形式。
    pub lost_clipboard_formats: Vec<String>,
    /// 適用された文体プロファイルの印 ([`style::history_label`])。
    ///
    /// `None` は「文体の選択を通っていない経路」(画面質問モードなど)、
    /// `Some("")` は「どれにも当たらなかった」。履歴 DB の列と同じ約束。
    pub style_profile: Option<String>,
}

/// ファイナライズワーカーへ渡す仕事。
///
/// 停止時にコントローラが組み立て、以降の重い処理はすべてワーカー側で行う。
struct FinalizeJob {
    recorder: Recorder,
    target: TargetWindow,
    started_at: SystemTime,
    /// 録音を始めたホットキーの用途。結果の届け方を決める。
    mode: HotkeyMode,
    /// deep context の結果。使い捨てで、履歴には残さない。
    context: context::ScreenContext,
    /// 画面質問モードの走査の待ち受け口 (他の用途では `None`)。
    screen: Option<screen::ScanHandle>,
}

/// [`finalize_one`] の成果。
///
/// タプルで返すと、要素が増えるたびに呼び出し側の分解を書き換えることになり、
/// **順番を取り違えても型が同じなら通ってしまう**。
struct FinalizedRecording {
    recording: RecordingSession,
    context: context::ScreenContext,
    screen: Option<screen::ScanHandle>,
}

/// ワーカーが処理する仕事。
///
/// 再転写も同じワーカーに載せるのは、STT / 整形の呼び出しを 1 本に
/// 直列化しておきたいから (レート制限を踏みにくく、状態遷移も単純になる)。
enum WorkerJob {
    /// 録音の確定 → 転写 → 整形 → 注入。
    /// Box にしてあるのは Retranscribe との大きさの差を潰すため。
    Finalize(Box<FinalizeJob>),
    /// 履歴にある未転写行を、退避 WAV から転写し直す。
    Retranscribe { id: i64 },
    /// 保持期限の執行。定期タイマーから来る。
    EnforceRetention,
}

/// アプリ全体の共有状態。
///
/// 各フィールドを個別の `Mutex` にしてあるのは、ある操作の最中も
/// 状態問い合わせをブロックさせないため。
struct AppState {
    status: Mutex<Status>,
    recorder: Mutex<Option<Recorder>>,
    /// 録音開始時に確定させた挿入先と開始時刻。停止時に取り出す。
    pending: Mutex<Option<PendingRecording>>,
    last_session: Mutex<Option<RecordingSession>>,
    /// トレイの状態表示項目。トレイ構築後にセットされる。
    tray_status_item: Mutex<Option<MenuItem<Wry>>>,
    /// フックの生存を握る。drop でフックスレッドが止まる。
    hook: Mutex<Option<HookHandle>>,
    /// 録音長の上限到達通知。送信は音声コールバック、受信はコントローラ。
    /// 送受信端の両方を持つのは、どちらも切断させないため。
    limit_tx: Sender<()>,
    limit_rx: Receiver<()>,
    /// ワーカーへの仕事キュー。
    finalize_tx: Sender<WorkerJob>,
    finalize_rx: Receiver<WorkerJob>,
    /// 履歴 DB (R4)。
    history: HistoryStore,
    /// 再転写が進行中の履歴 ID。多重発火を弾く。
    retranscribing: Mutex<std::collections::HashSet<i64>>,
    /// レベル送出の世代。停止済みの録音のスレッドが凍った値を出し続けるのを防ぐ。
    ///
    /// 単なる ON/OFF フラグだと、停止→即再開のときに古いスレッドが
    /// 生き残って二重に送る。世代が変わったら古い方は黙って終わる。
    level_generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// 設定 (API キー・言語・辞書など)。
    config: ConfigStore,
    /// STT / 整形で共用する HTTP クライアント。接続プールを使い回すため
    /// 録音ごとに作り直さない。構築に失敗した場合のみ `None`。
    http: Option<reqwest::blocking::Client>,
    /// 直近の転写・整形結果。
    last_result: Mutex<Option<ResultPayload>>,
    /// STT に失敗した WAV の退避先 (M4 の履歴 DB が入るまでの暫定)。
    failed_dir: PathBuf,
    /// ローカル STT のモデル置き場。
    models_dir: PathBuf,
    /// 進行中のキー捕獲がどの用途のものか ([`HotkeyMode::slot`])。
    ///
    /// 捕獲の確定はコントローラスレッドで起きるが、「どのキーを設定しに
    /// 来たのか」を知っているのは捕獲を始めたコマンド側だけ。
    /// 捕獲は排他 (同時に 1 つ) なので 1 枠で足りる。
    capture_slot: std::sync::atomic::AtomicUsize,
}

impl AppState {
    fn new(
        config_path: PathBuf,
        failed_dir: PathBuf,
        db_path: PathBuf,
        models_dir: PathBuf,
    ) -> Self {
        // 上限到達は 1 録音につき高々 1 回。
        let (limit_tx, limit_rx) = crossbeam_channel::bounded(1);
        let (finalize_tx, finalize_rx) = crossbeam_channel::unbounded();
        let http = match stt::build_http_client() {
            Ok(c) => Some(c),
            Err(e) => {
                log::error!("{e}");
                None
            }
        };
        Self {
            status: Mutex::new(Status::Idle),
            recorder: Mutex::new(None),
            pending: Mutex::new(None),
            last_session: Mutex::new(None),
            tray_status_item: Mutex::new(None),
            hook: Mutex::new(None),
            limit_tx,
            limit_rx,
            finalize_tx,
            finalize_rx,
            config: ConfigStore::load(config_path),
            history: HistoryStore::new(db_path),
            retranscribing: Mutex::new(std::collections::HashSet::new()),
            level_generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            http,
            last_result: Mutex::new(None),
            failed_dir,
            models_dir,
            capture_slot: std::sync::atomic::AtomicUsize::new(HotkeyMode::Inject.slot()),
        }
    }

    fn is_recording(&self) -> bool {
        self.recorder.lock().map(|r| r.is_some()).unwrap_or(false)
    }
}

/// 現在の状態を返す。
#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> Status {
    state.status.lock().map(|s| *s).unwrap_or(Status::Idle)
}

/// 直近の録音結果の要約を返す (WAV 本体は含まない)。
#[tauri::command]
fn get_last_session(state: tauri::State<'_, AppState>) -> Option<SessionSummary> {
    state
        .last_session
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(RecordingSession::summary))
}

/// 直近の転写・整形結果を返す。
#[tauri::command]
fn get_last_result(state: tauri::State<'_, AppState>) -> Option<ResultPayload> {
    state.last_result.lock().ok().and_then(|r| r.clone())
}

/// 設定を返す。**API キーの実体は含まない** ([`ConfigView`] 参照)。
#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> ConfigView {
    ConfigView::from(&state.config.snapshot())
}

/// 設定を部分更新して保存する。返すのは更新後のビュー (キーは含まない)。
#[tauri::command]
fn set_config(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    patch: ConfigPatch,
) -> Result<ConfigView, String> {
    // patch は API キーを含みうる。ログには出さない。
    let previous_retention = state.config.snapshot().history_retention_days;
    let updated = state.config.update(patch)?;
    // 保持日数を縮めたなら、次の録音を待たずに今すぐ効かせる。
    if updated.history_retention_days != previous_retention {
        enforce_retention(&app, updated.history_retention_days);
    }
    // ホットキーはフックを設置し直さず、比較する組み合わせだけ差し替える。
    apply_hotkeys(&updated);
    // キャンセルキーも同じく即時反映 (0 なら無効化)。
    hotkey::set_cancel_vk(updated.cancel_vk);
    // オーバーレイを後から有効にした場合はその場で作る。
    if updated.overlay_enabled {
        if let Err(e) = overlay::create(&app) {
            log::error!("オーバーレイを作成できません: {e}");
        }
    } else {
        overlay::hide(&app);
    }
    Ok(ConfigView::from(&updated))
}

/// 履歴を新しい順に返す。`before_id` を渡すと続きを取る。
///
/// **読めなかった場合は `Err`。** 空リストで返してはいけない
/// (UI が「履歴 0 件」と表示し、ユーザーは消えたと信じてしまう)。
#[tauri::command]
fn get_history(
    state: tauri::State<'_, AppState>,
    limit: Option<u32>,
    before_id: Option<i64>,
    query: Option<String>,
) -> Result<Vec<SessionRow>, String> {
    state
        .history
        .search(limit.unwrap_or(50), before_id, query.as_deref())
        .map_err(|e| e.to_string())
}

/// 履歴 1 件を返す。
#[tauri::command]
fn get_history_entry(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<Option<SessionRow>, String> {
    state.history.get(id).map_err(|e| e.to_string())
}

/// 履歴 1 件を削除する (退避 WAV も一緒に消える)。
#[tauri::command]
fn delete_history_entry(app: AppHandle, id: i64) -> Result<(), String> {
    let removal = app
        .state::<AppState>()
        .history
        .delete(id)
        .map_err(|e| e.to_string())?;
    emit_history_changed(&app);
    report_removal(&app, &removal, "履歴を削除しました")
}

/// 履歴を全消去する。呼び出し側で確認を取ってから使うこと。
#[tauri::command]
fn clear_history(app: AppHandle) -> Result<u64, String> {
    let removal = app
        .state::<AppState>()
        .history
        .clear()
        .map_err(|e| e.to_string())?;
    log::info!(
        "履歴を全消去しました ({} 件 / WAV {} 個)",
        removal.rows,
        removal.wavs_removed
    );
    emit_history_changed(&app);
    report_removal(&app, &removal, "履歴を全消去しました")?;
    Ok(removal.rows)
}

/// 削除の消し残しをユーザーへ伝える。
///
/// 「消したつもりでファイルが残っている」を無言にしない。残ったファイルを
/// 持つ行はあえて残してあるので、UI にもその行が見えたままになる。
fn report_removal(
    app: &AppHandle,
    removal: &history::Removal,
    _context: &str,
) -> Result<(), String> {
    if removal.is_complete() {
        return Ok(());
    }
    let message = format!(
        "退避した録音 {} 個を削除できませんでした。該当の履歴は残してあります (ファイルが使用中の可能性)",
        removal.wav_failures.len()
    );
    // 通知はクリックしても何も起きないので、行き先を文言で示す
    // (トレイの「履歴を開く」からも辿れる)。
    log::error!("{message}: {:?}", removal.wav_failures);
    notify(app, Notice::ActionRequired, &message);
    Err(message)
}

/// 履歴のテキストをクリップボードへ入れる (再貼付)。
///
/// **注入はしない。** どのウィンドウへ貼るかはこの時点では決められず、
/// 勝手に前景へ送ると意図しないアプリを壊す (R7 の考え方)。
/// クリップボードに置いてユーザーの Ctrl+V に委ねる。
#[tauri::command]
fn copy_history_entry(app: AppHandle, id: i64) -> Result<(), String> {
    let state = app.state::<AppState>();
    let row = state
        .history
        .get(id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "履歴が見つかりません".to_string())?;
    let text = row
        .text()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| "この履歴にはテキストがありません".to_string())?;

    inject::set_clipboard_text(text).map_err(|e| format!("{e:?}"))?;
    notify(
        &app,
        Notice::ActionRequired,
        "クリップボードに入れました。Ctrl+V で貼り付けてください",
    );
    Ok(())
}

/// 未転写の履歴を再転写する (ワーカーで直列に実行)。
///
/// 同じ行への二重要求は拒否する。連打するとワーカーのキューに積み上がり、
/// STT を無駄に何度も叩いたうえ、後から来た結果で上書きし合う。
#[tauri::command]
fn retranscribe_history_entry(state: tauri::State<'_, AppState>, id: i64) -> Result<(), String> {
    {
        let mut running = state
            .retranscribing
            .lock()
            .map_err(|_| "再転写の状態が壊れています".to_string())?;
        if !running.insert(id) {
            return Err("この履歴は再転写中です".to_string());
        }
    }

    if let Err(e) = state.finalize_tx.send(WorkerJob::Retranscribe { id }) {
        // キューに載せられなかったので進行中の印を戻す。
        if let Ok(mut running) = state.retranscribing.lock() {
            running.remove(&id);
        }
        let _ = e;
        return Err("後処理ワーカーが停止しています".to_string());
    }
    Ok(())
}

/// キー捕獲モードを開始する (設定 UI の「キーを押して設定」)。
///
/// キーを拾うのは**フロントの DOM** ([`finish_hotkey_capture`])。ここが行うのは
/// 「捕獲中である」ことの宣言だけで、その間フックは PTT の解釈をやめる
/// ([`hotkey::begin_capture`])。押し忘れて放置されると設定画面が捕獲状態のまま
/// 残るので、[`CAPTURE_TIMEOUT`] で自動的に畳む。
#[tauri::command]
fn start_hotkey_capture(app: AppHandle, mode: Option<String>) -> Result<u64, String> {
    // 録音中に捕獲へ入ると、PTT の離しが捕獲側へ吸われて録音が止まらなくなる。
    hotkey::can_begin_capture(app.state::<AppState>().is_recording()).map_err(str::to_string)?;

    // どの用途のキーを設定しに来たのか。確定はコントローラスレッドで
    // 起きるので、ここで残しておかないと行き先が分からない。
    let mode = parse_mode(mode.as_deref())?;
    app.state::<AppState>()
        .capture_slot
        .store(mode.slot(), std::sync::atomic::Ordering::SeqCst);
    log::info!("キー捕獲を開始 [{}]", mode.label());

    let generation = hotkey::begin_capture();

    // 捕獲そのものはフックに依存しなくなったが、**設定したホットキーを押す
    // 経路は依存したまま**。ユーザーがこの画面に居るということは、直後に
    // 押して試すということなので、ここでフックの生存を 1 回確かめておく。
    // 「設定はできたのに押しても録音が始まらない」を減らすための止血
    // ([`hotkey::ensure_hook_alive_async`] の doc)。捕獲の表示を遅らせない
    // よう、確認は別スレッドで走る。
    hotkey::ensure_hook_alive_async("キー捕獲の開始");

    // 時間で必ず畳む。自分の世代のときだけ効く。
    let timer_app = app.clone();
    let spawned = thread::Builder::new()
        .name("nox-capture-timeout".to_string())
        .spawn(move || {
            std::thread::sleep(CAPTURE_TIMEOUT);
            if hotkey::end_capture(Some(generation)) {
                log::info!("キー捕獲がタイムアウトしました");
                emit_hotkey_captured(&timer_app, None);
                // 「押したのに何も起きなかった」場合もここに落ちてくる。
                // DOM 方式では Win キーの組み合わせ・PrintScreen など、
                // OS が先に処理してしまうキーがそもそもウィンドウへ届かない。
                // 黙って取り消すと理由が分からないので必ず添える。
                emit_error(
                    &timer_app,
                    "キーが押されなかったため、ホットキーの設定を取り消しました。\n\
Win キーとの組み合わせや PrintScreen は Windows が先に処理するため設定できません",
                );
            }
        });
    if let Err(e) = spawned {
        log::warn!("キー捕獲のタイムアウトを設定できません: {e}");
    }
    Ok(CAPTURE_TIMEOUT.as_secs())
}

/// キー捕獲モードを中止する (UI 側で閉じた場合など)。
#[tauri::command]
fn cancel_hotkey_capture() {
    hotkey::end_capture(None);
}

/// 押している最中の `code` 列を表示名にする (「左 Ctrl + Space」)。
///
/// 表示名も Rust 側にしか無い ([`hotkey::key_label`])。フロントで組み立てると
/// **経過表示と確定後のラベルが別々の綴りになる**ので、経過も往復させる。
/// 写せない `code` はここでは弾かない — 押している最中に「使えません」と
/// 出しても、まだ組み合わせが完成していないので早すぎる。確定時
/// ([`finish_hotkey_capture`]) に理由つきで断る。
#[tauri::command]
fn describe_hotkey_codes(codes: Vec<String>) -> String {
    let keys: Vec<u32> = codes.iter().filter_map(|c| hotkey::code_to_vk(c)).collect();
    hotkey::describe_keys(&keys)
}

/// フロントから来た用途名を [`HotkeyMode`] へ。既定は貼り付け。
fn parse_mode(raw: Option<&str>) -> Result<HotkeyMode, String> {
    match raw.unwrap_or("inject") {
        "inject" => Ok(HotkeyMode::Inject),
        "clipboard_only" => Ok(HotkeyMode::ClipboardOnly),
        "screen_ask" => Ok(HotkeyMode::ScreenAsk),
        other => Err(format!("不明なホットキー用途です: {other}")),
    }
}

/// 用途に割り当てたホットキーを解除する。
///
/// 貼り付け用は解除できない — 解除すると録音を始める手段が無くなり、
/// 設定画面を開くことでしか復旧できないアプリになる。
#[tauri::command]
fn clear_hotkey(app: AppHandle, mode: Option<String>) -> Result<ConfigView, String> {
    let mode = parse_mode(mode.as_deref())?;
    if mode == HotkeyMode::Inject {
        return Err("録音用のホットキーは解除できません".to_string());
    }
    let state = app.state::<AppState>();
    let patch = match mode {
        HotkeyMode::ClipboardOnly => config::ConfigPatch {
            clipboard_hotkey_vk: Some(0),
            clipboard_hotkey_mods: Some(Vec::new()),
            ..Default::default()
        },
        // 画面質問は**有効化フラグには触らない**。「キーを付け替えたい」と
        // 「機能ごと止めたい」は別の意図で、解除のたびにトグルまで倒すと
        // 前者のつもりの操作が後者になる。
        HotkeyMode::ScreenAsk => config::ConfigPatch {
            screen_ask_hotkey_vk: Some(0),
            screen_ask_hotkey_mods: Some(Vec::new()),
            ..Default::default()
        },
        // 上で弾いてある。
        HotkeyMode::Inject => return Err("録音用のホットキーは解除できません".to_string()),
    };
    let updated = state.config.update(patch)?;
    apply_hotkeys(&updated);
    Ok(ConfigView::from(&updated))
}

/// 設定 UI の試聴。**保存前の値**で鳴らせるようにする
/// (聞いてから決められないと、選ぶたびに保存する羽目になる)。
#[tauri::command]
fn preview_sound(
    state: tauri::State<'_, AppState>,
    preset: sound::SoundPreset,
    path: Option<String>,
    volume: Option<u8>,
) -> Result<(), String> {
    let cfg = state.config.snapshot();
    let volume = volume.unwrap_or(cfg.sound_volume).min(100);
    let choice = sound::SoundChoice::new(preset, path.as_deref().unwrap_or_default());
    // 試聴だけは失敗を返す。設定 UI では「鳴らない」理由が要る
    // (通常の再生経路はログに落とすだけで、録音を止めない)。
    let wav = sound::render_wav(&choice, volume.max(1))?;
    sound::play_rendered(wav);
    Ok(())
}

/// プリセット一覧 (id と表示名)。UI の選択肢を Rust 側の定義から作る。
#[tauri::command]
fn list_sound_presets() -> Vec<SoundPresetInfo> {
    sound::SoundPreset::all()
        .iter()
        .map(|p| SoundPresetInfo {
            id: p.id().to_string(),
            label: p.label().to_string(),
        })
        .collect()
}

/// [`list_sound_presets`] の 1 件。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SoundPresetInfo {
    pub id: String,
    pub label: String,
}

/// オーバーレイの webview が読み込まれたことを知らせる。
///
/// **capability に "overlay" を入れ忘れると IPC が全部拒否され、
/// 小窓は透明な空ウィンドウのまま**になる。見た目では気づけないので、
/// webview 側から往復で呼ばせてログに残す。この行が出ていれば
/// 「Rust → イベント → webview → invoke → Rust」が通っている証拠になる。
#[tauri::command]
fn overlay_ready(app: AppHandle, listeners: usize) {
    log::info!("オーバーレイの webview が待受を開始しました (listener {listeners} 件)");
    // 起動直後の状態を送って同期する。捕獲や再転写の途中でも表示が食い違わない。
    let status = app
        .state::<AppState>()
        .status
        .lock()
        .map(|s| *s)
        .unwrap_or(Status::Idle);
    if let Err(e) = app.emit(
        EVENT_STATUS,
        StatusPayload {
            status,
            message: None,
            origin: StatusOrigin::Recording,
        },
    ) {
        log::warn!("オーバーレイへの初期状態送出に失敗: {e}");
    }
}

/// オーバーレイが状態イベントを受けて描画を変えたことを知らせる (疎通確認用)。
#[tauri::command]
fn overlay_rendered(state: String) {
    log::info!("オーバーレイの表示を更新しました: {state}");
}

/// 退避 WAV の使用量。
#[derive(Debug, Clone, Serialize)]
pub struct StorageStats {
    /// `failed/` 配下の WAV の合計バイト数。
    pub failed_bytes: u64,
    /// WAV の個数。
    pub failed_files: usize,
    /// 未転写として履歴に載っている行数。
    pub untranscribed_rows: u64,
}

/// ローカル STT が使えるかを返す。
#[tauri::command]
fn get_local_stt_status(state: tauri::State<'_, AppState>) -> local_stt::Availability {
    local_stt::availability(&state.models_dir)
}

/// モデルのダウンロード進捗イベント。
const EVENT_MODEL_PROGRESS: &str = "nox://model-progress";

/// ローカル STT のモデルをダウンロードする。
///
/// 1GB 級で数分かかるので、専用スレッドで走らせて進捗をイベントで返す。
/// UI スレッドも録音もブロックしない。
#[tauri::command]
fn download_local_model(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let Some(http) = state.http.clone() else {
        return Err("HTTP クライアントを構築できませんでした".to_string());
    };
    let models_dir = state.models_dir.clone();
    // 期待ハッシュは設定から取る (TOFU: 初回の値を以後の照合に使う)。
    let expected = state.config.snapshot().local_model_sha256;

    // 2 本が同じ .part を交互に書くと、壊れたモデルが検証を通ってしまう。
    if !local_stt::begin_download() {
        return Err("既にダウンロード中です".to_string());
    }

    let spawned = thread::Builder::new()
        .name("nox-model-download".to_string())
        .spawn(move || {
            let progress_app = app.clone();
            let result = local_stt::download_model(
                &http,
                &models_dir,
                local_stt::DEFAULT_MODEL_URL,
                (!expected.is_empty()).then_some(expected.as_str()),
                |progress| {
                    if let Err(e) = progress_app.emit(EVENT_MODEL_PROGRESS, &progress) {
                        log::debug!("進捗イベントの送出に失敗: {e}");
                    }
                },
            );
            local_stt::end_download();

            match result {
                Ok(outcome) => {
                    // 初回成功時に計算値を固定する。以後の再取得はこれと照合される。
                    if expected.is_empty() {
                        if let Err(e) = app.state::<AppState>().config.update(config::ConfigPatch {
                            local_model_sha256: Some(outcome.sha256.clone()),
                            ..Default::default()
                        }) {
                            log::warn!("モデルのハッシュを保存できません: {e}");
                        } else {
                            log::info!("モデルの SHA-256 を記録しました: {}", outcome.sha256);
                        }
                    }
                    notify(
                        &app,
                        Notice::ActionRequired,
                        "ローカル認識のモデルを準備しました",
                    );
                }
                Err(e) => {
                    log::error!("モデルのダウンロードに失敗: {e}");
                    // UI が「ダウンロード中…」で固まらないよう、必ず終了を知らせる。
                    if let Err(e) = app.emit(
                        EVENT_MODEL_PROGRESS,
                        serde_json::json!({
                            "downloaded": 0,
                            "total": serde_json::Value::Null,
                            "done": true,
                            "failed": true,
                        }),
                    ) {
                        log::debug!("進捗イベントの送出に失敗: {e}");
                    }
                    emit_background_error(
                        &app,
                        &format!("モデルのダウンロードに失敗しました: {e}"),
                    );
                }
            }
        });

    if let Err(e) = spawned {
        local_stt::end_download();
        return Err(format!("ダウンロードを開始できません: {e}"));
    }
    Ok(())
}

/// 退避 WAV の使用量を返す。
#[tauri::command]
fn get_storage_stats(state: tauri::State<'_, AppState>) -> Result<StorageStats, String> {
    let (failed_bytes, failed_files) = measure_failed_dir(&state.failed_dir);
    let untranscribed_rows = state
        .history
        .untranscribed_count()
        .map_err(|e| e.to_string())?;
    Ok(StorageStats {
        failed_bytes,
        failed_files,
        untranscribed_rows,
    })
}

/// ダッシュボード用の集計統計を返す。
#[tauri::command]
fn get_dashboard_stats(state: tauri::State<'_, AppState>) -> Result<DashboardStats, String> {
    let cfg = state.config.snapshot();
    let typing_speed = cfg.typing_speed_chars_per_min;
    state
        .history
        .get_dashboard_stats(typing_speed)
        .map_err(|e| e.to_string())
}

/// 「よく使っているのに文体プロファイルが無いアプリ」の提案。
///
/// **0 件と欠測を混同しない**ための形。`history_enabled` が偽なら
/// そもそも数える材料が無く、`total_sessions` が 0 なら材料はあるが
/// まだ喋っていない。どちらも「提案なし」と一緒に見せると、
/// 利用者は「この機能は壊れている」としか受け取れない。
#[derive(Debug, Clone, Serialize)]
pub struct StyleSuggestions {
    /// 履歴が有効か。偽なら以下の数字はすべて意味を持たない。
    pub history_enabled: bool,
    /// 集計に使えた録音の総数。
    pub total_sessions: u64,
    /// 提案 (多い順)。
    pub items: Vec<style::ProcessUsage>,
}

/// 設定画面に出す提案の上限。多すぎると「作業リスト」に見えてしまう。
const STYLE_SUGGESTION_LIMIT: usize = 5;

/// 履歴を集計して、プロファイルが無い挿入先の上位を返す。
#[tauri::command]
fn get_style_suggestions(state: tauri::State<'_, AppState>) -> Result<StyleSuggestions, String> {
    let cfg = state.config.snapshot();
    if !cfg.history_enabled {
        return Ok(StyleSuggestions {
            history_enabled: false,
            total_sessions: 0,
            items: Vec::new(),
        });
    }
    let total_sessions = state.history.count().map_err(|e| e.to_string())?;
    // 上位だけを見ると、既に設定済みのアプリで枠が埋まって何も出ない。
    // 余分に取ってから絞る。
    let usage = state
        .history
        .process_usage(50)
        .map_err(|e| e.to_string())?;
    Ok(StyleSuggestions {
        history_enabled: true,
        total_sessions,
        items: style::suggest_uncovered(&usage, &cfg.style_profiles, STYLE_SUGGESTION_LIMIT),
    })
}

/// `failed/` の WAV を数えて合計サイズを出す。
///
/// 読めないディレクトリは 0 件として扱う (統計表示のためだけなので、
/// ここで失敗してもアプリの機能は損なわれない)。
fn measure_failed_dir(dir: &std::path::Path) -> (u64, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut bytes = 0u64;
    let mut files = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wav") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            bytes += meta.len();
            files += 1;
        }
    }
    (bytes, files)
}

/// 未転写の録音をまとめて削除する (WAV + 履歴行)。
///
/// 呼び出し側で確認を取ってから使うこと。
#[tauri::command]
fn delete_untranscribed(app: AppHandle) -> Result<u64, String> {
    let removal = app
        .state::<AppState>()
        .history
        .delete_untranscribed()
        .map_err(|e| e.to_string())?;
    log::info!(
        "未転写の録音を削除しました ({} 行 / WAV {} 個)",
        removal.rows,
        removal.wavs_removed
    );
    emit_history_changed(&app);
    report_removal(&app, &removal, "未転写の録音を削除しました")?;
    Ok(removal.rows)
}

/// 履歴を開く (トレイ・通知からの導線)。
#[tauri::command]
fn open_history(app: AppHandle) {
    show_history(&app);
}

/// メインウィンドウを出して履歴へ誘導する。
fn show_history(app: &AppHandle) {
    tray::show_main_window(app);
    if let Err(e) = app.emit(EVENT_SHOW_HISTORY, ()) {
        log::warn!("履歴表示イベントの送出に失敗: {e}");
    }
}

/// 設定ウィンドウ (現状はメインウィンドウ) を表示する。
#[tauri::command]
fn show_window(app: AppHandle) {
    tray::show_main_window(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // **1 行目でやること**。Tauri のビルダーに触る前に名前付きミューテックスを取る。
    //
    // 下の `tauri-plugin-single-instance` は残してあるが、あれだけでは漏れる。
    // プラグインの判定は `Builder::setup` の中 (= アプリ初期化のかなり後) で
    // 走るうえ、「ミューテックスは在るがウィンドウがまだ無い」状態を
    // 「起動していない」と読んで**そのまま 2 個目を起動する**。
    // 昇格レベルが食い違う場合も同様に素通りする。詳しくは `instance` モジュール。
    //
    // カーネルオブジェクトの生成はアトミックで、ウィンドウ生成より桁違いに速い。
    // ここで取り切ることで起動競合が原理的に消える。
    // 既に起動していれば、この関数は既存インスタンスへ通知してから終了する。
    instance::ensure_single_instance();

    let app = tauri::Builder::default()
        // **多重起動を止める。他のプラグインより先に入れること** (公式の要件)。
        //
        // ただし多重起動を実際に止めているのは、上の `instance::ensure_single_instance()`
        // (名前付きミューテックス) のほう。このプラグインが担うのは UX 側、
        // すなわち「2 個目が起動されたら既存のウィンドウを前に出す」だけである。
        // 2 個目はミューテックスの時点で終了するので、下のコールバックは
        // 2 個目のプロセスからの `WM_COPYDATA` を**この 1 個目が受け取って**走る。
        //
        // 常駐トレイアプリはウィンドウが見えないので、二重に起動しても
        // 利用者からは分からない。そして二重に起動すると:
        //
        // - フックが 2 本刺さり、1 回のホットキーで**録音が 2 回**始まる
        // - 片方で設定を変えても、もう片方は古いホットキーのまま反応し続ける
        //   (「設定は保存されるのに効かない」の正体)
        // - 設定ファイルとログファイルを 2 プロセスで奪い合う
        //   (実際にログが途中から切り詰められているのを観測した)
        //
        // 2 個目が起動されたら、既存のインスタンスのウィンドウを出して終了する。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            log::info!("既に起動しています。既存のウィンドウを表示します");
            tray::show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        // 常駐運用ではウィンドウが閉じているので、行動を要する通知は
        // WebView イベントではなく OS トーストで出す必要がある。
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                // `target()` は既定ターゲットに「追加」する (置き換えではない) ため、
                // 明示指定するときは `targets()` で丸ごと差し替える。
                // 追加してしまうと同じログが二重に書かれる。
                .targets([
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
                    tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
                        file_name: None,
                    }),
                ])
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_last_session,
            get_last_result,
            get_config,
            set_config,
            get_history,
            get_history_entry,
            delete_history_entry,
            clear_history,
            copy_history_entry,
            retranscribe_history_entry,
            start_hotkey_capture,
            cancel_hotkey_capture,
            describe_hotkey_codes,
            finish_hotkey_capture,
            clear_hotkey,
            preview_sound,
            list_sound_presets,
            overlay_ready,
            overlay_rendered,
            open_history,
            get_storage_stats,
            get_dashboard_stats,
            get_style_suggestions,
            delete_untranscribed,
            get_local_stt_status,
            download_local_model,
            show_window
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // 設定と退避先のパスは AppHandle が無いと決まらないので、
            // 状態の登録は builder ではなく setup で行う。
            let (config_path, failed_dir) = resolve_paths(&handle);
            let db_path = resolve_db_path(&handle);
            let models_dir = db_path
                .parent()
                .map(|dir| dir.join("models"))
                .unwrap_or_else(|| PathBuf::from("models"));
            app.manage(AppState::new(config_path, failed_dir, db_path, models_dir));
            initialize_history(&handle);

            match tray::build(&handle) {
                Ok(item) => {
                    if let Ok(mut slot) = handle.state::<AppState>().tray_status_item.lock() {
                        *slot = Some(item);
                    }
                }
                // トレイが出せなくても録音機能自体は使えるので継続する。
                Err(e) => log::error!("トレイの構築に失敗: {e}"),
            }

            // 設定のホットキーを反映してからフックを設置する。
            let cfg = handle.state::<AppState>().config.snapshot();
            apply_hotkeys(&cfg);
            hotkey::set_cancel_vk(cfg.cancel_vk);

            if cfg.overlay_enabled {
                if let Err(e) = overlay::create(&handle) {
                    // 小窓が出せなくても録音はできる。
                    log::error!("オーバーレイを作成できません: {e}");
                }
            }

            start_finalize_worker(&handle);
            start_hotkey_controller(&handle);
            start_retention_timer(&handle);

            // ウィンドウは tauri.conf.json で非表示にして作られる。
            // 「出してから隠す」と一瞬フラッシュするので、出す側を明示する。
            let first_run = handle.state::<AppState>().config.is_first_run();
            if first_run || !cfg.start_hidden {
                if first_run {
                    log::info!("初回起動のため設定ウィンドウを表示します");
                }
                tray::show_main_window(&handle);
            } else {
                log::info!("トレイ常駐で起動しました (ウィンドウ非表示)");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 「閉じる」は終了ではなく非表示。常駐を維持する。
            // オーバーレイは装飾なしで閉じる手段が無いが、念のため同じ扱いにする。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(e) = window.hide() {
                    log::warn!("ウィンドウの非表示に失敗: {e}");
                }
            }
        })
        .build(tauri::generate_context!());

    let app = match app {
        Ok(app) => app,
        Err(e) => {
            // ここで落ちるとログにも残らないので eprintln も併用する。
            eprintln!("Tauri アプリの初期化に失敗しました: {e}");
            log::error!("Tauri アプリの初期化に失敗しました: {e}");
            return;
        }
    };

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            // レベル送出スレッドを畳む。
            stop_level_emitter(handle);
            // 録音中の終了 (トレイの「終了」等) で音声を無警告に捨てない。
            finalize_on_exit(handle);
            // 後処理待ちのキューに残っている録音も同様に退避する。
            let state = handle.state::<AppState>();
            let recovered = drain_pending_finalizations(&state.finalize_rx, &state.failed_dir);
            if recovered > 0 {
                log::warn!("後処理待ちだった録音 {recovered} 件を退避しました");
            }
            // フックスレッドを明示的に畳む (drop 任せにしない)。
            if let Ok(mut hook) = state.hook.lock() {
                hook.take();
            };
        }
    });
}

/// 終了時、ファイナライズ待ちのキューに残っているジョブを WAV として退避する。
///
/// design.md R4 (発話データの保全) の趣旨。処理中に終了すると、
/// キューに積まれた録音は誰にも処理されないまま消える。転写は行わず
/// (ネットワーク待ちで終了を引き延ばさない)、WAV だけ確実に残す。
///
/// 実行中の 1 件はワーカーが握っているのでここでは救えない。
/// それはワーカー側の [`process_job`] が最後まで走るのに任せる。
///
/// 戻り値は退避できた件数。
fn drain_pending_finalizations(rx: &Receiver<WorkerJob>, dir: &std::path::Path) -> usize {
    let mut recovered = 0;
    // try_recv なのでキューが空になれば即抜ける (終了処理を止めない)。
    while let Ok(job) = rx.try_recv() {
        // 再転写・保持期限は失うものが無い (WAV はディスク上にある)。
        let WorkerJob::Finalize(job) = job else {
            continue;
        };
        let Some(Ok(FinalizedRecording { recording, .. })) =
            guard_panic("終了時の WAV 化", || finalize_one(*job))
        else {
            log::error!("終了時に後処理待ちの録音を WAV 化できませんでした");
            continue;
        };
        if save_failed_recording(dir, &recording, "後処理待ちのまま終了したため未転写").is_some()
        {
            recovered += 1;
        }
    }
    recovered
}

/// 設定ファイルと失敗 WAV 退避先のパスを決める。
///
/// どちらも取得に失敗しうる (ポータブル実行など) ので、
/// その場合は実行ファイル隣の `nox-voice-data` へ退避する。
fn resolve_paths(app: &AppHandle) -> (PathBuf, PathBuf) {
    let base = app
        .path()
        .app_config_dir()
        .or_else(|_| app.path().app_data_dir())
        .unwrap_or_else(|e| {
            log::warn!(
                "アプリのデータディレクトリを解決できません ({e})。実行ファイル隣に置きます"
            );
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("nox-voice-data")
        });
    (base.join("config.json"), base.join("failed"))
}

/// 履歴 DB のパス。
fn resolve_db_path(app: &AppHandle) -> PathBuf {
    let (config_path, _) = resolve_paths(app);
    config_path
        .parent()
        .map(|dir| dir.join("nox-voice.db"))
        .unwrap_or_else(|| PathBuf::from("nox-voice.db"))
}

/// ファイナライズワーカースレッドを起動する。
fn start_finalize_worker(app: &AppHandle) {
    let rx = app.state::<AppState>().finalize_rx.clone();
    let worker_app = app.clone();
    let spawned = thread::Builder::new()
        .name("nox-finalize".to_string())
        .spawn(move || finalize_worker(worker_app, rx));

    if let Err(e) = spawned {
        let msg = format!("録音の後処理スレッドを起動できませんでした: {e}");
        log::error!("{msg}");
        emit_error(app, &msg);
    }
}

/// 設定の全ホットキーをフックへ反映する。
///
/// 用途を 1 つでも反映し忘れると「設定したのに効かない」になる。
/// 反映は必ずこの 1 か所を通す (design.md「同じ判断を 2 か所に書かない」)。
fn apply_hotkeys(cfg: &config::Config) {
    hotkey::set_mode_hotkey(HotkeyMode::Inject, Some(cfg.hotkey_combo()));
    hotkey::set_mode_hotkey(HotkeyMode::ClipboardOnly, cfg.clipboard_hotkey_combo());
    // `screen_ask_hotkey_combo()` は有効化フラグも見る。設定でオフにした
    // 瞬間にキーが死ぬ ({「オフなのに押すと画面が送られる」}を作らない)。
    hotkey::set_mode_hotkey(HotkeyMode::ScreenAsk, cfg.screen_ask_hotkey_combo());
}

/// フックを設置し、ホットキーイベントを解釈するコントローラスレッドを起動する。
fn start_hotkey_controller(app: &AppHandle) {
    let rx = match hotkey::spawn() {
        Ok((handle, rx)) => {
            if let Ok(mut slot) = app.state::<AppState>().hook.lock() {
                *slot = Some(handle);
            }
            rx
        }
        Err(e) => {
            // ホットキーが使えなくてもアプリは起動させ、理由を可視化する。
            let msg = format!("ホットキーを登録できませんでした: {e}");
            log::error!("{msg}");
            emit_error(app, &msg);
            return;
        }
    };

    let limit_rx = app.state::<AppState>().limit_rx.clone();
    let worker_app = app.clone();
    let spawned = thread::Builder::new()
        .name("nox-hotkey-controller".to_string())
        .spawn(move || {
            let app = worker_app;
            // 用途ごとに解釈器を持つ。押下パターン (長押し / トグル) は
            // 用途ごとに独立していなければならない — 「貼り付けをトグルで
            // 開始 → クリップボード用キーを踏む」で、片方の状態が
            // もう片方の判定に混ざると、止まらない録音ができあがる。
            let mut interpreters: [PttInterpreter; HOTKEY_SLOTS] =
                std::array::from_fn(|_| PttInterpreter::new(TAP_THRESHOLD));
            // 録音中の用途。**別用途のキーは録音中は無視する**。
            // 二重に開始できない以上、受け付けても「すでに録音中です」の
            // エラーを出すだけで、解釈器の状態が無駄に汚れる。
            let mut active_mode: Option<HotkeyMode> = None;
            let mut seen_drops = 0u64;
            // ここに捕獲の蓄積は無い。設定 UI のキー捕獲はフックを通らず、
            // フロントの DOM イベント → `finish_hotkey_capture` で確定する
            // (`hotkey::CAPTURE_MODE` の doc)。捕獲中フックは何も出さない。

            loop {
                crossbeam_channel::select! {
                    recv(rx) -> msg => match msg {
                        // フック側が落ちた = 終了。
                        Err(_) => break,
                        Ok(event) => {
                            // 録音のキャンセルは解釈器を通さない別経路。
                            // 破棄した時点で解釈器の押下状態は無効になるので捨てる
                            // (開始失敗時と同じ整理)。押しっぱなしだった PTT キーの
                            // 離しが、あとから StopRecording に化けないようにする。
                            if event.kind == hotkey::HotkeyEventKind::Cancel {
                                cancel_recording(&app);
                                for it in interpreters.iter_mut() {
                                    it.reset();
                                }
                                active_mode = None;
                                continue;
                            }
                            // どの用途のキーか。ここから先は用途ごとの解釈器へ。
                            let mode = match event.kind {
                                hotkey::HotkeyEventKind::Press { mode }
                                | hotkey::HotkeyEventKind::Release { mode } => mode,
                                // 上で処理済み (キャンセル)。
                                hotkey::HotkeyEventKind::Cancel => continue,
                            };
                            if active_mode.is_some_and(|active| active != mode) {
                                log::debug!(
                                    "[{}] のキーは無視 ({} で録音中)",
                                    mode.label(),
                                    active_mode.map(HotkeyMode::label).unwrap_or("")
                                );
                                continue;
                            }
                            let interpreter = &mut interpreters[mode.slot()];
                            // 判定は必ずイベントの発生時刻で行う。
                            // ここで Instant::now() を使うと、直前の処理で
                            // 詰まった分だけ短押しが長押しに化ける。
                            match interpreter.on_event(event) {
                                Some(HotkeyAction::StartRecording) => {
                                    match start_recording(&app, mode) {
                                        Ok(()) => active_mode = Some(mode),
                                        Err(e) => {
                                            log::error!("録音を開始できません: {e}");
                                            let cfg = app.state::<AppState>().config.snapshot();
                                            sound::play(
                                                &cfg.cancel_sound_choice(),
                                                cfg.effective_sound_volume(),
                                            );
                                            emit_error(&app, &e);
                                            set_status(&app, Status::Idle, Some(e));
                                            interpreter.reset();
                                            active_mode = None;
                                        }
                                    }
                                }
                                Some(HotkeyAction::StopRecording) => {
                                    active_mode = None;
                                    // 停止時に `reset()` は呼ばないこと。
                                    // 停止を出した時点で解釈器の状態は既に整合しており、
                                    // reset の「次の離しを捨てる」副作用が次回の
                                    // 長押し PTT の離しを飲み込んでしまう。
                                    if let Err(e) = request_finalize(&app) {
                                        log::error!("録音を確定できません: {e}");
                                        emit_error(&app, &e);
                                        set_status(&app, Status::Idle, Some(e));
                                    }
                                }
                                None => {}
                            }

                            // 取りこぼしがあれば可視化する (通常は 0 のまま)。
                            let dropped = hotkey::dropped_events();
                            if dropped > seen_drops {
                                log::warn!(
                                    "ホットキーイベントを {} 件取りこぼしました (累計 {dropped})",
                                    dropped - seen_drops
                                );
                                seen_drops = dropped;
                            }
                        }
                    },
                    recv(limit_rx) -> msg => {
                        if msg.is_err() {
                            continue;
                        }
                        // 上限で止めた録音の用途は問わない。走っているのは 1 本だけ。
                        let slot = active_mode.unwrap_or(HotkeyMode::Inject).slot();
                        handle_length_limit(&app, &mut interpreters[slot]);
                        active_mode = None;
                    },
                }
            }
            log::info!("ホットキーコントローラを終了");
        });

    if let Err(e) = spawned {
        let msg = format!("ホットキー処理スレッドの起動に失敗しました: {e}");
        log::error!("{msg}");
        emit_error(app, &msg);
    }
}

/// 録音を開始する。前景ウィンドウの確定を最優先で行う。
fn start_recording(app: &AppHandle, mode: HotkeyMode) -> Result<(), String> {
    let state = app.state::<AppState>();

    let mut slot = state
        .recorder
        .lock()
        .map_err(|_| "録音状態のロックが毒化しました".to_string())?;
    if slot.is_some() {
        return Err("すでに録音中です".to_string());
    }

    let cfg = state.config.snapshot();

    // 前回の録音が残した上限通知を捨てる。取りこぼすと、次の録音が
    // 開始直後に「上限到達」で止められてしまう。
    // (この関数はコントローラスレッド専用なので、受信の競合は起きない)
    while state.limit_rx.try_recv().is_ok() {}

    // デバイス初期化には数十 ms かかりうるので、その前に前景を押さえる。
    // ここで採った HWND が M3 の挿入先照合 (R7) の基準になる。
    let target = foreground::capture_foreground();

    let recorder = audio::start(state.limit_tx.clone()).map_err(|e| e.to_string())?;
    let started_at = recorder.started_at();
    let meter = recorder.meter();

    // 合図は録音が実際に始まってから鳴らす。開始に失敗したのに鳴らすと、
    // 「鳴った = 録音できている」という信頼が崩れる (ペダル運用では
    // この音だけが唯一の手がかり)。再生は別スレッドなので待たない。
    sound::play(&cfg.start_sound_choice(), cfg.effective_sound_volume());

    log::info!(
        "録音開始 [{}] (挿入先: {} / hwnd=0x{:X} / \"{}\")",
        mode.label(),
        target.process_name,
        target.hwnd,
        target.window_title
    );
    if !target.is_known() && mode == HotkeyMode::Inject {
        log::warn!("前景ウィンドウを特定できませんでした。挿入時の照合は行えません");
    }

    *slot = Some(recorder);
    drop(slot);

    // 画面コンテキストは録音開始の瞬間の画面を見る必要がある。
    // 取得は短命スレッド + 打ち切りつきなので、録音を待たせるのは最大 300ms。
    // 無効なら即座に空が返る。
    //
    // **画面質問モードでは取らない。** あちらは発話を整形しないので
    // deep context を渡す先が無く、取っても捨てるだけ。捨てる値のために
    // 録音開始を 300ms 待たせるのは、この用途では二重に無駄になる
    // (同じ画面をこの直後にモニタ単位で読む)。
    let screen_context = context::capture(cfg.deep_context && mode != HotkeyMode::ScreenAsk);
    if !screen_context.is_empty() {
        log::info!(
            "画面コンテキストを取得: {} ({} 文字)",
            screen_context.source.label(),
            screen_context.text.chars().count()
        );
    }

    // 画面質問モードのときだけ、モニタ 1 枚分の走査を**始める**。
    //
    // ここで待たないのが要点。deep context は 300ms 待つが、こちらは
    // 複数ウィンドウを読むうえスクリーンショットも撮るので、待てば秒単位に
    // なる。ユーザーが最も嫌うのは「話し始めるまで待たされる」こと
    // (design.md の設計原則) なので、走査は録音と並行させ、回収は
    // 後処理ワーカー — つまり STT を待っている時間の裏 — で行う。
    //
    // 用途で分岐しているので、`screen_ask_enabled` が false のときは
    // そもそもこのキーが割り当てられておらず、ここへは来ない。それでも
    // 設定を二重に見るのは、ホットキーの反映漏れがあっても
    // **画面が送られないほうへ倒す**ため。
    let screen = if mode == HotkeyMode::ScreenAsk {
        screen::start_scan(cfg.screen_ask_enabled)
    } else {
        None
    };

    if let Ok(mut pending) = state.pending.lock() {
        *pending = Some(PendingRecording {
            target,
            started_at,
            mode,
            context: screen_context,
            screen,
        });
    }

    set_status(app, Status::Recording, None);
    // ここから録音中。キャンセルキーを武装する (停止系の全経路で解除する)。
    hotkey::set_recording_active(true);

    // 表示とレベル送出は録音開始の後。ここで待たせると最初の一言が削れる。
    // 小窓を出さないならレベルを送る相手もいない。
    if cfg.overlay_enabled {
        overlay::show(app);
        start_level_emitter(app, meter);
    }
    Ok(())
}

/// 入力レベルを間引いてフロントへ送るスレッドを起動する。
///
/// 音声コールバックから直接送らないのは、1 秒に何百回も IPC を叩くと
/// WebView 側が詰まり、コールバックの時間予算も食うため。
///
/// 世代を進めてから起動するので、前の録音のスレッドはこの時点で終了へ向かう。
fn start_level_emitter(app: &AppHandle, meter: std::sync::Arc<audio::LevelMeter>) {
    use std::sync::atomic::Ordering;

    let state = app.state::<AppState>();
    let generation = state.level_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let current = std::sync::Arc::clone(&state.level_generation);
    let app = app.clone();

    let spawned = thread::Builder::new()
        .name("nox-level".to_string())
        .spawn(move || {
            // 自分が最新の世代である間だけ送る。
            while current.load(Ordering::SeqCst) == generation {
                if let Err(e) = app.emit(EVENT_LEVEL, meter.display_level()) {
                    log::debug!("入力レベルの送出に失敗: {e}");
                    break;
                }
                std::thread::sleep(LEVEL_EMIT_INTERVAL);
            }
            // 自分が最後の送り手なら、0 を送ってメーターを畳む。
            if current.load(Ordering::SeqCst) == generation {
                let _ = app.emit(EVENT_LEVEL, 0.0f32);
            }
        });

    if let Err(e) = spawned {
        log::warn!("入力レベルの送出スレッドを起動できません: {e}");
    }
}

/// レベル送出を止める (世代を進めるだけ)。
fn stop_level_emitter(app: &AppHandle) {
    use std::sync::atomic::Ordering;
    let state = app.state::<AppState>();
    state.level_generation.fetch_add(1, Ordering::SeqCst);
    // メーターを 0 に畳む。走っていたスレッドは世代違いで黙って終わる。
    let _ = app.emit(EVENT_LEVEL, 0.0f32);
}

/// 録音を止め、後処理をワーカーへ引き渡す。
///
/// **この関数は必ず軽いままにすること。** 呼び出し元はホットキーの
/// コントローラスレッドであり、ここでブロックすると次のキー操作の反映が遅れる。
/// 行うのはロックの取得・take・チャネル送信だけ。
fn request_finalize(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();

    let recorder = {
        let mut slot = state
            .recorder
            .lock()
            .map_err(|_| "録音状態のロックが毒化しました".to_string())?;
        slot.take()
    };
    let Some(recorder) = recorder else {
        return Err("録音していません".to_string());
    };
    // 通常停止の時点で録音は終わった扱い。キャンセルキーを非武装へ戻す。
    hotkey::set_recording_active(false);

    let pending = state
        .pending
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| PendingRecording {
            target: TargetWindow::unknown(),
            started_at: recorder.started_at(),
            // 開始情報を取り落とした異常系。貼り付け側に倒す
            // (クリップボードのみへ倒すと、貼られるはずの結果が黙って消える)。
            mode: HotkeyMode::Inject,
            context: context::ScreenContext::default(),
            screen: None,
        });

    stop_level_emitter(app);
    set_status(app, Status::Processing, None);

    state
        .finalize_tx
        .send(WorkerJob::Finalize(Box::new(FinalizeJob {
            recorder,
            target: pending.target,
            started_at: pending.started_at,
            mode: pending.mode,
            context: pending.context,
            screen: pending.screen,
        })))
        .map_err(|_| "後処理ワーカーが停止しています".to_string())
}

/// 録音を破棄する。WAV 化も転写も履歴記録も行わない。
///
/// **この関数は必ず軽いままにすること** ([`request_finalize`] と同じ制約)。
/// 呼び出し元はホットキーのコントローラスレッドであり、ここでブロックすると
/// 次のキー操作の反映が遅れる。行うのはロックの取得・take・drop・チャネルの
/// 読み捨てだけ。重い後始末は何もない — 破棄なので残すものがない。
fn cancel_recording(app: &AppHandle) {
    // 先に非武装へ戻す。以降のキャンセルキー入力は通常のキーとして流れる。
    hotkey::set_recording_active(false);

    let state = app.state::<AppState>();
    if !discard_recording(&state.recorder, &state.pending, &state.limit_rx) {
        return; // 古いイベント (既に停止している)。黙って捨てる。
    }

    stop_level_emitter(app);
    // 破棄したことは音でも伝える。画面を見ていない運用では、
    // 「取り消せたのか / まだ録っているのか」が音以外に分からない。
    let cfg = app.state::<AppState>().config.snapshot();
    sound::play(&cfg.cancel_sound_choice(), cfg.effective_sound_volume());
    log::info!("録音をキャンセルした");
    set_status_from(
        app,
        Status::Idle,
        Some("録音をキャンセルしました".into()),
        StatusOrigin::Recording,
    );
    // 小窓は idle への遷移では自分で畳まない (overlay.ts 参照)。
    // 結果イベントも飛ばないので、エラー表示と同じ対で畳みを予約する。
    overlay::hide_after(app, OVERLAY_ERROR_LINGER);
}

/// 録音状態を破棄する。WAV を作らずストリームだけ止める。破棄したら `true`。
///
/// [`AppHandle`] に触れない状態操作として分離してある。テストからは
/// `AppState` を丸ごと組み立てずにこの単位で検証できる (テストバイナリが
/// tauri のウィンドウ系コードをリンクすると、マニフェスト無しで起動しなくなる)。
fn discard_recording(
    recorder_slot: &Mutex<Option<Recorder>>,
    pending_slot: &Mutex<Option<PendingRecording>>,
    limit_rx: &Receiver<()>,
) -> bool {
    let recorder = recorder_slot.lock().ok().and_then(|mut slot| slot.take());
    let Some(recorder) = recorder else {
        return false; // 古いイベント (既に停止している)。
    };
    // drop でストリーム停止 (audio.rs の「drop で回収」パターン)。
    // WAV を生成しないので音声はここで消える — それがキャンセルの意味。
    drop(recorder);

    // 挿入先情報も録音と一緒に捨てる。
    if let Ok(mut slot) = pending_slot.lock() {
        *slot = None;
    }

    // 上限監視の通知が滞留していれば捨てる。残っていると次の録音が
    // 開始直後に「上限到達」で止められてしまう (start_recording と同じ掃除)。
    while limit_rx.try_recv().is_ok() {}
    true
}

/// ファイナライズワーカー本体。
///
/// リサンプル + WAV 化という秒単位になりうる処理をここで行う。
/// M2 以降の STT・整形・注入もこのスレッドに載せる。
/// panic を捕まえて `None` にする。
///
/// ワーカーは 1 件の不具合で死んではいけない。死んだあとも
/// [`crossbeam_channel::Sender::send`] は成功し続けるため、以後の録音は
/// 誰にも処理されずキューに溜まり、UI は「処理中」のまま固まる —
/// つまり**無言のデータ消失が恒久化する**。ジョブ単位で捕まえて生き延びる。
fn guard_panic<T>(context: &str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(payload) => {
            let detail = panic_message(&payload);
            log::error!("{context} で内部エラー (panic) が発生しました: {detail}");
            None
        }
    }
}

/// panic のペイロードから読めるメッセージを取り出す。
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "詳細不明".to_string()
    }
}

fn finalize_worker(app: AppHandle, rx: Receiver<WorkerJob>) {
    while let Ok(job) = rx.recv() {
        match job {
            WorkerJob::Finalize(job) => process_job(&app, *job),
            WorkerJob::EnforceRetention => {
                let days = app
                    .state::<AppState>()
                    .config
                    .snapshot()
                    .history_retention_days;
                enforce_retention(&app, days);
            }
            WorkerJob::Retranscribe { id } => {
                // 録音中なら「録音中」表示を奪わない (再転写は裏方の作業)。
                // 出どころを Background にして、オーバーレイには映さない
                // (映すと完了イベントが来ず「認識中…」で固まる)。
                if !app.state::<AppState>().is_recording() {
                    set_status_from(&app, Status::Processing, None, StatusOrigin::Background);
                }
                if guard_panic("再転写", || retranscribe(&app, id)).is_none() {
                    emit_background_error(&app, "再転写中に内部エラーが発生しました");
                }
                // panic しても進行中の印は必ず外す (二度と再転写できなくなる)。
                if let Ok(mut running) = app.state::<AppState>().retranscribing.lock() {
                    running.remove(&id);
                }
            }
        }

        // 次の録音が既に始まっているなら Idle へ戻さない。
        // 無条件に戻すと、録音 B の最中に録音 A の後処理が終わった瞬間、
        // 表示が「待機中」に化ける。
        if !app.state::<AppState>().is_recording() {
            set_status(&app, Status::Idle, None);
            // 小窓は結果を少し見せてから自分で消える (overlay.ts 側)。
            // ここでは録音が続いていないことだけ確かめる。
        }
    }
    log::info!("ファイナライズワーカーを終了");
}

/// ジョブ 1 件を処理する。**どんな失敗でもワーカーを殺さない。**
fn process_job(app: &AppHandle, job: FinalizeJob) {
    // 第 1 段: WAV 化。ここで panic すると音声は救えないので、
    // せめてワーカーを生かして次の録音を処理できるようにする。
    let FinalizedRecording {
        recording,
        context: screen_context,
        screen,
    } = match guard_panic("録音の WAV 化", || finalize_one(job)) {
        Some(Ok(finalized)) => finalized,
        Some(Err(e)) => {
            log::error!("録音を確定できません: {e}");
            emit_error(app, &e);
            return;
        }
        None => {
            emit_error(
                app,
                "録音の WAV 化中に内部エラーが発生しました (録音は失われました)",
            );
            return;
        }
    };

    let summary = recording.summary();
    if let Err(e) = app.emit(EVENT_SESSION, &summary) {
        log::warn!("録音メタ情報イベントの送出に失敗: {e}");
    }

    // 第 2 段: 転写と整形 (画面質問モードでは転写と回答)。
    // ここで panic しても WAV は手元にあるので退避できる。
    let stage_name = if recording.mode == HotkeyMode::ScreenAsk {
        "画面質問"
    } else {
        "転写・整形"
    };
    if guard_panic(stage_name, || {
        if recording.mode == HotkeyMode::ScreenAsk {
            answer_screen_question(app, &recording, screen);
        } else {
            transcribe_and_format(app, &recording, &screen_context);
        }
    })
    .is_none()
    {
        let dir = app.state::<AppState>().failed_dir.clone();
        let saved = save_failed_recording(&dir, &recording, "後処理中に内部エラー (panic)");
        let notice = match saved {
            Some(path) => format!(
                "転写中に内部エラーが発生しました。録音は {} に保存しました",
                path.display()
            ),
            None => "転写中に内部エラーが発生し、録音の退避にも失敗しました".to_string(),
        };
        emit_error(app, &notice);
    }

    if let Ok(mut slot) = app.state::<AppState>().last_session.lock() {
        *slot = Some(recording);
    }
}

/// 1 件の録音を WAV 化し、[`RecordingSession`] を確定させる。
fn finalize_one(job: FinalizeJob) -> Result<FinalizedRecording, String> {
    let FinalizeJob {
        recorder,
        target,
        started_at,
        mode,
        context,
        screen,
    } = job;
    let device_name = recorder.device_name().to_string();
    let limit_reached = recorder.limit_reached();

    let (wav_bytes, duration) = audio::finish(recorder).map_err(|e| e.to_string())?;

    let recording = RecordingSession {
        wav_bytes,
        sample_rate: audio::TARGET_SAMPLE_RATE,
        target,
        started_at,
        duration,
        mode,
    };

    log::info!(
        "録音確定: {} bytes / {:.2} 秒 / {} Hz / 挿入先={} (hwnd=0x{:X}) / device=\"{}\"{}",
        recording.wav_bytes.len(),
        recording.duration.as_secs_f64(),
        recording.sample_rate,
        recording.target_process(),
        recording.target_hwnd(),
        device_name,
        if limit_reached {
            " [長さ上限で打ち切り]"
        } else {
            ""
        },
    );
    Ok(FinalizedRecording {
        recording,
        context,
        screen,
    })
}

/// WAV を転写し、必要なら整形して結果を通知する。
///
/// - 整形の失敗は [`pipeline::run`] が生転写へ落とす (R2 劣化モード)。
/// - **STT の失敗は劣化できない**ので、WAV を退避してから通知する
///   (M4 の履歴 DB が入るまでの暫定措置 / R4 の趣旨)。
fn transcribe_and_format(
    app: &AppHandle,
    recording: &RecordingSession,
    screen_context: &context::ScreenContext,
) {
    let state = app.state::<AppState>();
    let Some(http) = state.http.clone() else {
        let msg = "HTTP クライアントを構築できなかったため転写できません".to_string();
        log::error!("{msg}");
        emit_error(app, &msg);
        save_failed_recording(&state.failed_dir, recording, &msg);
        return;
    };

    let cfg = state.config.snapshot();
    let started = Instant::now();

    // 整形が有効なら、キーが無くても Formatter を作る。
    // そうすることで「キー未設定」が Disabled ではなく
    // RawFallback(理由つき) として UI に出る。
    let formatter = cfg.formatting_enabled.then(|| {
        GeminiFormatter::new(
            http,
            cfg.gemini_url(),
            cfg.gemini_key().secret.unwrap_or_default(),
        )
        // 共用クライアントの 60 秒より短く見切る。整形は待たせた分だけ
        // 体感を損ねる上、落ちても R2 で生転写に落ちるだけなので。
        .with_timeout(format::FORMAT_TIMEOUT)
    });

    // 挿入先に合う文体を選ぶ。
    let profile = style::match_profile(
        &cfg.style_profiles,
        recording.target_process(),
        &recording.target.window_title,
    );
    // 当たらなかったことも残す。「プロファイルが無いアプリ」を数える
    // には、当たった記録だけでは足りない。
    let style_label = style::history_label(profile);
    match profile {
        Some(profile) => log::info!(
            "スタイルプロファイルを適用: {} ({}) → {}",
            profile.process,
            if profile.id.is_empty() { "ユーザー" } else { &profile.id },
            profile.instruction.chars().take(30).collect::<String>()
        ),
        None => log::info!(
            "スタイルプロファイルは未一致: {}",
            recording.target_process()
        ),
    }

    let entries = cfg.dictionary_entries();
    let input = pipeline::PipelineInput {
        wav: &recording.wav_bytes,
        language: &cfg.language,
        dictionary: &entries,
        style: profile.map(|p| p.instruction.as_str()),
        // 前景が取れなかった録音では "<unknown>" が入る。
        // それをアプリ名としてプロンプトへ載せても意味が無い。
        app: recording
            .target
            .is_known()
            .then(|| recording.target_process()),
        context: screen_context.as_prompt_text(),
    };
    let formatter_ref = formatter.as_ref().map(|f| f as &dyn format::TextFormatter);

    let outcome = run_stt_pipeline(app, &cfg, &input, formatter_ref);

    match outcome {
        Ok(result) => {
            let mut payload = ResultPayload {
                degraded: result.outcome.is_degraded(),
                raw_text: result.raw_text,
                text: result.text,
                outcome: result.outcome,
                stt_ms: result.stt_ms,
                format_ms: result.format_ms,
                total_ms: started.elapsed().as_millis() as u64,
                target_process: recording.target_process().to_string(),
                target_hwnd: recording.target_hwnd(),
                duration_ms: recording.duration.as_millis() as u64,
                injected: false,
                inject_outcome: InjectOutcome::Disabled,
                clipboard_state: ClipboardState::Untouched,
                lost_clipboard_formats: Vec::new(),
                style_profile: Some(style_label.clone()),
            };

            // 本文はユーザーの発話そのものなので info には出さない。
            // 長さと所要時間だけ記録する。
            log::info!(
                "転写完了: 生 {} 文字 / 採用 {} 文字 / STT {} ms / 整形 {} ms / 計 {} ms{}",
                payload.raw_text.chars().count(),
                payload.text.chars().count(),
                payload.stt_ms,
                payload.format_ms,
                payload.total_ms,
                if payload.degraded {
                    " [劣化モード: 生転写を採用]"
                } else {
                    ""
                },
            );
            if let FormatOutcome::RawFallback { reason } = &payload.outcome {
                log::warn!("整形をスキップしました: {reason}");
            }

            if let Ok(mut slot) = state.last_result.lock() {
                *slot = Some(payload.clone());
            }

            // --- R4: 注入より先に永続化する ---
            //
            // 注入は最も壊れやすい工程 (フォーカスが変わる・クリップボードを
            // 奪われる・昇格アプリに弾かれる)。そこで落ちたときに発話が
            // 消えるのが最悪なので、書いてから注入する。
            // DB が書けなくてもパイプラインは止めず、WAV 退避に落とす。
            let history_id = record_history(app, &cfg, recording, &payload);

            apply_injection(app, &cfg, recording, &mut payload);
            payload.total_ms = started.elapsed().as_millis() as u64;

            // 注入の結果を後から書き足す。
            if let Some(id) = history_id {
                let history = &app.state::<AppState>().history;
                if let Err(e) = history.update_injection(
                    id,
                    &format!("{:?}", payload.inject_outcome),
                    &format!("{:?}", payload.clipboard_state),
                ) {
                    // 貼付が不達だった行を後から探せなくなるので黙らない。
                    log::warn!("履歴へ注入結果を書けません: {e}");
                    emit_error(
                        app,
                        &format!("履歴へ貼り付け結果を記録できませんでした: {e}"),
                    );
                }
            }

            if let Ok(mut slot) = state.last_result.lock() {
                *slot = Some(payload.clone());
            }
            if let Err(e) = app.emit(EVENT_RESULT, &payload) {
                log::warn!("結果イベントの送出に失敗: {e}");
            }
            // 結果を少し見せてから畳む。次の録音で表示が更新されれば、
            // このタイマーは世代違いで何もしない。
            overlay::hide_after(app, OVERLAY_RESULT_LINGER);
            emit_history_changed(app);
            // 常駐したままでも保持期限が守られるよう、録音のたびに執行する。
            enforce_retention(app, cfg.history_retention_days);
        }
        Err(e) => {
            let msg = e.to_string();
            log::error!("転写に失敗しました: {msg}");
            // 失敗も音で伝える。画面を見ていないと、結果が来ないことと
            // 失敗したことの区別が付かない。
            sound::play(&cfg.cancel_sound_choice(), cfg.effective_sound_volume());
            let saved = save_failed_recording(&state.failed_dir, recording, &msg);

            // 失敗した「その場で」履歴に未転写行を作る。起動時の取り込み任せに
            // すると、再転写の導線が次回起動まで出てこない — 常駐運用では
            // それが一番必要な瞬間に一番見えない、という状態になる。
            let listed = match &saved {
                Some(path) if cfg.history_enabled => {
                    record_untranscribed(app, recording, path, &msg)
                }
                _ => false,
            };

            let notice = match (&saved, listed) {
                (Some(_), true) => {
                    format!("{msg}\n録音は履歴に残しました。履歴から再転写できます")
                }
                (Some(path), false) => {
                    format!("{msg}\n録音は {} に保存しました", path.display())
                }
                (None, _) => format!("{msg}\n※録音の退避にも失敗しました"),
            };
            notify(app, Notice::ActionRequired, &notice);
        }
    }
}

/// 画面についての質問に答え、答えをクリップボードへ入れる。
///
/// # ここは音声入力ではない
///
/// 他の 2 用途は「発話を整えて届ける」経路だが、この用途では
/// **発話は届けるものではなく問い**であり、届くのは画面を読んだ答えである。
/// 違いは全部ここから出てくる:
///
/// - **文体プロファイルを適用しない**。「Slack へ貼るので砕けた口調で」を
///   答えに適用したら、一覧が挨拶付きの雑談になる。そもそも貼り付け先が
///   決まっていない (出力はクリップボード)
/// - **整形パイプラインを通さない**。整形の仕事は「言い直しを畳んで句読点を
///   打つ」ことで、質問文にそれをしても意味が無いうえ 1 往復ぶん遅くなる。
///   STT の生転写をそのまま問いとして使う
/// - **成功した質問と回答は履歴に残さない**。回答は画面の内容そのものなので、
///   残せば「画面の内容を履歴に残さない」という約束 ([`screen`] のモジュール
///   doc) が破れる。質問文も画面の語を含みがちなので同じ扱いにする
/// - **失敗してもクリップボードに触れない**。エラー文をクリップボードへ
///   入れると、ユーザーはそれを貼り付ける。元の内容を壊さずに黙って
///   引き下がり、理由はトーストで言う
///
/// # 唯一の例外: STT が失敗したときの音声
///
/// 質問を**聞き取れなかった**場合だけは、他の用途と同じく WAV を
/// `failed/` へ退避する。退避 WAV は起動時に履歴の未転写行として
/// 取り込まれるので、**再転写すれば質問の文面が履歴 DB に載る**。
///
/// これを承知で残しているのは、外す方が壊すものが大きいから:
///
/// - 取り込みだけを除外すると、**どの DB 行も所有しない WAV** が
///   `failed/` に溜まり続ける。design.md M4 の所有権原則が名指しで
///   戒めている状態で、「すべて削除」が嘘になりディスクは無限に増える
/// - 退避そのものをやめると、この機能 1 つの都合で R4 (発話データ保全)
///   という全体の不変条件を曲げることになる。しかも聞き取り失敗は
///   環境が悪いときに起きやすく、そこで言い直しを強いるのは体験が悪い
///
/// 守りたかった中心は保たれている: **画面から読んだ資料も、それを元にした
/// 回答も、履歴には一切入らない。** 入りうるのは「聞き取れなかった
/// ユーザー自身の音声」だけで、それは画面の内容ではない。
/// 退避メタには `hotkey_mode` を書いてあるので、方針を変えるならそこを
/// 取り込み側で見ればよい。
fn answer_screen_question(
    app: &AppHandle,
    recording: &RecordingSession,
    scan: Option<screen::ScanHandle>,
) {
    let state = app.state::<AppState>();
    let cfg = state.config.snapshot();
    let started = Instant::now();

    // --- 質問の書き起こし ---
    //
    // **走査の回収より先に行う。** 走査は録音開始と同時に始まっていて、
    // 残り予算の分だけ待てる。ここで先に待つと、その待ち時間が STT と
    // **直列**に乗る — 短い発話 (録音 1〜2 秒) では走査がまだ終わって
    // いないので、まるまる体感待ち時間になる。STT を先に回せば、その
    // 1 秒前後の裏で走査が進み、たいていは待ち時間ゼロで回収できる。
    //
    // formatter に None を渡すので整形は走らない ([`pipeline::run`])。
    // 辞書だけは渡す — 固有名詞を取り違えると質問そのものが変わる。
    let entries = cfg.dictionary_entries();
    let input = pipeline::PipelineInput {
        wav: &recording.wav_bytes,
        language: &cfg.language,
        dictionary: &entries,
        style: None,
        app: None,
        context: None,
    };
    let transcript = match run_stt_pipeline(app, &cfg, &input, None) {
        Ok(result) => result,
        Err(e) => {
            let msg = format!("質問を聞き取れませんでした: {e}");
            log::error!("{msg}");
            sound::play(&cfg.cancel_sound_choice(), cfg.effective_sound_volume());
            // 音声だけは救う。**画面の資料はここで捨てる** — 残す先が無いし、
            // 残してよいものでもない。
            let saved = save_failed_recording(&state.failed_dir, recording, &msg);
            // 「履歴に残らないはずの機能なのに録音が残っている」と読めては
            // 困るので、残したことと理由をその場で言う (この関数の doc の
            // 「唯一の例外」)。
            let notice = match saved {
                Some(_) => format!(
                    "{msg}
録音だけは失われないよう退避しました (画面の内容は残していません)"
                ),
                None => format!("{msg}
※録音の退避にも失敗しました"),
            };
            notify(app, Notice::ActionRequired, &notice);
            return;
        }
    };
    // --- 資料の回収 ---
    //
    // STT が終わった時点で、走査はほぼ確実に終わっている。
    // 終わっていなくても残り予算の分しか待たない。
    let scan = match scan {
        Some(handle) => handle.wait(),
        None => screen::ScreenScan::failed(
            "画面の走査を開始できませんでした (前の走査がまだ終わっていない可能性があります)",
        ),
    };

    let question = transcript.raw_text.trim().to_string();
    if question.is_empty() {
        let msg = "質問が聞き取れませんでした (無音だった可能性があります)";
        log::warn!("{msg}");
        sound::play(&cfg.cancel_sound_choice(), cfg.effective_sound_volume());
        notify(app, Notice::ActionRequired, msg);
        return;
    }

    // --- 資料が無いなら、黙って空を返さない (design.md「0 件と欠測を混同しない」) ---
    if !scan.has_material() {
        let reason = scan
            .failure
            .unwrap_or_else(|| "画面から読み取れる内容がありませんでした".to_string());
        let msg = format!("画面を読み取れなかったため質問に答えられません: {reason}");
        log::warn!("{msg}");
        sound::play(&cfg.cancel_sound_choice(), cfg.effective_sound_volume());
        notify(app, Notice::ActionRequired, &msg);
        return;
    }

    // --- 質問する ---
    let Some(http) = state.http.clone() else {
        let msg = "HTTP クライアントを構築できなかったため質問できません";
        log::error!("{msg}");
        notify(app, Notice::ActionRequired, msg);
        return;
    };
    let asker = GeminiFormatter::new(
        http,
        cfg.gemini_url(),
        cfg.gemini_key().secret.unwrap_or_default(),
    )
    .with_timeout(SCREEN_ASK_TIMEOUT);

    let windows: Vec<format::AskWindow<'_>> = scan
        .windows
        .iter()
        .map(|w| format::AskWindow {
            title: &w.title,
            process: &w.process,
            position: &w.position,
            text: &w.text,
        })
        .collect();
    let images: Vec<format::AskImage<'_>> = scan
        .screenshot
        .iter()
        .map(|shot| format::AskImage {
            mime: "image/png",
            bytes: &shot.png,
        })
        .collect();

    let ask_started = Instant::now();
    let answer = asker.ask(&format::AskRequest {
        question: &question,
        monitor: scan.monitor.label(),
        windows: &windows,
        images: &images,
    });
    let ask_ms = ask_started.elapsed().as_millis() as u64;

    let answer = match answer {
        Ok(answer) => answer,
        Err(e) => {
            // 劣化先が無い。生転写 (= 質問文) をクリップボードへ入れても
            // ユーザーが欲しかったものではないので、何もしないで理由を言う。
            let msg = format!("画面についての質問に答えられませんでした: {e}");
            log::error!("{msg}");
            sound::play(&cfg.cancel_sound_choice(), cfg.effective_sound_volume());
            notify(app, Notice::ActionRequired, &msg);
            return;
        }
    };

    // --- 答えを届ける ---
    let report = inject::copy_only(&answer);
    log::info!(
        "画面質問完了: 質問 {} 文字 / 回答 {} 文字 / STT {} ms / 回答 {ask_ms} ms / 計 {} ms / {:?}",
        question.chars().count(),
        answer.chars().count(),
        transcript.stt_ms,
        started.elapsed().as_millis(),
        report.outcome,
    );
    if let Some(message) = inject::lost_formats_message(&report.lost_formats) {
        log::warn!("{message}");
        notify(app, Notice::ActionRequired, &message);
    }
    if let Some(message) = &report.message {
        notify(app, Notice::ActionRequired, message);
    }

    let payload = ResultPayload {
        // R5 の並置と同じ枠を使う。ここでの「生」は質問、「採用」は答え。
        raw_text: question,
        text: answer,
        outcome: FormatOutcome::Formatted,
        degraded: false,
        stt_ms: transcript.stt_ms,
        format_ms: ask_ms,
        total_ms: started.elapsed().as_millis() as u64,
        target_process: recording.target_process().to_string(),
        target_hwnd: recording.target_hwnd(),
        duration_ms: recording.duration.as_millis() as u64,
        injected: report.injected,
        inject_outcome: report.outcome,
        clipboard_state: report.clipboard_state,
        lost_clipboard_formats: report.lost_formats,
        // 画面質問モードは文体プロファイルを通さない (答えは Gemini が
        // 直接書く)。未一致 ("") ではなく未記録 (None) が正しい。
        style_profile: None,
    };

    // **履歴 DB には書かない** (この関数の doc 参照)。
    // メモリ上の直近結果には置く — 画面に出さないと「何も起きなかった」
    // ように見えるうえ、履歴に無い以上ここが唯一の再確認手段になる。
    if let Ok(mut slot) = state.last_result.lock() {
        *slot = Some(payload.clone());
    }
    if let Err(e) = app.emit(EVENT_RESULT, &payload) {
        log::warn!("結果イベントの送出に失敗: {e}");
    }
    overlay::hide_after(app, OVERLAY_RESULT_LINGER);
}

/// 採用テキストを前景アプリへ注入し、結果を `payload` に反映する。
///
/// 中止・失敗はいずれも致命ではない。整形テキストはクリップボードか
/// 画面に残るので、ユーザーは手で貼り付けられる (design.md R4)。
fn apply_injection(
    app: &AppHandle,
    cfg: &config::Config,
    recording: &RecordingSession,
    payload: &mut ResultPayload,
) {
    // クリップボードのみモード: 前景の照合をせずコピーだけして終える。
    //
    // ここを `injection_enabled == false` と同じ扱いにしてはいけない。
    // あちらは「画面からコピーしてください」で終わるが、こちらは
    // **クリップボードに入っていることが結果**であり、それを payload と
    // 履歴に残さないと UI が「何も起きなかった」ように見える。
    if recording.mode == HotkeyMode::ClipboardOnly {
        let report = inject::copy_only(&payload.text);
        log::info!(
            "クリップボードのみモード: {:?} ({:?})",
            report.outcome,
            report.clipboard_state
        );
        if let Some(message) = inject::lost_formats_message(&report.lost_formats) {
            log::warn!("{message}");
            notify(app, Notice::ActionRequired, &message);
        }
        if let Some(message) = &report.message {
            notify(app, Notice::ActionRequired, message);
        }
        payload.injected = report.injected;
        payload.inject_outcome = report.outcome;
        payload.clipboard_state = report.clipboard_state;
        payload.lost_clipboard_formats = report.lost_formats;
        return;
    }

    if !cfg.injection_enabled {
        log::info!("設定により自動貼り付けは無効です");
        payload.inject_outcome = InjectOutcome::Disabled;
        return;
    }

    let report = inject::inject(
        &payload.text,
        InjectTarget::new(recording.target_hwnd(), recording.target.process_id),
        cfg.clipboard_policy(),
    );

    log::info!(
        "注入結果: {:?} (送出={} / クリップボード={:?})",
        report.outcome,
        report.injected,
        report.clipboard_state,
    );

    // R6: 画像やファイルが失われたことは、注入の成否とは別に必ず伝える。
    if let Some(message) = inject::lost_formats_message(&report.lost_formats) {
        log::warn!("{message}");
        notify(app, Notice::ActionRequired, &message);
    }
    // 中止・失敗の理由と復旧方法を伝える。
    if let Some(message) = &report.message {
        let level = if report.needs_user_action() {
            Notice::ActionRequired
        } else {
            Notice::Informational
        };
        notify(app, level, message);
    }

    payload.injected = report.injected;
    payload.inject_outcome = report.outcome;
    payload.clipboard_state = report.clipboard_state;
    payload.lost_clipboard_formats = report.lost_formats;
}

/// 通知の重さ。
///
/// このアプリはトレイ常駐で使うので、**メインウィンドウは普段閉じている**。
/// WebView へのイベントだけでは誰も見ない。ユーザーが動かないと発話が
/// 失われる/何かが壊れたまま気づかれない類のものは OS トーストにも出す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notice {
    /// ユーザーの操作が要る (手で Ctrl+V する、失われたものに気づく)。
    /// トースト + イベント。
    ActionRequired,
    /// 見えていれば役に立つが、見逃しても損はない。イベントのみ。
    Informational,
}

/// ユーザーへの通知。重さに応じて OS トーストを併用する。
fn notify(app: &AppHandle, level: Notice, message: &str) {
    emit_error(app, message);
    if level != Notice::ActionRequired {
        return;
    }
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title("nox-voice")
        .body(message)
        .show()
    {
        // トーストが出せなくてもイベントは出ているので致命ではない。
        log::warn!("通知を表示できません: {e}");
    }
}

/// 設定に従って STT を選び、パイプラインを回す。
///
/// **新規録音と再転写の両方がここを通る。** 片方だけが設定を見ていないと、
/// 「ローカルのみ」を選んだ人の音声がクラウドへ出てしまう
/// (実際に再転写だけがモードを見ておらず、そうなっていた)。
fn run_stt_pipeline(
    app: &AppHandle,
    cfg: &config::Config,
    input: &pipeline::PipelineInput<'_>,
    formatter: Option<&dyn format::TextFormatter>,
) -> Result<pipeline::PipelineResult, stt::SttError> {
    let plan = local_stt::EnginePlan::from_mode(cfg.local_stt_mode);

    if !plan.use_cloud {
        // ローカルのみ: クラウドのクライアントを**作りもしない**。
        log::info!("ローカルのみモードのため、音声はクラウドへ送りません");
        return run_with_local_stt(app, input, formatter, None);
    }

    let Some(http) = app.state::<AppState>().http.clone() else {
        return Err(stt::SttError::Network(
            "HTTP クライアントを構築できませんでした".to_string(),
        ));
    };
    let stt_client = GroqStt::new(
        http,
        &cfg.groq_endpoint,
        &cfg.stt_model,
        // キーが無い場合も同じ経路を通す。GroqStt が MissingApiKey を返す。
        cfg.groq_key().secret.unwrap_or_default(),
    );

    match pipeline::run(input, &stt_client, formatter) {
        Err(e) if plan.use_local && local_stt::should_fall_back(&e) => {
            log::warn!("Groq が使えないためローカル認識へ切り替えます: {e}");
            notify(
                app,
                Notice::Informational,
                "クラウド認識が使えないため、ローカルで認識します (時間がかかります)",
            );
            run_with_local_stt(app, input, formatter, Some(&e))
        }
        other => other,
    }
}

/// ローカルモデルで転写してからパイプラインの続きを回す。
///
/// `cloud_error` は「なぜローカルへ来たか」(`None` はローカルのみモード)。
/// ローカルも使えない場合は、**元のクラウド側の失敗を返す** —
/// 「モデルが無い」より「ネットワークが繋がらない」の方が効く情報だから。
/// ローカルのみモードでは Groq のキーの話をしない (誤誘導になる)。
fn run_with_local_stt(
    app: &AppHandle,
    input: &pipeline::PipelineInput<'_>,
    formatter: Option<&dyn format::TextFormatter>,
    cloud_error: Option<&stt::SttError>,
) -> Result<pipeline::PipelineResult, stt::SttError> {
    let models_dir = app.state::<AppState>().models_dir.clone();
    let availability = local_stt::availability(&models_dir);
    if !availability.is_ready() {
        log::warn!("ローカル認識も使えません: {}", availability.message());
        return Err(match cloud_error {
            // クラウドから落ちてきた場合は、そちらの理由の方が効く
            // (「モデルが無い」より「ネットワークが繋がらない」)。
            Some(e) => e.clone(),
            // ローカルのみモードで来た場合は、Groq のキーの話をしない。
            None => stt::SttError::Decode(availability.message()),
        });
    }

    let local = LocalStt { models_dir };
    pipeline::run(input, &local, formatter)
}

/// [`local_stt`] を [`stt::SpeechToText`] として使うためのラッパ。
struct LocalStt {
    models_dir: PathBuf,
}

impl stt::SpeechToText for LocalStt {
    fn transcribe(
        &self,
        request: &stt::TranscribeRequest<'_>,
    ) -> Result<stt::Transcript, stt::SttError> {
        // ローカルモデルは prompt を使わない (辞書バイアスは整形側で効かせる)。
        local_stt::transcribe(&self.models_dir, request.wav, request.language)
    }
}

/// 履歴へ 1 件書く (R4: 注入前)。
///
/// 書けなければ `None` を返し、呼び出し側は注入を続ける。
/// **ただし黙って落とさない**: WAV を退避してユーザーへ知らせる。
/// 履歴に残らないうえ音声も無い、という全損経路を作らないため。
fn record_history(
    app: &AppHandle,
    cfg: &config::Config,
    recording: &RecordingSession,
    payload: &ResultPayload,
) -> Option<i64> {
    if !cfg.history_enabled {
        log::info!("設定により履歴は保存しません");
        return None;
    }

    let (outcome, reason) = match &payload.outcome {
        FormatOutcome::Formatted => (history::OUTCOME_FORMATTED, None),
        FormatOutcome::RawFallback { reason } => {
            (history::OUTCOME_RAW_FALLBACK, Some(reason.clone()))
        }
        FormatOutcome::Disabled => (history::OUTCOME_DISABLED, None),
    };

    let draft = SessionDraft {
        started_at_ms: recording
            .started_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        duration_ms: payload.duration_ms,
        target_process: payload.target_process.clone(),
        target_hwnd: payload.target_hwnd,
        raw_text: Some(payload.raw_text.clone()),
        formatted_text: Some(payload.text.clone()),
        outcome: outcome.to_string(),
        outcome_reason: reason,
        stt_ms: Some(payload.stt_ms),
        format_ms: Some(payload.format_ms),
        wav_path: None,
        style_profile: payload.style_profile.clone(),
    };

    match app.state::<AppState>().history.insert(&draft) {
        Ok(id) => Some(id),
        Err(e) => {
            // 履歴に残せなかった以上、せめて音声は残す。
            log::error!("履歴に保存できません: {e}");
            let dir = app.state::<AppState>().failed_dir.clone();
            let saved = save_failed_recording(&dir, recording, &format!("履歴に保存できず: {e}"));
            let notice = match saved {
                Some(path) => format!(
                    "履歴に保存できませんでした。録音は {} に退避しました",
                    path.display()
                ),
                None => "履歴に保存できず、録音の退避にも失敗しました".to_string(),
            };
            notify(app, Notice::ActionRequired, &notice);
            None
        }
    }
}

/// STT に失敗した録音を、その場で未転写行として履歴へ載せる。
///
/// 起動時の取り込みと同じ形の行を作るので、次回起動の取り込みは
/// `wav_path` の重複で弾かれ、二重登録にならない。
fn record_untranscribed(
    app: &AppHandle,
    recording: &RecordingSession,
    wav_path: &std::path::Path,
    error: &str,
) -> bool {
    let Some(path_str) = wav_path.to_str() else {
        log::warn!(
            "退避 WAV のパスを文字列にできません: {}",
            wav_path.display()
        );
        return false;
    };

    let draft = SessionDraft {
        started_at_ms: recording
            .started_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        duration_ms: recording.duration.as_millis() as u64,
        target_process: recording.target_process().to_string(),
        target_hwnd: recording.target_hwnd(),
        raw_text: None,
        formatted_text: None,
        outcome: history::OUTCOME_UNTRANSCRIBED.to_string(),
        outcome_reason: Some(error.to_string()),
        stt_ms: None,
        format_ms: None,
        wav_path: Some(path_str.to_string()),
        // 整形まで届いていないので「当たらなかった」ではなく未記録。
        style_profile: None,
    };

    match app.state::<AppState>().history.insert_untranscribed(&draft) {
        Ok(Some(_)) => {
            emit_history_changed(app);
            true
        }
        // 既に同じ WAV の行がある (稀: 退避名が衝突した等)。
        Ok(None) => {
            emit_history_changed(app);
            true
        }
        Err(e) => {
            log::error!("未転写の履歴を作れません: {e}");
            false
        }
    }
}

/// 保持期限の定期執行を始める。
///
/// 常駐アプリは何日も起動しっぱなしになる。「起動時 + 録音時」だけでは、
/// 録音せずに放置した場合に「30 日保持」の約束が守られない期間が延々と続く。
///
/// 実行はワーカーへ投げる (DB を触るのはワーカーと短命接続だけ、という
/// 取り決めを守るため)。
fn start_retention_timer(app: &AppHandle) {
    let tx = app.state::<AppState>().finalize_tx.clone();
    let spawned = thread::Builder::new()
        .name("nox-retention".to_string())
        .spawn(move || loop {
            std::thread::sleep(RETENTION_INTERVAL);
            // 受け手が落ちていればループを畳む。
            if tx.send(WorkerJob::EnforceRetention).is_err() {
                break;
            }
        });
    if let Err(e) = spawned {
        log::warn!("保持期限の定期実行を開始できません: {e}");
    }
}

/// 保持期限を執行する。起動時だけでなく録音のたびにも通す。
///
/// 常駐アプリは何日も起動しっぱなしになる。起動時だけの執行では
/// 「30 日保持」の約束が守られない期間が延々と続く。
fn enforce_retention(app: &AppHandle, days: u32) {
    match app.state::<AppState>().history.purge_older_than(days) {
        Ok(0) => {}
        Ok(n) => {
            log::info!("保持期限を過ぎた履歴 {n} 件を削除しました");
            emit_history_changed(app);
        }
        Err(e) => log::warn!("履歴の保持期限処理に失敗: {e}"),
    }
}

/// キー捕獲の確定結果。
///
/// # なぜ `Result` の `Err` ではなく値で返すのか
///
/// 「使えないキーだった」は異常ではなく普通の分岐であり、しかも
/// **捕獲を続けるかどうかが分岐ごとに違う**。`Err` に混ぜると、フロント側は
/// `catch` の中で「これは押し直せる拒否なのか、もう終わっている失敗なのか」を
/// 文字列から推測するしかなくなり、静かに取り違える。実際に、拒否されたのに
/// 捕獲まで畳まれる形で表面化した (E2E T10)。分岐を型にして、
/// **捕獲が続くかどうかを呼び出し側が読み違えられないようにする。**
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
enum CaptureVerdict {
    /// 採用した。捕獲は終了し、確定値は `nox://hotkey-captured` でも届く。
    Accepted,
    /// 取り消した (Esc)。捕獲は終了。
    Cancelled,
    /// ホットキーに使えない。**捕獲は続いている** — 押し直せる。
    Rejected { message: String },
    /// 捕獲が既に畳まれていた (タイムアウト・区画切替・窓から離れた)。
    Expired { message: String },
}

/// 設定 UI のキー捕獲を確定する (フロントの DOM が「全キーを離した」と判断した時点)。
///
/// # なぜフロントから来るのか
///
/// 以前はグローバルフックが捕獲イベントを流していたが、`WH_KEYBOARD_LL` は
/// 応答が遅れると OS に無言で外れる ([`hotkey::spawn_hook_watchdog`] の doc)。
/// 外れている間、PTT より先に**捕獲だけが完全に沈黙する**。捕獲は
/// 「設定ウィンドウにフォーカスがある」場面の操作なので、そもそもグローバル
/// フックを使う必然性が無い。今はフロントが `keydown` / `keyup` を拾い、
/// `KeyboardEvent.code` の列をここへ渡す (`docs/design.md` 2026-08-28)。
///
/// # なぜ VK への変換をここで行うのか
///
/// 許可判定・正規化・表示名がすべて Rust 側に揃っているため
/// ([`hotkey::code_to_vk`] の doc)。フロントは `code` を運ぶだけにして、
/// 判定は既存の [`hotkey::decide_capture_combo`] をそのまま通す。
///
/// `keys` の順序は押された順。トリガーの決め方がそれに依存する
/// (修飾キーだけの組み合わせは「最後に押した方」がトリガー)。
#[tauri::command]
fn finish_hotkey_capture(app: AppHandle, codes: Vec<String>) -> CaptureVerdict {
    // タイムアウトや区画切替で既に畳まれている。ここで確定させると、
    // 「取り消したはずなのにホットキーが変わっていた」になる。
    if !hotkey::is_capturing() {
        return CaptureVerdict::Expired {
            message: "ホットキーの設定は既に終了しています。もう一度やり直してください"
                .to_string(),
        };
    }

    // 写せない `code` は**黙って落とさない**。落とすと「Win + F13」の
    // つもりが「F13」単独で保存される。捕獲は続けたまま理由を返す。
    let keys = match hotkey::codes_to_vks(&codes) {
        Ok(keys) => keys,
        Err(code) => {
            log::info!("捕獲: 写せない code を受け取りました: {code}");
            return CaptureVerdict::Rejected {
                message: format!(
                    "このキー ({code}) はホットキーに使えません。
Ctrl / Alt / Shift / Win / CapsLock / F1〜F24 などから選んでください"
                ),
            };
        }
    };

    match hotkey::decide_capture_combo(&keys) {
        hotkey::CaptureOutcome::Cancel => {
            hotkey::end_capture(None);
            log::info!("キー捕獲を取り消しました");
            emit_hotkey_captured(&app, None);
            CaptureVerdict::Cancelled
        }
        hotkey::CaptureOutcome::Rejected(label) => {
            // **捕獲は続ける。** 押し直せばよい (タイムアウトで自動的に畳む)。
            // ここで end_capture を呼ぶと「1 回押したら終わり」になり、
            // Space のような単独では選べないキーを踏んだ利用者が
            // そのたびにボタンを押し直す羽目になる。
            log::info!("ホットキーに使えない組み合わせです: {label} (捕獲は継続)");
            CaptureVerdict::Rejected {
                message: format!(
                    "「{label}」はホットキーに使えません。押している間ずっと入力先へ流れてしまいます。
Ctrl / Alt / Shift / Win / CapsLock / F1〜F12 などから選んでください"
                ),
            }
        }
        hotkey::CaptureOutcome::Accept(combo) => {
            commit_captured_combo(&app, &keys, combo);
            CaptureVerdict::Accepted
        }
    }
}

/// 確定した組み合わせを設定へ書き、実フックへ反映して UI へ返す。
fn commit_captured_combo(app: &AppHandle, pressed: &[u32], combo: hotkey::HotkeyCombo) {
    log::info!("捕獲 確定: {} ({combo:?})", combo.label());

    // 抑制を**先に**張ってから捕獲モードを抜ける。逆順だと、その隙間に
    // 押しっぱなしのオートリピートが通常経路へ流れ、設定しただけで録音が
    // 始まる (M1 回帰)。押されていないキーは登録されない
    // ([`hotkey::suppress_until_release_keys`] の doc — 登録すると解除する
    // keyup が来ず、そのキーが恒久的に無視される)。
    hotkey::suppress_until_release_keys(pressed);
    hotkey::end_capture(None);

    let state = app.state::<AppState>();
    // 捕獲を始めたコマンドが残した用途へ書く。
    // 用途が増えるたびに if を足していくと、足し忘れた用途が黙って
    // 貼り付け側へ書き込む (= 録音キーが勝手に変わる)。スロット番号
    // から機械的に戻す。
    let mode = HotkeyMode::from_slot(state.capture_slot.load(std::sync::atomic::Ordering::SeqCst));
    let patch = match mode {
        HotkeyMode::Inject => config::ConfigPatch {
            hotkey_vk: Some(combo.vk),
            hotkey_mods: Some(combo.mods_vec()),
            ..Default::default()
        },
        HotkeyMode::ClipboardOnly => config::ConfigPatch {
            clipboard_hotkey_vk: Some(combo.vk),
            clipboard_hotkey_mods: Some(combo.mods_vec()),
            ..Default::default()
        },
        HotkeyMode::ScreenAsk => config::ConfigPatch {
            screen_ask_hotkey_vk: Some(combo.vk),
            screen_ask_hotkey_mods: Some(combo.mods_vec()),
            ..Default::default()
        },
    };
    match state.config.update(patch) {
        Ok(updated) => {
            // 正規化後の値で実際のフックへ反映する。設定が保存できてから
            // にするのは、保存失敗時に挙動と設定がずれるのを防ぐため。
            //
            // 正規化で弾かれることがある (貼り付け用と同じ組み合わせ)。
            // その場合 `clipboard_hotkey_combo()` は None を返すので、
            // UI には「設定されなかった」ことがそのまま伝わる。
            apply_hotkeys(&updated);
            let saved = match mode {
                HotkeyMode::Inject => Some(updated.hotkey_combo()),
                HotkeyMode::ClipboardOnly => updated.clipboard_hotkey_combo(),
                // 有効化フラグを見ない方で確認する。無効のまま
                // キーだけ先に決めるのは正しい操作順なので、
                // それを「保存できなかった」と報告してはいけない。
                HotkeyMode::ScreenAsk => hotkey::HotkeyCombo::from_parts(
                    &updated.screen_ask_hotkey_mods,
                    updated.screen_ask_hotkey_vk,
                ),
            };
            match saved {
                Some(combo) => emit_hotkey_captured_combo(app, &combo),
                None => {
                    emit_error(
                        app,
                        "その組み合わせは他の用途のホットキーと同じなので設定できません",
                    );
                    emit_hotkey_captured(app, None);
                }
            }
        }
        Err(e) => {
            log::error!("ホットキーを保存できません: {e}");
            emit_error(app, &format!("ホットキーを保存できません: {e}"));
            emit_hotkey_captured(app, None);
        }
    }
}

/// 捕獲結果を UI へ返す (`None` は取り消し/失敗)。
fn emit_hotkey_captured_combo(app: &AppHandle, combo: &hotkey::HotkeyCombo) {
    let payload = serde_json::json!({
        "label": combo.label(),
        "mods": combo.mods_vec(),
        "vk": combo.vk,
    });
    if let Err(e) = app.emit(EVENT_HOTKEY_CAPTURED, payload) {
        log::warn!("キー捕獲結果の送出に失敗: {e}");
    }
}

// 押している最中の経過表示は**イベントでは流さない**。
// キーを見ているのはフロント自身なので、`describe_hotkey_codes` の戻り値を
// その場で描けばよい。Rust からイベントで返すと、押すたびに
// invoke → emit → listen の 3 ホップを回ることになり、
// 表示が実際の指の動きから遅れる。

/// 捕獲の終端 (取り消し・タイムアウト・保存失敗) を UI へ返す。
///
/// UI は現在値を取り直して表示を元に戻す。
fn emit_hotkey_captured(app: &AppHandle, result: Option<hotkey::HotkeyCombo>) {
    let payload = result.map(|combo| {
        serde_json::json!({
            "label": combo.label(),
            "mods": combo.mods_vec(),
            "vk": combo.vk,
        })
    });
    if let Err(e) = app.emit(EVENT_HOTKEY_CAPTURED, payload) {
        log::warn!("キー捕獲結果の送出に失敗: {e}");
    }
}

/// 履歴が変わったことをフロントへ知らせる。
fn emit_history_changed(app: &AppHandle) {
    if let Err(e) = app.emit(EVENT_HISTORY, ()) {
        log::warn!("履歴更新イベントの送出に失敗: {e}");
    }
}

/// 起動時の履歴セットアップ: スキーマ作成 → 期限切れ削除 → 失敗 WAV 取り込み。
///
/// どれが失敗してもアプリは起動する (履歴は補助機能で、録音と注入が本体)。
fn initialize_history(app: &AppHandle) {
    let state = app.state::<AppState>();
    if let Err(e) = state.history.initialize() {
        log::error!("{e}");
        emit_error(app, &e.to_string());
        return;
    }

    let cfg = state.config.snapshot();
    enforce_retention(app, cfg.history_retention_days);

    if cfg.history_enabled {
        match import_failed_recordings(&state.history, &state.failed_dir) {
            Ok(0) => {}
            Ok(n) => log::info!("未転写の録音 {n} 件を履歴に取り込みました"),
            Err(e) => log::warn!("失敗録音の取り込みに失敗: {e}"),
        }
    }

    match state.history.count() {
        Ok(n) => log::info!("履歴 {n} 件を保持しています"),
        Err(e) => log::warn!("履歴の件数を取得できません: {e}"),
    }
}

/// `failed/` に残っている WAV を「未転写」行として履歴へ取り込む。
///
/// 既に登録済みのものは飛ばすので、起動のたびに走っても増えない。
/// 戻り値は新規に取り込んだ件数。
fn import_failed_recordings(
    history: &HistoryStore,
    dir: &std::path::Path,
) -> Result<usize, String> {
    if !dir.exists() {
        return Ok(0);
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("{} を読めません: {e}", dir.display()))?;

    let mut imported = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wav") {
            continue;
        }
        let Some(path_str) = path.to_str() else {
            log::warn!("退避 WAV のパスを文字列にできません: {}", path.display());
            continue;
        };

        // 退避時に書いたメタ情報 (無ければ既定値で登録する)。
        let meta = std::fs::read_to_string(path.with_extension("json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let get_u64 = |key: &str| -> u64 {
            meta.as_ref()
                .and_then(|m| m.get(key))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };

        let draft = SessionDraft {
            started_at_ms: get_u64("started_at_ms"),
            duration_ms: get_u64("duration_ms"),
            target_process: meta
                .as_ref()
                .and_then(|m| m.get("target_process"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>")
                .to_string(),
            target_hwnd: meta
                .as_ref()
                .and_then(|m| m.get("target_hwnd"))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0) as isize,
            raw_text: None,
            formatted_text: None,
            outcome: history::OUTCOME_UNTRANSCRIBED.to_string(),
            outcome_reason: meta
                .as_ref()
                .and_then(|m| m.get("error"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            stt_ms: None,
            format_ms: None,
            wav_path: Some(path_str.to_string()),
            style_profile: None,
        };

        match history.insert_untranscribed(&draft) {
            Ok(Some(_)) => imported += 1,
            Ok(None) => {}
            Err(e) => log::warn!("{} を取り込めません: {e}", path.display()),
        }
    }
    Ok(imported)
}

/// 履歴の未転写行を、退避 WAV から転写し直す。
fn retranscribe(app: &AppHandle, id: i64) {
    let state = app.state::<AppState>();

    let wav_path = match state.history.wav_path(id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            emit_background_error(app, "この履歴には再転写できる音声がありません");
            return;
        }
        Err(e) => {
            emit_background_error(app, &format!("履歴を読めません: {e}"));
            return;
        }
    };

    let wav = match std::fs::read(&wav_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            emit_background_error(app, &format!("退避した音声を読めません ({wav_path}): {e}"));
            return;
        }
    };

    let cfg = state.config.snapshot();
    // 整形はクラウドのみ。ローカルのみモードでも整形は使う
    // (テキストの送信は R1 で受容済み。**音声**を出さないことが要点)。
    let formatter = match (cfg.formatting_enabled, state.http.clone()) {
        (true, Some(http)) => Some(
            GeminiFormatter::new(
                http,
                cfg.gemini_url(),
                cfg.gemini_key().secret.unwrap_or_default(),
            )
            .with_timeout(format::FORMAT_TIMEOUT),
        ),
        _ => None,
    };

    // 再転写では画面コンテキストを使わない。録音時の画面はもう無く、
    // 今の画面を混ぜると当時と違う文脈で整形してしまう。
    let entries = cfg.dictionary_entries();
    let mut input = pipeline::PipelineInput::new(&wav, &cfg.language);
    input.dictionary = &entries;
    // 新規録音と同じ経路。ここを別実装にすると設定の見落としが起きる。
    match run_stt_pipeline(
        app,
        &cfg,
        &input,
        formatter.as_ref().map(|f| f as &dyn format::TextFormatter),
    ) {
        Ok(result) => {
            let (outcome, reason) = match &result.outcome {
                FormatOutcome::Formatted => (history::OUTCOME_FORMATTED, None),
                FormatOutcome::RawFallback { reason } => {
                    (history::OUTCOME_RAW_FALLBACK, Some(reason.clone()))
                }
                FormatOutcome::Disabled => (history::OUTCOME_DISABLED, None),
            };
            log::info!(
                "再転写完了 (履歴 #{id}): 生 {} 文字 / 採用 {} 文字",
                result.raw_text.chars().count(),
                result.text.chars().count()
            );
            match state.history.update_transcription(
                id,
                &history::TranscriptionUpdate {
                    raw_text: &result.raw_text,
                    formatted_text: &result.text,
                    outcome,
                    outcome_reason: reason.as_deref(),
                    stt_ms: result.stt_ms,
                    format_ms: result.format_ms,
                },
            ) {
                Ok(0) => {
                    // 再転写中に行が削除された。結果を黙って捨てない。
                    let msg = "再転写した履歴が処理中に削除されたため、結果を保存できませんでした";
                    log::warn!("{msg} (履歴 #{id})");
                    notify(app, Notice::ActionRequired, msg);
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    emit_background_error(app, &format!("再転写の結果を保存できません: {e}"));
                    return;
                }
            }
            emit_history_changed(app);
        }
        Err(e) => {
            // 音声は退避先に残ったままなので、もう一度試せる。
            emit_background_error(app, &format!("再転写に失敗しました: {e}"));
        }
    }
}

/// STT に失敗した WAV を退避する。
///
/// M4 の履歴 DB (R4: 注入前の永続化) が入るまでの暫定措置。
/// 「転写に失敗したから録音も消える」という最悪の失敗モードを避ける。
/// 同時にメタ情報も残し、後から再送できるようにする。
fn save_failed_recording(
    dir: &std::path::Path,
    recording: &RecordingSession,
    error: &str,
) -> Option<PathBuf> {
    if let Err(e) = std::fs::create_dir_all(dir) {
        log::error!("退避先を作成できません ({}): {e}", dir.display());
        return None;
    }

    let stamp = recording
        .started_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // 時刻が取れない (stamp=0) 場合や同一ミリ秒の衝突で、
    // 前の退避を上書きしてしまわないよう空き名を探す。
    let (wav_path, meta_path) = match reserve_failed_paths(dir, stamp) {
        Some(paths) => paths,
        None => {
            log::error!("退避先のファイル名を確保できません ({})", dir.display());
            return None;
        }
    };

    // tmp へ書いてから rename する。途中で落ちても、
    // 半端な .wav が「再送できる録音」の顔をして残らない。
    let tmp = wav_path.with_extension("wav.part");
    if let Err(e) = std::fs::write(&tmp, &recording.wav_bytes) {
        log::error!("録音を退避できません ({}): {e}", tmp.display());
        return None;
    }
    if let Err(e) = std::fs::rename(&tmp, &wav_path) {
        log::error!("録音の退避を確定できません ({}): {e}", wav_path.display());
        let _ = std::fs::remove_file(&tmp);
        return None;
    }

    // メタ情報は失敗しても致命ではない (WAV が残っていれば再送できる)。
    let meta = serde_json::json!({
        "error": error,
        "started_at_ms": stamp as u64,
        "duration_ms": recording.duration.as_millis() as u64,
        "sample_rate": recording.sample_rate,
        "target_process": recording.target_process(),
        "target_hwnd": recording.target_hwnd(),
        // どの用途の録音だったか。画面質問モードの音声だけは扱いが違う
        // ([`answer_screen_question`] の「履歴に入るもの / 入らないもの」)
        // ので、退避したファイル側にも根拠を残す。
        "hotkey_mode": format!("{:?}", recording.mode),
    });
    if let Err(e) = std::fs::write(&meta_path, meta.to_string()) {
        log::warn!("退避メタ情報を書けません ({}): {e}", meta_path.display());
    }

    log::warn!("転写に失敗した録音を退避しました: {}", wav_path.display());
    Some(wav_path)
}

/// 未使用の `<stamp>.wav` / `<stamp>.json` の組を探す。
///
/// 同一ミリ秒や `stamp = 0` (時刻を取れなかった場合) での衝突で
/// 既存の退避を潰さないよう、埋まっていれば連番を付ける。
fn reserve_failed_paths(dir: &std::path::Path, stamp: u128) -> Option<(PathBuf, PathBuf)> {
    for suffix in 0..1_000 {
        let name = if suffix == 0 {
            stamp.to_string()
        } else {
            format!("{stamp}-{suffix}")
        };
        let wav = dir.join(format!("{name}.wav"));
        // .part も見るのは、書きかけと衝突しないため。
        if !wav.exists() && !wav.with_extension("wav.part").exists() {
            return Some((wav, dir.join(format!("{name}.json"))));
        }
    }
    None
}

/// 録音長の上限に到達したときの処理。黙って切り捨てず、停止して可視化する。
fn handle_length_limit(app: &AppHandle, interpreter: &mut PttInterpreter) {
    let state = app.state::<AppState>();
    if !state.is_recording() {
        // 既に停止済み (上限通知と手動停止が競合した)。何もしない。
        return;
    }

    let msg = format!(
        "録音が上限の {} 分に達したため自動停止しました。ここまでの音声は保持されます",
        (audio::MAX_RECORDING_SECONDS / 60.0).round() as u32
    );
    log::warn!("{msg}");
    emit_error(app, &msg);

    if let Err(e) = request_finalize(app) {
        log::error!("上限到達時の停止に失敗: {e}");
        set_status(app, Status::Idle, Some(e));
    }
    // ユーザーはまだキーを押している可能性が高い。その離しは捨てる。
    interpreter.reset();
}

/// 終了時に録音が残っていれば確定させる (無警告で捨てない)。
///
/// 終了処理を長引かせないので**転写は行わず**、WAV を退避先へ書き出す。
/// UI からの再取得は M4 の履歴機能で扱う。
fn finalize_on_exit(app: &AppHandle) {
    let state = app.state::<AppState>();
    let pending = state.recorder.lock().ok().and_then(|mut r| r.take());
    let Some(recorder) = pending else {
        return;
    };
    // 終了処理へ引き渡した時点で録音は終わった扱い。キャンセルキーを非武装へ戻す。
    hotkey::set_recording_active(false);

    log::warn!("録音中に終了が要求されました。録音の確定を試みます");
    let pending = state
        .pending
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| PendingRecording {
            target: TargetWindow::unknown(),
            started_at: recorder.started_at(),
            // 開始情報を取り落とした異常系。貼り付け側に倒す
            // (クリップボードのみへ倒すと、貼られるはずの結果が黙って消える)。
            mode: HotkeyMode::Inject,
            context: context::ScreenContext::default(),
            screen: None,
        });

    match finalize_one(FinalizeJob {
        recorder,
        target: pending.target,
        started_at: pending.started_at,
        mode: pending.mode,
        context: pending.context,
        screen: pending.screen,
    }) {
        Ok(FinalizedRecording { recording, .. }) => {
            // 終了処理をネットワーク待ちで引き延ばさないため転写はしない。
            // 代わりに退避しておき、後から拾えるようにする。
            let saved = save_failed_recording(
                &state.failed_dir,
                &recording,
                "終了時に録音中だったため未転写",
            );
            log::warn!(
                "終了時に録音を確定しました: {} bytes / {:.2} 秒 / 挿入先={} / 退避先={}",
                recording.wav_bytes.len(),
                recording.duration.as_secs_f64(),
                recording.target_process(),
                saved
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(退避に失敗)".to_string()),
            );
        }
        Err(e) => log::error!("終了時の録音確定に失敗しました (音声は失われます): {e}"),
    }
}

/// 状態を更新し、トレイ表示とフロントへ反映する (録音由来)。
fn set_status(app: &AppHandle, status: Status, message: Option<String>) {
    set_status_from(app, status, message, StatusOrigin::Recording);
}

/// 出どころを明示して状態を更新する。
fn set_status_from(app: &AppHandle, status: Status, message: Option<String>, origin: StatusOrigin) {
    let state = app.state::<AppState>();
    if let Ok(mut slot) = state.status.lock() {
        *slot = status;
    }
    if let Ok(item) = state.tray_status_item.lock() {
        if let Some(item) = item.as_ref() {
            tray::update_status(item, status);
        }
    }
    if let Err(e) = app.emit(
        EVENT_STATUS,
        StatusPayload {
            status,
            message,
            origin,
        },
    ) {
        log::warn!("状態イベントの送出に失敗: {e}");
    }
}

/// 録音の流れで起きたエラーをフロントへ送る。小窓にも出す。
fn emit_error(app: &AppHandle, message: &str) {
    emit_error_from(app, message, StatusOrigin::Recording);
}

/// 裏方の作業で起きたエラー。**小窓には出さない。**
///
/// 再転写や履歴操作の失敗を小窓に出すと、録音中なら表示を乗っ取り、
/// さらに `hide_after` が録音中の小窓を消してしまう。
fn emit_background_error(app: &AppHandle, message: &str) {
    emit_error_from(app, message, StatusOrigin::Background);
}

/// 出どころを明示してエラーを送る。
fn emit_error_from(app: &AppHandle, message: &str, origin: StatusOrigin) {
    if let Err(e) = app.emit(
        EVENT_ERROR,
        ErrorPayload {
            message: message.to_string(),
            origin,
        },
    ) {
        log::warn!("エラーイベントの送出に失敗: {e}");
    }
    // 小窓に出したものだけ、畳む対を予約する。
    if origin == StatusOrigin::Recording {
        overlay::hide_after(app, OVERLAY_ERROR_LINGER);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nox-lib-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_recording() -> RecordingSession {
        RecordingSession {
            wav_bytes: b"RIFF____WAVEfmt ".to_vec(),
            sample_rate: audio::TARGET_SAMPLE_RATE,
            target: TargetWindow {
                hwnd: 0x1234,
                process_id: 42,
                process_name: "notepad.exe".to_string(),
                window_title: "無題 - メモ帳".to_string(),
            },
            started_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            duration: Duration::from_millis(2_500),
            mode: HotkeyMode::Inject,
        }
    }

    #[test]
    fn failed_recording_is_written_with_its_metadata() {
        let dir = temp_dir("failed");
        let recording = sample_recording();

        let path = save_failed_recording(&dir, &recording, "Groq のレート制限に達しました")
            .expect("退避に成功する");

        // WAV 本体がそのまま残っていること (再送できる形)。
        assert_eq!(
            std::fs::read(&path).expect("読める"),
            recording.wav_bytes,
            "退避した WAV が壊れている"
        );

        let meta_path = path.with_extension("json");
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).expect("メタを読める"))
                .expect("JSON として読める");
        assert_eq!(meta["target_process"], "notepad.exe");
        assert_eq!(meta["target_hwnd"], 0x1234);
        assert_eq!(meta["duration_ms"], 2_500);
        assert_eq!(meta["sample_rate"], audio::TARGET_SAMPLE_RATE);
        assert!(
            meta["error"]
                .as_str()
                .expect("error は文字列")
                .contains("レート制限"),
            "失敗理由が残っていない"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_recordings_do_not_overwrite_each_other() {
        let dir = temp_dir("failed-multi");
        let first = sample_recording();
        let mut second = sample_recording();
        second.started_at += Duration::from_secs(1);

        let a = save_failed_recording(&dir, &first, "1 回目").expect("退避 1");
        let b = save_failed_recording(&dir, &second, "2 回目").expect("退避 2");
        assert_ne!(a, b, "別の録音が同じファイル名で潰し合っている");
        assert!(a.exists() && b.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// m-4 回帰: 時刻が同じでも (stamp=0 の異常系でも) 上書きし合わない。
    #[test]
    fn identical_timestamps_do_not_collide() {
        let dir = temp_dir("failed-collide");
        let mut a = sample_recording();
        let mut b = sample_recording();
        // 時刻を取れなかった場合を模す (どちらも stamp=0 になる)。
        a.started_at = SystemTime::UNIX_EPOCH;
        b.started_at = SystemTime::UNIX_EPOCH;
        a.wav_bytes = b"AAAA".to_vec();
        b.wav_bytes = b"BBBB".to_vec();

        let pa = save_failed_recording(&dir, &a, "1 件目").expect("退避 1");
        let pb = save_failed_recording(&dir, &b, "2 件目").expect("退避 2");
        assert_ne!(pa, pb, "同じ名前で潰し合っている");
        assert_eq!(std::fs::read(&pa).expect("読める"), b"AAAA");
        assert_eq!(std::fs::read(&pb).expect("読める"), b"BBBB");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// m-4: 書きかけの `.part` が退避済みの顔をして残らない。
    #[test]
    fn no_partial_files_are_left_behind() {
        let dir = temp_dir("failed-atomic");
        save_failed_recording(&dir, &sample_recording(), "テスト").expect("退避できる");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("読める")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "書きかけが残っている: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- M-2: 終了時のキュー退避 ---

    fn test_job(marker: f32) -> FinalizeJob {
        // 0.2 秒ぶんの無音でない波形。WAV 化して長さを持つ。
        let samples: Vec<f32> = (0..3_200).map(|_| marker).collect();
        FinalizeJob {
            recorder: Recorder::for_test(samples, audio::TARGET_SAMPLE_RATE),
            target: TargetWindow::unknown(),
            started_at: SystemTime::now(),
            mode: HotkeyMode::Inject,
            context: context::ScreenContext::default(),
            screen: None,
        }
    }

    /// M-2 回帰: 処理待ちのまま終了しても、キュー内の録音は退避される。
    #[test]
    fn pending_jobs_are_recovered_on_exit() {
        let dir = temp_dir("drain");
        let (tx, rx) = crossbeam_channel::unbounded::<WorkerJob>();
        tx.send(WorkerJob::Finalize(Box::new(test_job(0.1))))
            .expect("送れる");
        tx.send(WorkerJob::Finalize(Box::new(test_job(0.2))))
            .expect("送れる");
        // 再転写待ちは WAV がディスク上にあるので退避対象外。
        tx.send(WorkerJob::Retranscribe { id: 99 }).expect("送れる");

        let recovered = drain_pending_finalizations(&rx, &dir);

        assert_eq!(recovered, 2, "キュー内の録音が退避されていない");
        let wavs: Vec<_> = std::fs::read_dir(&dir)
            .expect("読める")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".wav"))
            .collect();
        assert_eq!(wavs.len(), 2, "退避された WAV の数が合わない: {wavs:?}");
        // 中身が本物の WAV であること (再送できる形)。
        for name in &wavs {
            let bytes = std::fs::read(dir.join(name)).expect("読める");
            assert_eq!(&bytes[0..4], b"RIFF", "{name} が WAV でない");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn draining_an_empty_queue_is_a_no_op() {
        let dir = temp_dir("drain-empty");
        let (_tx, rx) = crossbeam_channel::unbounded::<WorkerJob>();
        assert_eq!(drain_pending_finalizations(&rx, &dir), 0);
        // 空振りで無駄なディレクトリを作らない。
        assert!(!dir.exists());
    }

    // --- M-3: panic 耐性 ---

    #[test]
    fn guard_panic_returns_none_instead_of_unwinding() {
        let previous = std::panic::take_hook();
        // テスト出力を汚さないため一時的に黙らせる。
        std::panic::set_hook(Box::new(|_| {}));
        let result = guard_panic("テスト", || -> i32 { panic!("意図的な panic") });
        std::panic::set_hook(previous);
        assert!(result.is_none());
    }

    #[test]
    fn guard_panic_passes_through_normal_values() {
        assert_eq!(guard_panic("テスト", || 42), Some(42));
    }

    /// M-3 回帰: ジョブ内で panic しても、ループは次の仕事を処理できる。
    #[test]
    fn a_panicking_job_does_not_stop_the_loop() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut processed = 0;
        for i in 0..3 {
            if guard_panic("ジョブ", || {
                if i == 1 {
                    panic!("2 件目で落ちる");
                }
            })
            .is_some()
            {
                processed += 1;
            }
        }
        std::panic::set_hook(previous);
        assert_eq!(processed, 2, "panic 後に後続のジョブが処理されていない");
    }

    /// M-3 回帰: 転写段で panic しても WAV は退避される。
    #[test]
    fn a_panic_after_wav_creation_still_saves_the_recording() {
        let dir = temp_dir("panic-save");
        let recording = sample_recording();

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = guard_panic("転写・整形", || panic!("転写中に落ちた"));
        std::panic::set_hook(previous);

        assert!(outcome.is_none());
        // ワーカーはこの経路で退避する。
        let saved = save_failed_recording(&dir, &recording, "後処理中に内部エラー (panic)")
            .expect("panic 後でも退避できる");
        assert_eq!(std::fs::read(&saved).expect("読める"), recording.wav_bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- M4: 失敗 WAV の履歴取り込みと、DB 失敗時の劣化 ---

    fn temp_store(tag: &str) -> (PathBuf, HistoryStore) {
        let dir = temp_dir(tag);
        let store = HistoryStore::new(dir.join("nox-voice.db"));
        store.initialize().expect("初期化できる");
        (dir, store)
    }

    #[test]
    fn failed_recordings_are_imported_as_untranscribed_rows() {
        let (db_dir, store) = temp_store("import-db");
        let failed_dir = temp_dir("import-wav");
        let recording = sample_recording();
        let wav = save_failed_recording(&failed_dir, &recording, "Groq のレート制限")
            .expect("退避できる");

        let imported = import_failed_recordings(&store, &failed_dir).expect("取り込める");
        assert_eq!(imported, 1);

        let rows = store.recent(10, None).expect("読める");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.outcome, history::OUTCOME_UNTRANSCRIBED);
        assert!(row.has_audio, "再転写できる行になっていない");
        assert_eq!(row.raw_text, None, "未転写なのに本文が入っている");
        // 退避時のメタ情報が引き継がれている。
        assert_eq!(row.target_process, "notepad.exe");
        assert_eq!(row.duration_ms, 2_500);
        assert!(
            row.outcome_reason
                .as_deref()
                .is_some_and(|r| r.contains("レート制限")),
            "失敗理由が失われている"
        );
        assert_eq!(
            store.wav_path(row.id).expect("読める").as_deref(),
            wav.to_str()
        );

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    #[test]
    fn importing_twice_does_not_duplicate_rows() {
        // 起動のたびに走るので、冪等でないと履歴が水増しされる。
        let (db_dir, store) = temp_store("import-idem-db");
        let failed_dir = temp_dir("import-idem-wav");
        save_failed_recording(&failed_dir, &sample_recording(), "失敗").expect("退避できる");

        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("1 回目"),
            1
        );
        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("2 回目"),
            0
        );
        assert_eq!(store.count().expect("読める"), 1);

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    #[test]
    fn importing_from_a_missing_directory_is_not_an_error() {
        let (db_dir, store) = temp_store("import-nodir");
        let missing = temp_dir("import-nodir-wav");
        assert_eq!(
            import_failed_recordings(&store, &missing).expect("エラーにしない"),
            0
        );
        let _ = std::fs::remove_dir_all(&db_dir);
    }

    #[test]
    fn import_ignores_non_wav_files() {
        let (db_dir, store) = temp_store("import-filter-db");
        let failed_dir = temp_dir("import-filter-wav");
        std::fs::create_dir_all(&failed_dir).expect("作れる");
        // 退避のメタ情報 (.json) や書きかけ (.part) を行にしない。
        std::fs::write(failed_dir.join("123.json"), "{}").expect("書ける");
        std::fs::write(failed_dir.join("123.wav.part"), "x").expect("書ける");

        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("取り込める"),
            0
        );
        assert_eq!(store.count().expect("読める"), 0);

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    /// M-3 回帰: STT 失敗の「その場で」履歴に未転写行ができる。
    ///
    /// 起動時の取り込み任せだと、再転写の導線が次回起動まで出てこない。
    /// 常駐運用では、一番必要な瞬間に一番見えない状態になる。
    /// (本番は AppHandle 越しなので、同じ組み合わせをここで検証する)
    #[test]
    fn an_stt_failure_lands_in_history_immediately() {
        let (db_dir, store) = temp_store("stt-fail-db");
        let failed_dir = temp_dir("stt-fail-wav");
        let recording = sample_recording();

        // 本番と同じ順序: WAV を退避 → その場で未転写行を作る。
        let wav = save_failed_recording(&failed_dir, &recording, "Groq のレート制限")
            .expect("退避できる");
        let draft = SessionDraft {
            started_at_ms: 1,
            duration_ms: recording.duration.as_millis() as u64,
            target_process: recording.target_process().to_string(),
            target_hwnd: recording.target_hwnd(),
            raw_text: None,
            formatted_text: None,
            outcome: history::OUTCOME_UNTRANSCRIBED.to_string(),
            outcome_reason: Some("Groq のレート制限".to_string()),
            stt_ms: None,
            format_ms: None,
            wav_path: Some(wav.to_string_lossy().to_string()),
            style_profile: None,
        };
        assert!(store
            .insert_untranscribed(&draft)
            .expect("書ける")
            .is_some());

        // 再起動を待たずに、再転写できる行として見えている。
        let rows = store.recent(10, None).expect("読める");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].has_audio, "再転写の導線が出ない");
        assert_eq!(rows[0].outcome, history::OUTCOME_UNTRANSCRIBED);

        // 次回起動の取り込みは重複を作らない。
        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("取り込める"),
            0,
            "起動時の取り込みが二重登録した"
        );
        assert_eq!(store.count().expect("読める"), 1);

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    /// M-1 回帰: 削除した行は、起動時の取り込みで復活しない。
    #[test]
    fn a_deleted_row_does_not_come_back_on_the_next_startup() {
        let (db_dir, store) = temp_store("revive-db");
        let failed_dir = temp_dir("revive-wav");
        save_failed_recording(&failed_dir, &sample_recording(), "失敗").expect("退避できる");

        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("取り込める"),
            1
        );
        let id = store.recent(10, None).expect("読める")[0].id;

        // ユーザーが履歴から削除する。
        let removal = store.delete(id).expect("消せる");
        assert_eq!(removal.rows, 1);
        assert_eq!(removal.wavs_removed, 1, "WAV を残すと復活する");

        // 起動をやり直しても戻ってこない。
        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("取り込める"),
            0
        );
        assert_eq!(store.count().expect("読める"), 0, "削除した履歴が復活した");

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    /// M-1 回帰: 全消去のあと `failed/` に WAV が残らない (R1 / 無限成長の防止)。
    #[test]
    fn clearing_history_empties_the_failed_directory() {
        let (db_dir, store) = temp_store("clear-revive-db");
        let failed_dir = temp_dir("clear-revive-wav");
        for _ in 0..2 {
            let mut r = sample_recording();
            r.started_at += std::time::Duration::from_secs(1);
            save_failed_recording(&failed_dir, &r, "失敗").expect("退避できる");
        }
        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("取り込める"),
            2
        );

        let removal = store.clear().expect("消せる");
        assert_eq!(removal.rows, 2);
        assert_eq!(removal.wavs_removed, 2);

        let leftover: Vec<_> = std::fs::read_dir(&failed_dir)
            .expect("読める")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(leftover.is_empty(), "全消去後に残骸がある: {leftover:?}");
        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("取り込める"),
            0
        );

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    /// M-2 回帰: 再転写 → 保持期限 → 再取り込み でテキストが失われない。
    #[test]
    fn a_retranscribed_row_is_not_resurrected_as_untranscribed() {
        let (db_dir, store) = temp_store("ghost-db");
        let failed_dir = temp_dir("ghost-wav");
        // 保持期限で正当に消える年代だと、幽霊行の有無を切り分けられない。
        // 「まだ期限内の録音」で試す。
        let mut recording = sample_recording();
        recording.started_at = SystemTime::now();
        save_failed_recording(&failed_dir, &recording, "失敗").expect("退避できる");
        import_failed_recordings(&store, &failed_dir).expect("取り込める");
        let id = store.recent(10, None).expect("読める")[0].id;

        // 再転写に成功する。
        store
            .update_transcription(
                id,
                &history::TranscriptionUpdate {
                    raw_text: "回収した生転写",
                    formatted_text: "回収したテキスト。",
                    outcome: history::OUTCOME_FORMATTED,
                    outcome_reason: None,
                    stt_ms: 1,
                    format_ms: 1,
                },
            )
            .expect("更新できる");

        // 保持期限 → 起動時の取り込み、という次回起動の流れをなぞる。
        store.purge_older_than(30).expect("消せる");
        assert_eq!(
            import_failed_recordings(&store, &failed_dir).expect("取り込める"),
            0,
            "幽霊の未転写行が湧いた"
        );

        let rows = store.recent(10, None).expect("読める");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].formatted_text.as_deref(),
            Some("回収したテキスト。"),
            "回収したテキストが失われた"
        );
        assert!(!rows[0].has_audio);

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    /// DB が壊れていても音声は失わない (R4 の最後の砦)。
    ///
    /// 本番の `record_history` は AppHandle を要るので、
    /// 同じ組み合わせ (履歴書き込み失敗 → WAV 退避) をここで検証する。
    #[test]
    fn a_broken_database_still_leaves_the_audio_on_disk() {
        let db_dir = temp_dir("broken-db");
        std::fs::create_dir_all(&db_dir).expect("作れる");
        let db_path = db_dir.join("nox-voice.db");
        std::fs::write(&db_path, b"not a sqlite file at all").expect("書ける");
        let store = HistoryStore::new(db_path);

        let recording = sample_recording();
        let draft = SessionDraft {
            started_at_ms: 1,
            duration_ms: 2_500,
            target_process: "notepad.exe".to_string(),
            target_hwnd: 0x1234,
            raw_text: Some("生転写".to_string()),
            formatted_text: Some("整形後。".to_string()),
            outcome: history::OUTCOME_FORMATTED.to_string(),
            outcome_reason: None,
            stt_ms: Some(1),
            format_ms: Some(1),
            wav_path: None,
            style_profile: Some(String::new()),
        };

        // 履歴には書けない。
        let err = store.insert(&draft).expect_err("壊れた DB では失敗する");
        // ...が、音声は退避できる。
        let failed_dir = temp_dir("broken-db-wav");
        let saved =
            save_failed_recording(&failed_dir, &recording, &err.to_string()).expect("退避できる");
        assert_eq!(
            std::fs::read(&saved).expect("読める"),
            recording.wav_bytes,
            "履歴が書けないうえ音声も失う全損経路になっている"
        );

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    #[test]
    fn save_creates_the_directory_when_missing() {
        // 何段も掘った未作成のパスでも作れること。
        let dir = temp_dir("failed-nested").join("a").join("b");
        assert!(!dir.exists());
        let path = save_failed_recording(&dir, &sample_recording(), "テスト").expect("退避できる");
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(temp_dir("failed-nested"));
    }

    // --- 録音キャンセル ---
    //
    // ここでの検証は `AppState` を組み立てない。テストバイナリが tauri の
    // ウィンドウ系コードまでリンクすると、マニフェスト (Common-Controls v6)
    // を持たない exe はロード時に落ちるため (discard_recording の doc 参照)。

    /// キャンセル経路は確定処理を通らない。履歴にも退避にも何も残らない。
    ///
    /// cancel_recording が行うのは Recorder の drop だけであり、
    /// finalize_one / save_failed_recording / record_history は呼ばない。
    /// 「破棄なのに痕跡が残る」ことが最悪の失敗モードなので、
    /// 痕跡ゼロをここで畳んでおく。
    #[test]
    fn a_cancelled_recording_leaves_no_history_row_and_no_failed_wav() {
        let (db_dir, store) = temp_store("cancel-db");
        let failed_dir = temp_dir("cancel-wav");

        // キャンセル相当: 確定させずにそのまま捨てる。
        let recorder = Recorder::for_test(vec![0.5f32; 3_200], audio::TARGET_SAMPLE_RATE);
        drop(recorder);

        assert_eq!(
            store.count().expect("読める"),
            0,
            "キャンセルが履歴行を作った"
        );
        let leftovers = std::fs::read_dir(&failed_dir)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "キャンセルが退避ファイルを作った");

        let _ = std::fs::remove_dir_all(&db_dir);
        let _ = std::fs::remove_dir_all(&failed_dir);
    }

    /// キャンセル後のホットキー離しで停止が暴発しないこと。
    ///
    /// PTT 長押し中に Esc を押してキャンセルしたあと、まだ押されたままの
    /// ホットキーを離す。Cancel 分岐は解釈器を reset するので、この離しは
    /// 何も生んではいけない (StopRecording に化けると状態表示とエラーが乱れる)。
    #[test]
    fn a_release_after_cancel_does_not_stop_the_next_recording() {
        // コントローラループと同じ順序: 押下 → Cancel (破棄 + reset) → 離し。
        let mut interpreter = PttInterpreter::new(TAP_THRESHOLD);
        let t0 = Instant::now();
        assert_eq!(interpreter.on_press(t0), Some(HotkeyAction::StartRecording));

        // Esc でのキャンセル。解釈器は経由しないが、押下状態は捨てられる。
        interpreter.reset();

        // ホットキーを離しても何も起きない。
        assert_eq!(
            interpreter.on_release(t0 + Duration::from_millis(800)),
            None,
            "キャンセル後の離しが停止に化けた"
        );
        // 次の押下は素直に録音開始になる。
        assert_eq!(
            interpreter.on_press(t0 + Duration::from_secs(1)),
            Some(HotkeyAction::StartRecording)
        );
    }

    /// キャンセルと上限自動停止が競合しても二重処理にならないこと。
    ///
    /// handle_length_limit は録音スロットが空なら何もしない。キャンセルが
    /// スロットを空にしていれば、遅れて届いた上限通知は無視される。また
    /// discard_recording は滞留した上限通知を掃除するので、次の録音が
    /// 開始直後に誤停止しない。
    #[test]
    fn a_cancelled_recording_does_not_double_stop_on_the_length_limit() {
        let recorder_slot: Mutex<Option<Recorder>> = Mutex::new(Some(Recorder::for_test(
            vec![0.0f32; 16],
            audio::TARGET_SAMPLE_RATE,
        )));
        let pending_slot: Mutex<Option<PendingRecording>> = Mutex::new(None);
        let (limit_tx, limit_rx) = crossbeam_channel::bounded(1);

        // 上限通知が滞留している状況でキャンセルが走る。
        limit_tx.send(()).expect("上限通知を積める");
        assert!(discard_recording(&recorder_slot, &pending_slot, &limit_rx));

        // スロットは空 (= handle_length_limit の早期リターン条件が成立)。
        assert!(recorder_slot.lock().expect("ロックできる").is_none());
        assert!(pending_slot.lock().expect("ロックできる").is_none());
        // 滞留が掃除済みで、次の録音が開始直後に止められないこと。
        assert!(limit_rx.try_recv().is_err(), "上限通知の滞留が残っている");

        // 空スロットへの再実行は古いイベントとして何もしない。
        limit_tx.send(()).expect("上限通知を積める");
        assert!(!discard_recording(&recorder_slot, &pending_slot, &limit_rx));
    }
}
