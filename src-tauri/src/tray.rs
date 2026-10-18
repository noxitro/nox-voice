//! トレイ常駐。メインウィンドウを閉じてもプロセスは生き続ける。

use tauri::menu::{MenuBuilder, MenuItem, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::session::Status;

const MENU_ID_SETTINGS: &str = "settings";
const MENU_ID_HISTORY: &str = "history";
const MENU_ID_QUIT: &str = "quit";
const MENU_ID_UPDATE_CHECK: &str = "update-check";
const MENU_ID_UPDATE_INSTALL: &str = "update-install";

/// 構築後に書き換えるメニュー項目。
pub struct TrayItems {
    /// 「状態: 録音中」等。テキストを更新して反映する。
    pub status: MenuItem<Wry>,
    /// 「更新を確認」。確認中は無効にする ([`crate::updater`])。
    pub update_check: MenuItem<Wry>,
    /// 「更新して再起動」。更新が手元に揃うまで無効のまま置いておく。
    pub update_install: MenuItem<Wry>,
}

/// トレイアイコンとメニューを構築し、後から書き換える項目を返す。
pub fn build(app: &AppHandle) -> tauri::Result<TrayItems> {
    let status_item = MenuItemBuilder::with_id("status", status_label(Status::Idle))
        .enabled(false)
        .build(app)?;
    // 更新の 2 項目は、文言も有効/無効も updater.rs が決める (判断を 1 か所に置く)。
    // 「更新して再起動」を最初から出しておくのは、項目が増えたり消えたりすると
    // 「さっきあった項目が無い」になって探させるため。無効の灰色で待たせる。
    let update_check_item = MenuItemBuilder::with_id(
        MENU_ID_UPDATE_CHECK,
        crate::updater::check_item_label(false),
    )
    .build(app)?;
    let update_install_item = MenuItemBuilder::with_id(
        MENU_ID_UPDATE_INSTALL,
        crate::updater::install_item_label(None),
    )
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
        .item(&update_check_item)
        .item(&update_install_item)
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
            MENU_ID_UPDATE_CHECK => crate::updater::check_now(app),
            MENU_ID_UPDATE_INSTALL => crate::updater::install_now(app),
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

    Ok(TrayItems {
        status: status_item,
        update_check: update_check_item,
        update_install: update_install_item,
    })
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
