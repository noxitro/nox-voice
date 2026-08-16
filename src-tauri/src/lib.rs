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
mod foreground;
mod format;
mod hotkey;
mod pipeline;
mod session;
mod stt;
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
use format::GeminiFormatter;
use hotkey::{HookHandle, HotkeyAction, PttInterpreter, TAP_THRESHOLD};
use pipeline::FormatOutcome;
use session::{RecordingSession, SessionSummary, Status, StatusPayload, TargetWindow};
use stt::GroqStt;

/// 状態変化の通知イベント。
const EVENT_STATUS: &str = "nox://status";
/// 録音完了の通知イベント (WAV のメタ情報)。
const EVENT_SESSION: &str = "nox://session";
/// 転写・整形の結果イベント。
const EVENT_RESULT: &str = "nox://result";
/// ユーザーに見せるべきエラーの通知イベント。
const EVENT_ERROR: &str = "nox://error";

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
}

/// ファイナライズワーカーへ渡す仕事。
///
/// 停止時にコントローラが組み立て、以降の重い処理はすべてワーカー側で行う。
struct FinalizeJob {
    recorder: Recorder,
    target: TargetWindow,
    started_at: SystemTime,
}

/// アプリ全体の共有状態。
///
/// 各フィールドを個別の `Mutex` にしてあるのは、ある操作の最中も
/// 状態問い合わせをブロックさせないため。
struct AppState {
    status: Mutex<Status>,
    recorder: Mutex<Option<Recorder>>,
    /// 録音開始時に確定させた挿入先と開始時刻。停止時に取り出す。
    pending: Mutex<Option<(TargetWindow, SystemTime)>>,
    last_session: Mutex<Option<RecordingSession>>,
    /// トレイの状態表示項目。トレイ構築後にセットされる。
    tray_status_item: Mutex<Option<MenuItem<Wry>>>,
    /// フックの生存を握る。drop でフックスレッドが止まる。
    hook: Mutex<Option<HookHandle>>,
    /// 録音長の上限到達通知。送信は音声コールバック、受信はコントローラ。
    /// 送受信端の両方を持つのは、どちらも切断させないため。
    limit_tx: Sender<()>,
    limit_rx: Receiver<()>,
    /// ファイナライズワーカーへの仕事キュー。
    finalize_tx: Sender<FinalizeJob>,
    finalize_rx: Receiver<FinalizeJob>,
    /// 設定 (API キー・言語・辞書など)。
    config: ConfigStore,
    /// STT / 整形で共用する HTTP クライアント。接続プールを使い回すため
    /// 録音ごとに作り直さない。構築に失敗した場合のみ `None`。
    http: Option<reqwest::blocking::Client>,
    /// 直近の転写・整形結果。
    last_result: Mutex<Option<ResultPayload>>,
    /// STT に失敗した WAV の退避先 (M4 の履歴 DB が入るまでの暫定)。
    failed_dir: PathBuf,
}

impl AppState {
    fn new(config_path: PathBuf, failed_dir: PathBuf) -> Self {
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
            http,
            last_result: Mutex::new(None),
            failed_dir,
        }
    }

    fn is_recording(&self) -> bool {
        self.recorder.lock().map(|r| r.is_some()).unwrap_or(false)
    }
}

/// 現在の状態を返す。
#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> Status {
    state
        .status
        .lock()
        .map(|s| *s)
        .unwrap_or(Status::Idle)
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
    state: tauri::State<'_, AppState>,
    patch: ConfigPatch,
) -> Result<ConfigView, String> {
    // patch は API キーを含みうる。ログには出さない。
    let updated = state.config.update(patch)?;
    Ok(ConfigView::from(&updated))
}

