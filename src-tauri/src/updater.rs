//! アプリ内の自動更新 (tauri-plugin-updater)。
//!
//! # 方針: 知らせて、選ばせる
//!
//! ```text
//! [起動 30 秒後 / 以後 6 時間ごと / トレイ「更新を確認」]
//!     ▼ check()      latest.json (GitHub Release の最新版) を読む
//!     ▼ download()   署名を確かめた setup.exe を**メモリに持つだけ**。入れない
//!     ▼ 通知 1 回    「更新して再起動 (vX.Y.Z)」をトレイで有効にする
//! [利用者がトレイで選ぶ]
//!     ▼ 録音・処理中なら断る (通知で理由を言う)
//!     ▼ install()    NSIS を passive で起動 → このプロセスは即終了 → 入れ終わったら再起動
//! ```
//!
//! **勝手に入れない。** 音声入力は「押したら必ず録れる」ことが価値で、ある朝突然
//! 再起動していたら、その間の発話は失われる。ダウンロードまでは裏で済ませておき、
//! 入れるタイミングだけは利用者に握らせる。
//!
//! **録音・処理中は入れない** ([`can_install_now`])。Windows ではインストーラを
//! 起動した時点でプラグインが `std::process::exit(0)` を呼ぶ (Tauri の制約。
//! NSIS が実行中の exe を置き換えられないため)。処理中の録音は転写されずに終わる。
//! 待って入れる (アイドルになるまで保留する) 作りにしなかったのは、利用者が
//! 押した瞬間と実際に再起動する瞬間がずれると「いつの間にか落ちた」と同じ体験に
//! なるから。断って、もう一度押してもらう方が単純で、何が起きたかが分かる。
//!
//! # `RunEvent::Exit` は来ない
//!
//! 上の `exit(0)` は Tauri のイベントループを通らない。そこで `on_before_exit` から
//! 録音の退避 ([`crate::salvage_recordings`]、通常の終了と同じもの) を呼ぶ。
//! 呼ばないと、更新のときだけ録音中・処理待ちの音声が黙って消える。
//!
//! フックやスレッドを畳む側 ([`crate::shutdown_cleanup`] の残り) は呼ばない。
//! `on_before_exit` はインストーラの起動**より前**に走り、起動はまだ失敗しうる。
//! 畳んでから失敗すると、押しても録れない常駐が残る。成功すれば `exit(0)` で
//! プロセスごと消えるので、畳む必要がそもそも無い。失敗時に `app.restart()` で
//! 組み直す案は採らなかった: 子を起動してから親が終わるので、子の
//! 多重起動防止 (`instance::ensure_single_instance`) が親のミューテックスに当たりうる。
//!
//! # 通知は版ごとに 1 回
//!
//! 6 時間ごとの確認で同じ版を見つけるたびに通知すると、無視している人を責め続ける
//! ことになる。定期の確認では、まだ知らせていない版のときだけ出す
//! ([`should_notify`])。覚えておくのはこのプロセスの中だけ — 再起動 (PC の起動を
//! 含む) のあとは 1 回だけ出し直す。それ以上覚えさせるには設定ファイルに版を
//! 書くことになるが、そこまでして黙らせる理由が無い。
//! トレイから「更新を確認」を選んだときは、結果を必ず返す (選んだのに無反応は
//! 壊れているのと区別できない)。

use std::sync::Mutex;
use std::time::Duration;

use tauri::menu::MenuItem;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::session::Status;

/// 起動してから最初に確認するまでの間。
///
/// 起動直後はホットキーのフックや小窓の作成が走っており、PC の起動時なら
/// ネットワークもまだつながっていないことが多い。急ぐ理由は無い。
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
/// 定期確認の間隔。常駐アプリは何日も起動しっぱなしになるので、起動時だけでは足りない。
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// latest.json の取得の時間切れ。小さな JSON なので長くは待たない。
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// setup.exe のダウンロードの時間切れ。
///
/// プラグインはダウンロードに時間切れを付けない (check の設定は引き継がれない)。
/// 回線が詰まったまま返らないと「確認中」のまま次の確認もできなくなるので、
/// 遅い回線でも数 MB が落ち切る長さで打ち切る。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 確認のきっかけ。結果をどこまで利用者に知らせるかが変わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 起動時・定期。新しい版の初回だけ知らせ、失敗はログだけにする
    /// (オフラインの PC で 6 時間ごとにエラーを出さない)。
    Scheduled,
    /// トレイの「更新を確認」。結果を必ず知らせる。
    Manual,
}

