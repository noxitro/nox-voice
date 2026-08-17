//! 録音中・処理中を知らせる小窓 (オーバーレイ)。
//!
//! 画面下部中央に常時最前面で出す。トレイ常駐アプリなので、メインウィンドウは
//! 普段閉じている。「今わたしの声を録っているのか」が分からないまま喋るのは
//! 体験として最悪なので、状態だけを最小限に見せる。
//!
//! # フォーカスを絶対に奪わない
//!
//! これは見た目の問題ではなく**正しさの問題**。録音開始時に保存した前景 HWND と
//! 貼付直前の前景を照合する (design.md R7) 設計なので、オーバーレイが前景を
//! 取ると照合が外れて貼付が中止される。したがって:
//!
//! - `focusable(false)` — クリックしても入力フォーカスを取らない
//! - `set_ignore_cursor_events(true)` — クリックがそのまま下のアプリへ抜ける
//!   (`WS_EX_TRANSPARENT` 相当)。オーバーレイの上でも普通に操作できる
//! - 表示・非表示は `show()` / `hide()` のみで、`set_focus()` は呼ばない
//!
//! この 3 つが効いているので、`GetForegroundWindow` がオーバーレイを返すことは
//! ない。**R7 の照合側にオーバーレイの除外処理は要らない** (除外リストを持つと、
//! 「フォーカスを取らない」という不変条件が破れたことに気づけなくなる)。
//!
//! # 録音開始を遅らせない
//!
//! ウィンドウ生成は起動時に一度だけ行い、以降は表示/非表示の切り替えだけにする。
//! 録音開始のたびに作ると、その分だけ最初の一言が失われる。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// オーバーレイウィンドウのラベル。
pub const WINDOW_LABEL: &str = "overlay";

/// 小窓の大きさ (論理ピクセル)。
const WIDTH: f64 = 320.0;
const HEIGHT: f64 = 72.0;
/// 画面下端からの余白。タスクバーに隠れない程度に上げる。
const BOTTOM_MARGIN: f64 = 96.0;

/// 表示・自動非表示の世代。
///
/// 「結果を見せてから畳む」タイマーは**Rust 側が持つ**。webview に持たせると、
/// 次の録音で小窓を出した直後に前回のタイマーが発火して消してしまう
/// (webview は自分が何代目の表示かを知らない)。
/// 表示のたびに世代を進め、タイマーは自分の世代のときだけ隠す。
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// オーバーレイを作る (非表示の状態で)。
///
/// 失敗してもアプリは動く。オーバーレイは補助表示なので、
/// 出せないことを理由に録音機能を止めない。
pub fn create(app: &AppHandle) -> tauri::Result<()> {
    if app.get_webview_window(WINDOW_LABEL).is_some() {
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        app,
        WINDOW_LABEL,
        WebviewUrl::App("overlay.html".into()),
    )
    .title("nox-voice overlay")
    .inner_size(WIDTH, HEIGHT)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    // フォーカスを奪わない。R7 の HWND 照合が壊れるため必須。
    .focusable(false)
    .resizable(false)
    .shadow(false)
    .visible(false)
    .build()?;

    // クリックを下のアプリへ通す。作業の邪魔をしない。
    if let Err(e) = window.set_ignore_cursor_events(true) {
        log::warn!("オーバーレイのクリックスルーを設定できません: {e}");
    }

    position_bottom_center(&window);
    log::info!("オーバーレイを作成しました (非表示)");
    Ok(())
}

/// 画面下部中央へ置く。
fn position_bottom_center(window: &tauri::WebviewWindow) {
    let monitor = match window.primary_monitor() {
        Ok(Some(m)) => m,
        Ok(None) => {
            log::warn!("プライマリモニタを取得できません。既定位置のままにします");
            return;
        }
        Err(e) => {
            log::warn!("モニタ情報を取得できません ({e})。既定位置のままにします");
            return;
        }
    };

    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);
    let position = monitor.position().to_logical::<f64>(scale);

    let (x, y) = bottom_center(position.x, position.y, size.width, size.height);
    if let Err(e) = window.set_position(tauri::LogicalPosition::new(x, y)) {
        log::warn!("オーバーレイの位置を設定できません: {e}");
    }
}