/// 設定ウィンドウ (現状はメインウィンドウ) を表示する。
#[tauri::command]
fn show_window(app: AppHandle) {
    tray::show_main_window(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
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
            show_window
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // 設定と退避先のパスは AppHandle が無いと決まらないので、
            // 状態の登録は builder ではなく setup で行う。
            let (config_path, failed_dir) = resolve_paths(&handle);
            app.manage(AppState::new(config_path, failed_dir));

            match tray::build(&handle) {
                Ok(item) => {
                    if let Ok(mut slot) = handle.state::<AppState>().tray_status_item.lock() {
                        *slot = Some(item);
                    }
                }
                // トレイが出せなくても録音機能自体は使えるので継続する。
                Err(e) => log::error!("トレイの構築に失敗: {e}"),
            }

            start_finalize_worker(&handle);
            start_hotkey_controller(&handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            // 「閉じる」は終了ではなく非表示。常駐を維持する。
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
fn drain_pending_finalizations(rx: &Receiver<FinalizeJob>, dir: &std::path::Path) -> usize {
    let mut recovered = 0;
    // try_recv なのでキューが空になれば即抜ける (終了処理を止めない)。
    while let Ok(job) = rx.try_recv() {
        let Some(Ok(recording)) = guard_panic("終了時の WAV 化", || finalize_one(job)) else {
            log::error!("終了時に後処理待ちの録音を WAV 化できませんでした");
            continue;
        };
        if save_failed_recording(dir, &recording, "後処理待ちのまま終了したため未転写").is_some() {
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
            log::warn!("アプリのデータディレクトリを解決できません ({e})。実行ファイル隣に置きます");
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("nox-voice-data")
        });
    (base.join("config.json"), base.join("failed"))
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
            let mut interpreter = PttInterpreter::new(TAP_THRESHOLD);
            let mut seen_drops = 0u64;

            loop {
                crossbeam_channel::select! {
                    recv(rx) -> msg => match msg {
                        // フック側が落ちた = 終了。
                        Err(_) => break,
                        Ok(event) => {
                            // 判定は必ずイベントの発生時刻で行う。
                            // ここで Instant::now() を使うと、直前の処理で
                            // 詰まった分だけ短押しが長押しに化ける。
                            match interpreter.on_event(event) {
                                Some(HotkeyAction::StartRecording) => {
                                    if let Err(e) = start_recording(&app) {
                                        log::error!("録音を開始できません: {e}");
                                        emit_error(&app, &e);
                                        set_status(&app, Status::Idle, Some(e));
                                        interpreter.reset();
                                    }
                                }
                                Some(HotkeyAction::StopRecording) => {
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
                        handle_length_limit(&app, &mut interpreter);
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
fn start_recording(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();

    let mut slot = state
        .recorder
        .lock()
        .map_err(|_| "録音状態のロックが毒化しました".to_string())?;
    if slot.is_some() {
        return Err("すでに録音中です".to_string());
    }

    // 前回の録音が残した上限通知を捨てる。取りこぼすと、次の録音が
    // 開始直後に「上限到達」で止められてしまう。
    // (この関数はコントローラスレッド専用なので、受信の競合は起きない)
    while state.limit_rx.try_recv().is_ok() {}

    // デバイス初期化には数十 ms かかりうるので、その前に前景を押さえる。
    // ここで採った HWND が M3 の挿入先照合 (R7) の基準になる。
    let target = foreground::capture_foreground();

    let recorder = audio::start(state.limit_tx.clone()).map_err(|e| e.to_string())?;
    let started_at = recorder.started_at();
    log::info!(
        "録音開始 (挿入先: {} / hwnd=0x{:X} / \"{}\")",
        target.process_name,
        target.hwnd,
        target.window_title
    );
    if !target.is_known() {
        log::warn!("前景ウィンドウを特定できませんでした。挿入時の照合は行えません");
    }

    *slot = Some(recorder);
    drop(slot);

    if let Ok(mut pending) = state.pending.lock() {
        *pending = Some((target, started_at));
    }

    set_status(app, Status::Recording, None);
    Ok(())
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

    let (target, started_at) = state
        .pending
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| (TargetWindow::unknown(), recorder.started_at()));

    set_status(app, Status::Processing, None);

    state
        .finalize_tx
        .send(FinalizeJob {
            recorder,
            target,
            started_at,
        })
        .map_err(|_| "後処理ワーカーが停止しています".to_string())
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

fn finalize_worker(app: AppHandle, rx: Receiver<FinalizeJob>) {
    while let Ok(job) = rx.recv() {
        process_job(&app, job);

        // 次の録音が既に始まっているなら Idle へ戻さない。
        // 無条件に戻すと、録音 B の最中に録音 A の後処理が終わった瞬間、
        // 表示が「待機中」に化ける。
        if !app.state::<AppState>().is_recording() {
            set_status(&app, Status::Idle, None);
        }
    }
    log::info!("ファイナライズワーカーを終了");
}

/// ジョブ 1 件を処理する。**どんな失敗でもワーカーを殺さない。**
fn process_job(app: &AppHandle, job: FinalizeJob) {
    // 第 1 段: WAV 化。ここで panic すると音声は救えないので、
    // せめてワーカーを生かして次の録音を処理できるようにする。
    let recording = match guard_panic("録音の WAV 化", || finalize_one(job)) {
        Some(Ok(recording)) => recording,
        Some(Err(e)) => {
            log::error!("録音を確定できません: {e}");
            emit_error(app, &e);
            return;
        }
        None => {
            emit_error(app, "録音の WAV 化中に内部エラーが発生しました (録音は失われました)");
            return;
        }
    };

    let summary = recording.summary();
    if let Err(e) = app.emit(EVENT_SESSION, &summary) {
        log::warn!("録音メタ情報イベントの送出に失敗: {e}");
    }

    // 第 2 段: 転写と整形。ここで panic しても WAV は手元にあるので退避できる。
    if guard_panic("転写・整形", || transcribe_and_format(app, &recording)).is_none() {
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
fn finalize_one(job: FinalizeJob) -> Result<RecordingSession, String> {
    let FinalizeJob {
        recorder,
        target,
        started_at,
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
    Ok(recording)
}

/// WAV を転写し、必要なら整形して結果を通知する。
///
/// - 整形の失敗は [`pipeline::run`] が生転写へ落とす (R2 劣化モード)。
/// - **STT の失敗は劣化できない**ので、WAV を退避してから通知する
///   (M4 の履歴 DB が入るまでの暫定措置 / R4 の趣旨)。
fn transcribe_and_format(app: &AppHandle, recording: &RecordingSession) {
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

    // キーが無い場合も同じ経路を通す。GroqStt が MissingApiKey を返し、
    // ユーザーには「キーを設定してください」という文言で届く。
    let groq_key = cfg.groq_key().secret.unwrap_or_default();
    let stt_client = GroqStt::new(http.clone(), &cfg.groq_endpoint, &cfg.stt_model, groq_key);

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

    let outcome = pipeline::run(
        &recording.wav_bytes,
        &cfg.language,
        &cfg.dictionary,
        &stt_client,
        formatter.as_ref().map(|f| f as &dyn format::TextFormatter),
    );

    match outcome {
        Ok(result) => {
            let payload = ResultPayload {
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
            if let Err(e) = app.emit(EVENT_RESULT, &payload) {
                log::warn!("結果イベントの送出に失敗: {e}");
            }
        }
        Err(e) => {
            let msg = e.to_string();
            log::error!("転写に失敗しました: {msg}");
            let saved = save_failed_recording(&state.failed_dir, recording, &msg);
            let notice = match saved {
                Some(path) => format!("{msg}\n録音は {} に保存しました", path.display()),
                None => format!("{msg}\n※録音の退避にも失敗しました"),
            };
            emit_error(app, &notice);
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

    log::warn!("録音中に終了が要求されました。録音の確定を試みます");
    let (target, started_at) = state
        .pending
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| (TargetWindow::unknown(), recorder.started_at()));

    match finalize_one(
        FinalizeJob {
            recorder,
            target,
            started_at,
        },
    ) {
        Ok(recording) => {
            // 終了処理をネットワーク待ちで引き延ばさないため転写はしない。
            // 代わりに退避しておき、後から拾えるようにする。
            let saved =
                save_failed_recording(&state.failed_dir, &recording, "終了時に録音中だったため未転写");
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

/// 状態を更新し、トレイ表示とフロントへ反映する。
fn set_status(app: &AppHandle, status: Status, message: Option<String>) {
    let state = app.state::<AppState>();
    if let Ok(mut slot) = state.status.lock() {
        *slot = status;
    }
    if let Ok(item) = state.tray_status_item.lock() {
        if let Some(item) = item.as_ref() {
            tray::update_status(item, status);
        }
    }
    if let Err(e) = app.emit(EVENT_STATUS, StatusPayload { status, message }) {
        log::warn!("状態イベントの送出に失敗: {e}");
    }
}

/// ユーザー可視のエラーをフロントへ送る。
fn emit_error(app: &AppHandle, message: &str) {
    if let Err(e) = app.emit(EVENT_ERROR, message) {
        log::warn!("エラーイベントの送出に失敗: {e}");
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
        }
    }

    /// M-2 回帰: 処理待ちのまま終了しても、キュー内の録音は退避される。
    #[test]
    fn pending_jobs_are_recovered_on_exit() {
        let dir = temp_dir("drain");
        let (tx, rx) = crossbeam_channel::unbounded::<FinalizeJob>();
        tx.send(test_job(0.1)).expect("送れる");
        tx.send(test_job(0.2)).expect("送れる");

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
        let (_tx, rx) = crossbeam_channel::unbounded::<FinalizeJob>();
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

    #[test]
    fn save_creates_the_directory_when_missing() {
        // 何段も掘った未作成のパスでも作れること。
        let dir = temp_dir("failed-nested").join("a").join("b");
        assert!(!dir.exists());
        let path = save_failed_recording(&dir, &sample_recording(), "テスト").expect("退避できる");
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(temp_dir("failed-nested"));
    }
}