/// 1 回の確認の結末。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// いまの版が最新。
    UpToDate,
    /// 新しい版をダウンロードし終え、入れられる状態になった。
    Ready { version: String },
    /// 確認かダウンロードに失敗した。
    Failed { reason: String },
}

/// 確認の結末を、トーストに出す文に変える。`None` なら出さない (ログだけ)。
///
/// `already_notified` は、この版を既に知らせたか ([`should_notify`] の結果の裏返し)。
pub fn report_message(
    trigger: Trigger,
    outcome: &CheckOutcome,
    current_version: &str,
    already_notified: bool,
) -> Option<String> {
    match (trigger, outcome) {
        (Trigger::Manual, CheckOutcome::UpToDate) => Some(format!("最新です (v{current_version})")),
        (Trigger::Scheduled, CheckOutcome::UpToDate) => None,
        (Trigger::Scheduled, CheckOutcome::Ready { .. }) if already_notified => None,
        (_, CheckOutcome::Ready { version }) => Some(format!(
            "新しい版 v{version} の準備ができました。トレイの「更新して再起動」で入れられます"
        )),
        (Trigger::Manual, CheckOutcome::Failed { reason }) => {
            Some(format!("更新を確認できませんでした: {reason}"))
        }
        (Trigger::Scheduled, CheckOutcome::Failed { .. }) => None,
    }
}

/// 見つけた版を (定期の確認で) 知らせるべきか。まだ知らせていない版のときだけ真。
pub fn should_notify(found: &str, notified: Option<&str>) -> bool {
    notified != Some(found)
}

/// 見つけた版をダウンロードし直す必要があるか。
///
/// 既に同じ版が手元に揃っていれば、6 時間ごとに数 MB を落とし直さない。
/// より新しい版が出ていれば (準備済みの版と違えば) 取り直す。
pub fn needs_download(found: &str, ready: Option<&str>) -> bool {
    ready != Some(found)
}

/// いま更新を入れてよいか。だめなら利用者に見せる理由を返す。
///
/// `queued` は後処理ワーカーのキューに積まれたままの仕事の数。状態表示 (`status`)
/// だけでは足りない: ワーカーは仕事を 1 つ終えるたびに表示を `Idle` に戻すので、
/// 録音を続けて 2 本止めた直後は「待機中」なのに 2 本目が未処理で残っている。
/// ここで終了すると 2 本目は転写されない (終了時の退避で WAV は残るが、貼られない)。
pub fn can_install_now(status: Status, queued: usize) -> Result<(), &'static str> {
    match status {
        Status::Recording => {
            Err("録音中のため更新できません。録音が終わってからもう一度選んでください")
        }
        Status::Processing => {
            Err("処理中のため更新できません。処理が終わってからもう一度選んでください")
        }
        Status::Idle if queued > 0 => {
            Err("処理中のため更新できません。処理が終わってからもう一度選んでください")
        }
        Status::Idle => Ok(()),
    }
}

/// 「更新を確認」の文言。確認中は押せなくし、押せない理由を文言で見せる。
pub fn check_item_label(running: bool) -> &'static str {
    if running {
        "更新を確認中…"
    } else {
        "更新を確認"
    }
}

/// 「更新して再起動」の文言。準備済みの版があれば版を添える。
pub fn install_item_label(ready: Option<&str>) -> String {
    match ready {
        Some(version) => format!("更新して再起動 (v{version})"),
        None => "更新して再起動".to_string(),
    }
}