/// モニタの矩形から小窓の左上座標を出す (純関数なのでテストできる)。
///
/// 画面が小さい場合でも上端より上へ出さない。マルチモニタでは
/// モニタ原点が負になることがあるので、原点を足してから収める。
fn bottom_center(
    monitor_x: f64,
    monitor_y: f64,
    monitor_width: f64,
    monitor_height: f64,
) -> (f64, f64) {
    let x = monitor_x + ((monitor_width - WIDTH) / 2.0).max(0.0);
    // 余白を取ると上へはみ出す小さい画面では、余白を諦めて画面内に収める。
    let y_offset = (monitor_height - HEIGHT - BOTTOM_MARGIN).max(0.0);
    (x, monitor_y + y_offset)
}

/// 表示する。**フォーカスは取らない** (`set_focus` を呼ばないこと)。
///
/// 保留中の自動非表示タイマーは、世代が進むことで無効になる。
pub fn show(app: &AppHandle) {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    // 位置は表示のたびに直す。モニタ構成が変わっていることがある。
    position_bottom_center(&window);
    if let Err(e) = window.show() {
        log::warn!("オーバーレイを表示できません: {e}");
    }
}

/// 即座に隠す。
pub fn hide(app: &AppHandle) {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    hide_now(app);
}

fn hide_now(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else {
        return;
    };
    if let Err(e) = window.hide() {
        log::warn!("オーバーレイを隠せません: {e}");
    }
}

/// `delay` 後に隠す。ただし**その間に表示が更新されたら何もしない**。
pub fn hide_after(app: &AppHandle, delay: Duration) {
    let generation = GENERATION.load(Ordering::SeqCst);
    let worker_app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("nox-overlay-hide".to_string())
        .spawn(move || {
            std::thread::sleep(delay);
            // 自分より後に show/hide があったなら、畳むのは自分の役目ではない。
            if GENERATION.load(Ordering::SeqCst) == generation {
                hide_now(&worker_app);
            }
        });
    if let Err(e) = spawned {
        // 予約できないなら出しっぱなしより即座に畳む方がまし。
        log::warn!("オーバーレイの自動非表示を予約できません: {e}");
        hide_now(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_is_stable() {
        // フロント (overlay.html) と Rust 側で共有する識別子。
        // 変えると片方だけ壊れて、小窓が二重に作られる。
        assert_eq!(WINDOW_LABEL, "overlay");
    }

    #[test]
    fn showing_invalidates_a_pending_hide() {
        // m3 回帰: 次の録音で出した小窓を、前回のタイマーが消してはいけない。
        let before = GENERATION.load(Ordering::SeqCst);
        GENERATION.fetch_add(1, Ordering::SeqCst); // show 相当
        assert_ne!(
            GENERATION.load(Ordering::SeqCst),
            before,
            "表示で世代が進んでいない"
        );
    }

    #[test]
    fn it_sits_bottom_center_on_a_normal_screen() {
        let (x, y) = bottom_center(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(x, (1920.0 - WIDTH) / 2.0, "水平中央でない");
        assert_eq!(y, 1080.0 - HEIGHT - BOTTOM_MARGIN);
        // 画面内に収まっていること。
        assert!(x >= 0.0 && x + WIDTH <= 1920.0);
        assert!(y >= 0.0 && y + HEIGHT <= 1080.0);
    }

    #[test]
    fn it_respects_a_secondary_monitor_origin() {
        // 左や上にあるモニタでは原点が負になる。
        let (x, y) = bottom_center(-1920.0, -200.0, 1920.0, 1080.0);
        assert_eq!(x, -1920.0 + (1920.0 - WIDTH) / 2.0);
        assert_eq!(y, -200.0 + 1080.0 - HEIGHT - BOTTOM_MARGIN);
    }

    #[test]
    fn it_stays_on_screen_when_the_display_is_tiny() {
        // 余白を引くと上へはみ出すような画面でも、原点より上へ出さない。
        let (x, y) = bottom_center(0.0, 0.0, 200.0, 100.0);
        assert!(x >= 0.0, "左へはみ出した: {x}");
        assert!(y >= 0.0, "上へはみ出した: {y}");
    }
}
