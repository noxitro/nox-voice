//! トレイ常駐。メインウィンドウを閉じてもプロセスは生き続ける。

use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::session::Status;

const MENU_ID_SETTINGS: &str = "settings";
const MENU_ID_HISTORY: &str = "history";
const MENU_ID_QUIT: &str = "quit";

/// トレイアイコンとメニューを構築し、状態表示用のメニュー項目を返す。
///
/// 返した [`MenuItem`] のテキストを更新することで「状態: 録音中」等を反映する。
pub fn build(app: &AppHandle) -> tauri::Result<MenuItem<Wry>> {
    let status_item = MenuItemBuilder::with_id("status", status_label(Status::Idle))
        .enabled(false)
        .build(app)?;
    let settings_item = MenuItemBuilder::with_id(MENU_ID_SETTINGS, "設定を開く").build(app)?;
    // 通知はクリックしても遷移できないので、履歴への入口をトレイにも置く。
    let history_item = MenuItemBuilder::with_id(MENU_ID_HISTORY, "履歴を開く").build(app)?;
    let quit_item = MenuItemBuilder::with_id(MENU_ID_QUIT, "終了").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&status_item)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&history_item)
        .item(&settings_item)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&quit_item)
        .build()?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    TrayIconBuilder::with_id("nox-voice-tray")
        .icon(icon)
        .tooltip("nox-voice")
        // 左クリックはメニューではなくウィンドウ表示に割り当てる。
        .show_menu_on_left_click(false)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_ID_SETTINGS => show_main_window(app),
            MENU_ID_HISTORY => {
                show_main_window(app);
                if let Err(e) = app.emit(crate::EVENT_SHOW_HISTORY, ()) {
                    log::warn!("履歴表示イベントの送出に失敗: {e}");
                }
            }
            MENU_ID_QUIT => {
                log::info!("トレイメニューから終了");
                app.exit(0);
            }
            other => log::debug!("未処理のトレイメニューイベント: {other}"),
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(status_item)
}

/// トレイの状態表示を更新する。
pub fn update_status(item: &MenuItem<Wry>, status: Status) {
    if let Err(e) = item.set_text(status_label(status)) {
        log::warn!("トレイの状態表示を更新できません: {e}");
    }
}

fn status_label(status: Status) -> String {
    format!("状態: {}", status.label())
}

/// メインウィンドウを表示して前面に出す。
pub fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        log::warn!("メインウィンドウが見つかりません");
        return;
    };
    if let Err(e) = window.show() {
        log::warn!("ウィンドウの表示に失敗: {e}");
    }
    if let Err(e) = window.unminimize() {
        log::debug!("unminimize に失敗 (最小化されていない可能性): {e}");
    }
    if let Err(e) = window.set_focus() {
        log::warn!("ウィンドウのフォーカス取得に失敗: {e}");
    }
}