/// プラグインのエラーを利用者向けの短い文にする。
///
/// 最初のリリース前や latest.json を載せ忘れた Release では、エンドポイントが
/// 404 を返し、プラグインは `ReleaseNotFound` (英語の一般的な文) にまとめてしまう。
/// 利用者に打つ手は無いので、何が起きたかだけを日本語で言う。
fn describe_error(e: &tauri_plugin_updater::Error) -> String {
    match e {
        tauri_plugin_updater::Error::ReleaseNotFound => {
            "更新情報 (latest.json) を取得できませんでした".to_string()
        }
        other => other.to_string(),
    }
}

/// 準備済みの更新。`Update` は `install()` に要る設定 (インストールモード・
/// 終了前の後始末) を抱えているので、ダウンロードした中身と一緒に持つ。
struct Ready {
    update: Update,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Inner {
    /// 確認〜ダウンロードが進行中か。同時に 2 本走らせない。
    running: bool,
    ready: Option<Ready>,
    /// 定期の確認で知らせ済みの版。
    notified: Option<String>,
}

/// 更新の状態。`app.manage` で登録する。
pub struct UpdaterState {
    inner: Mutex<Inner>,
    check_item: Option<MenuItem<Wry>>,
    install_item: Option<MenuItem<Wry>>,
}

impl UpdaterState {
    /// トレイが作れなかった場合は項目なしで作る (定期の確認と通知だけ動く。
    /// 入れる手段が無いので、通知の文はトレイが無い場合でも変えない — トレイが
    /// 無い状態自体が既にエラーとしてログに出ている)。
    pub fn new(check_item: Option<MenuItem<Wry>>, install_item: Option<MenuItem<Wry>>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            check_item,
            install_item,
        }
    }

    fn set_running(&self, running: bool) {
        if let Some(item) = &self.check_item {
            if let Err(e) = item.set_text(check_item_label(running)) {
                log::warn!("トレイの「更新を確認」を書き換えられません: {e}");
            }
            if let Err(e) = item.set_enabled(!running) {
                log::warn!("トレイの「更新を確認」を切り替えられません: {e}");
            }
        }
    }

    fn set_ready(&self, ready: Option<&str>) {
        if let Some(item) = &self.install_item {
            if let Err(e) = item.set_text(install_item_label(ready)) {
                log::warn!("トレイの「更新して再起動」を書き換えられません: {e}");
            }
            if let Err(e) = item.set_enabled(ready.is_some()) {
                log::warn!("トレイの「更新して再起動」を切り替えられません: {e}");
            }
        }
    }
}

/// 更新のトースト。
///
/// `crate::notify` は使わない。あちらは WebView へ**エラー**としても送るので、
/// 「新しい版があります」が設定画面や小窓にエラー表示で出てしまう。
fn toast(app: &AppHandle, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title("nox-voice")
        .body(body)
        .show()
    {
        log::warn!("更新の通知を表示できません: {e}");
    }
}

/// 起動時と定期の確認を始める。
pub fn start_schedule(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("nox-updater".to_string())
        .spawn(move || {
            std::thread::sleep(FIRST_CHECK_DELAY);
            loop {
                spawn_check(&app, Trigger::Scheduled);
                std::thread::sleep(CHECK_INTERVAL);
            }
        });
    if let Err(e) = spawned {
        log::warn!("更新の定期確認を開始できません: {e}");
    }
}

/// トレイの「更新を確認」。
pub fn check_now(app: &AppHandle) {
    spawn_check(app, Trigger::Manual);
}

