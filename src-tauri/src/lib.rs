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
mod foreground;
mod hotkey;
mod session;
mod tray;

use std::sync::Mutex;
use std::thread;
use std::time::SystemTime;

use crossbeam_channel::{Receiver, Sender};
use tauri::menu::MenuItem;
use tauri::{AppHandle, Emitter, Manager, RunEvent, Wry};

use audio::Recorder;
use hotkey::{HookHandle, HotkeyAction, PttInterpreter, TAP_THRESHOLD};
use session::{RecordingSession, SessionSummary, Status, StatusPayload, TargetWindow};

/// 状態変化の通知イベント。
const EVENT_STATUS: &str = "nox://status";
/// 録音完了の通知イベント。
const EVENT_SESSION: &str = "nox://session";
/// ユーザーに見せるべきエラーの通知イベント。
const EVENT_ERROR: &str = "nox://error";

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
}

impl AppState {
    fn new() -> Self {
        // 上限到達は 1 録音につき高々 1 回。
        let (limit_tx, limit_rx) = crossbeam_channel::bounded(1);
        let (finalize_tx, finalize_rx) = crossbeam_channel::unbounded();
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
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_last_session,
            show_window
        ])
        .setup(|app| {
            let handle = app.handle().clone();

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
            // フックスレッドを明示的に畳む (drop 任せにしない)。
            if let Ok(mut hook) = handle.state::<AppState>().hook.lock() {
                hook.take();
            }
        }
    });
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
fn finalize_worker(app: AppHandle, rx: Receiver<FinalizeJob>) {
    while let Ok(job) = rx.recv() {
        match finalize_one(&app, job) {
            Ok(summary) => {
                if let Err(e) = app.emit(EVENT_SESSION, &summary) {
                    log::warn!("録音結果イベントの送出に失敗: {e}");
                }
                set_status(&app, Status::Idle, None);
            }
            Err(e) => {
                log::error!("録音を確定できません: {e}");
                emit_error(&app, &e);
                set_status(&app, Status::Idle, Some(e));
            }
        }
    }
    log::info!("ファイナライズワーカーを終了");
}

/// 1 件の録音を WAV 化し、[`RecordingSession`] を確定させる。
///
/// M2 ではこの関数の末尾 (仮ハンドラ) が STT 呼び出しに置き換わる。
fn finalize_one(app: &AppHandle, job: FinalizeJob) -> Result<SessionSummary, String> {
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

    // --- 仮ハンドラ (M2 で STT 呼び出しに置換する) ---
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

    let summary = recording.summary();
    if let Ok(mut slot) = app.state::<AppState>().last_session.lock() {
        *slot = Some(recording);
    }
    Ok(summary)
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
/// UI からの再取得は M4 の履歴機能で扱う。ここでは最低限、
/// 音声が存在したことと WAV 化の成否をログに残す。
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
        app,
        FinalizeJob {
            recorder,
            target,
            started_at,
        },
    ) {
        Ok(summary) => log::warn!(
            "終了時に録音を確定しました: {} bytes / {:.2} 秒 / 挿入先={}。\
             注入は行われていません",
            summary.wav_bytes,
            summary.duration_ms as f64 / 1000.0,
            summary.target_process,
        ),
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