/// 確認を 1 本、非同期に走らせる。既に走っていれば何もしない。
fn spawn_check(app: &AppHandle, trigger: Trigger) {
    let Some(state) = app.try_state::<UpdaterState>() else {
        log::warn!("更新の状態が未登録のため確認しません");
        return;
    };
    {
        let Ok(mut inner) = state.inner.lock() else {
            log::warn!("更新の状態を読めません (ロックが壊れている)");
            return;
        };
        if inner.running {
            // 手動のときは項目が無効になっているので、ここへは定期の確認が
            // 手動の確認と重なったときだけ来る。黙って譲る。
            log::info!("更新の確認は既に進行中");
            return;
        }
        inner.running = true;
    }
    state.set_running(true);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // 本体は別タスクにして結果を待つ。本体が panic しても finish_check を
        // 必ず通し、`running` が立ったまま (確認が二度と走らない) にしない。
        let worker_app = app.clone();
        let outcome =
            match tauri::async_runtime::spawn(async move { run_check(&worker_app).await }).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    log::warn!("更新の確認が異常終了しました: {e}");
                    CheckOutcome::Failed {
                        reason: "更新の確認が異常終了しました".to_string(),
                    }
                }
            };
        finish_check(&app, trigger, outcome);
    });
}

/// 確認とダウンロードの本体。結果は [`finish_check`] が状態へ書く。
async fn run_check(app: &AppHandle) -> CheckOutcome {
    let cleanup_app = app.clone();
    let updater = match app
        .updater_builder()
        .timeout(CHECK_TIMEOUT)
        // Windows では install() がインストーラを起動した直後に exit(0) する。
        // RunEvent::Exit を通らないので、録音の退避だけをここから呼ぶ (モジュールの doc)。
        .on_before_exit(move || crate::salvage_recordings(&cleanup_app))
        .build()
    {
        Ok(u) => u,
        Err(e) => {
            log::warn!("更新の確認を準備できません: {e}");
            return CheckOutcome::Failed {
                reason: describe_error(&e),
            };
        }
    };

    let mut update = match updater.check().await {
        Ok(Some(update)) => update,
        Ok(None) => {
            log::info!("更新の確認: 最新です");
            return CheckOutcome::UpToDate;
        }
        Err(e) => {
            log::warn!("更新の確認に失敗: {e}");
            return CheckOutcome::Failed {
                reason: describe_error(&e),
            };
        }
    };
    let version = update.version.clone();

    let ready_version = app.try_state::<UpdaterState>().and_then(|s| {
        s.inner
            .lock()
            .ok()
            .and_then(|i| i.ready.as_ref().map(|r| r.update.version.clone()))
    });
    if !needs_download(&version, ready_version.as_deref()) {
        log::info!("更新の確認: v{version} は準備済み");
        return CheckOutcome::Ready { version };
    }

    log::info!("更新の確認: v{version} が見つかったのでダウンロードします");
    update.timeout = Some(DOWNLOAD_TIMEOUT);
    // download() は署名 (tauri.conf.json の pubkey) を確かめてから中身を返す。
    // 検証に落ちたものは手元に残らない。
    match update.download(|_, _| {}, || {}).await {
        Ok(bytes) => {
            log::info!(
                "更新 v{version} をダウンロードしました ({} バイト)",
                bytes.len()
            );
            if let Some(state) = app.try_state::<UpdaterState>() {
                if let Ok(mut inner) = state.inner.lock() {
                    inner.ready = Some(Ready { update, bytes });
                }
            }
            CheckOutcome::Ready { version }
        }
        Err(e) => {
            log::warn!("更新 v{version} のダウンロードに失敗: {e}");
            CheckOutcome::Failed {
                reason: describe_error(&e),
            }
        }
    }
}

fn finish_check(app: &AppHandle, trigger: Trigger, outcome: CheckOutcome) {
    let Some(state) = app.try_state::<UpdaterState>() else {
        return;
    };
    let (already_notified, ready_version) = match state.inner.lock() {
        Ok(mut inner) => {
            inner.running = false;
            if matches!(outcome, CheckOutcome::UpToDate) {
                // 配信側が「最新」と言うなら、手元の準備済み版は取り下げられた
                // ものなので入れさせない (Release を消した・差し戻した場合)。
                inner.ready = None;
            }
            let already = match &outcome {
                CheckOutcome::Ready { version } => {
                    let already = !should_notify(version, inner.notified.as_deref());
                    inner.notified = Some(version.clone());
                    already
                }
                _ => false,
            };
            (
                already,
                inner.ready.as_ref().map(|r| r.update.version.clone()),
            )
        }
        Err(_) => {
            log::warn!("更新の状態を書けません (ロックが壊れている)");
            (false, None)
        }
    };
    state.set_running(false);
    state.set_ready(ready_version.as_deref());

    let current = app.package_info().version.to_string();
    if let Some(message) = report_message(trigger, &outcome, &current, already_notified) {
        toast(app, &message);
    }
}

/// トレイの「更新して再起動」。
///
/// 成功すればこの関数は戻らない (プラグインがプロセスを終了させる)。
pub fn install_now(app: &AppHandle) {
    let status = crate::current_status(app);
    let queued = crate::queued_jobs(app);
    if let Err(reason) = can_install_now(status, queued) {
        log::info!("更新を見送り: {reason} (状態 {status:?} / 待ち {queued})");
        toast(app, reason);
        return;
    }

    let Some(state) = app.try_state::<UpdaterState>() else {
        return;
    };
    let ready = match state.inner.lock() {
        Ok(mut inner) => inner.ready.take(),
        Err(_) => None,
    };
    let Some(ready) = ready else {
        // 項目は準備済みのときしか押せないので、ここへは来ないはず。
        log::warn!("準備済みの更新が無いのに「更新して再起動」が選ばれた");
        state.set_ready(None);
        return;
    };

    log::info!(
        "更新 v{} を入れます (インストーラを起動して終了します)",
        ready.update.version
    );
    // 成功すればここで exit(0) する。戻ってきたら失敗。
    if let Err(e) = ready.update.install(&ready.bytes) {
        // フックは畳んでいないので、今の版のまま録音は続けられる (モジュールの doc)。
        log::error!("更新を入れられませんでした: {e}");
        toast(
            app,
            &format!("更新を入れられませんでした: {}", describe_error(&e)),
        );
        // もう一度選べるように戻しておく。
        let version = ready.update.version.clone();
        if let Ok(mut inner) = state.inner.lock() {
            inner.ready = Some(ready);
        }
        state.set_ready(Some(&version));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifies_each_version_once() {
        assert!(should_notify("0.7.1", None));
        assert!(!should_notify("0.7.1", Some("0.7.1")));
        // 知らせたあとにさらに新しい版が出たら、また知らせる
        assert!(should_notify("0.7.2", Some("0.7.1")));
    }

    #[test]
    fn downloads_only_when_the_ready_version_differs() {
        assert!(needs_download("0.7.1", None));
        assert!(!needs_download("0.7.1", Some("0.7.1")));
        assert!(needs_download("0.7.2", Some("0.7.1")));
    }

    #[test]
    fn install_is_refused_while_recording_or_processing() {
        assert!(can_install_now(Status::Recording, 0).is_err());
        assert!(can_install_now(Status::Processing, 0).is_err());
        assert!(can_install_now(Status::Idle, 0).is_ok());
    }

    #[test]
    fn install_is_refused_while_jobs_are_queued_even_if_idle() {
        // ワーカーは仕事ごとに Idle へ戻すので、表示が待機中でもキューが残りうる
        assert!(can_install_now(Status::Idle, 1).is_err());
        assert!(can_install_now(Status::Recording, 3).is_err());
    }

    #[test]
    fn manual_check_always_reports() {
        let up = report_message(Trigger::Manual, &CheckOutcome::UpToDate, "0.7.0", false);
        assert_eq!(up.as_deref(), Some("最新です (v0.7.0)"));

        let failed = CheckOutcome::Failed { reason: "x".into() };
        let msg = report_message(Trigger::Manual, &failed, "0.7.0", false).unwrap();
        assert!(msg.contains("確認できませんでした"), "{msg}");

        // 手動なら、知らせ済みの版でも答える (選んだのに無反応にしない)
        let ready = CheckOutcome::Ready {
            version: "0.7.1".into(),
        };
        let msg = report_message(Trigger::Manual, &ready, "0.7.0", true).unwrap();
        assert!(msg.contains("v0.7.1"), "{msg}");
    }

    #[test]
    fn scheduled_check_reports_only_new_versions() {
        let ready = CheckOutcome::Ready {
            version: "0.7.1".into(),
        };
        assert!(report_message(Trigger::Scheduled, &ready, "0.7.0", false).is_some());
        assert!(report_message(Trigger::Scheduled, &ready, "0.7.0", true).is_none());
        // 最新・失敗は定期では黙る (オフラインの PC で 6 時間ごとに騒がない)
        assert!(
            report_message(Trigger::Scheduled, &CheckOutcome::UpToDate, "0.7.0", false).is_none()
        );
        let failed = CheckOutcome::Failed {
            reason: "offline".into(),
        };
        assert!(report_message(Trigger::Scheduled, &failed, "0.7.0", false).is_none());
    }

    #[test]
    fn labels() {
        assert_eq!(check_item_label(false), "更新を確認");
        assert_eq!(check_item_label(true), "更新を確認中…");
        assert_eq!(install_item_label(None), "更新して再起動");
        assert_eq!(install_item_label(Some("0.7.1")), "更新して再起動 (v0.7.1)");
    }

    /// tauri.conf.json の plugins.updater が、Cargo.lock で固定したプラグインの
    /// 設定として読めること。読めないと setup の `handle.plugin(...)?` で落ち、
    /// **アプリが起動しなくなる** (実行時にしか分からない壊れ方なのでここで押さえる)。
    #[test]
    fn tauri_conf_updater_section_parses_with_the_pinned_plugin() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json");
        let conf: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let section = conf["plugins"]["updater"].clone();
        let parsed: tauri_plugin_updater::Config = serde_json::from_value(section).unwrap();
        assert!(
            parsed.require_signed_version,
            "署名に記録された版を必ず照合する"
        );
        assert_eq!(parsed.endpoints.len(), 1);
        assert_eq!(
            parsed.endpoints[0].as_str(),
            "https://github.com/noxitro/nox-voice/releases/latest/download/latest.json"
        );
        assert!(!parsed.pubkey.is_empty());
        let windows = parsed.windows.expect("windows の設定がある");
        // 型は crate の外へ出ていないので、Debug の表記で確かめる。
        assert_eq!(format!("{:?}", windows.install_mode), "Passive");
    }

    /// scripts/updater-manifest.mjs が書く形 (版に v 無し・pub_date はミリ秒無しの
    /// RFC 3339・3 つのキー) を、固定したプラグインが読めること。
    #[test]
    fn manifest_shape_written_by_the_script_parses() {
        let json = serde_json::json!({
            "version": "0.7.1",
            "pub_date": "2026-10-18T09:56:17Z",
            "platforms": {
                "windows-x86_64-nsis": { "signature": "c2ln", "url": "https://github.com/noxitro/nox-voice/releases/download/v0.7.1/nox-voice_0.7.1_x64-setup.exe" },
                "windows-x86_64-msi": { "signature": "c2ln", "url": "https://github.com/noxitro/nox-voice/releases/download/v0.7.1/nox-voice_0.7.1_x64_en-US.msi" },
                "windows-x86_64": { "signature": "c2ln", "url": "https://github.com/noxitro/nox-voice/releases/download/v0.7.1/nox-voice_0.7.1_x64-setup.exe" }
            }
        });
        let release: tauri_plugin_updater::RemoteRelease = serde_json::from_value(json).unwrap();
        assert_eq!(release.version.to_string(), "0.7.1");
        assert!(release.pub_date.is_some(), "pub_date が読めている");
        let msi = release.download_url("windows-x86_64-msi").unwrap();
        assert!(msi.as_str().ends_with(".msi"), "{msi}");
        let fallback = release.download_url("windows-x86_64").unwrap();
        assert!(fallback.as_str().ends_with("-setup.exe"), "{fallback}");
    }

    #[test]
    fn release_not_found_is_described_in_japanese() {
        let msg = describe_error(&tauri_plugin_updater::Error::ReleaseNotFound);
        assert!(msg.contains("latest.json"), "{msg}");
    }
}
