//! [`crate::screen`] の Win32 実装。
//!
//! ここは実機でしか動かない層なので、**判断は一切持たせない**。
//! 「どの窓を読むか」「読めたと認めるか」「画像が要るか」は親モジュールの
//! 純関数 ([`scan_decision`] / [`is_usable_text`] / [`needs_screenshot`]) にあり、
//! ここは値を集めて渡し、結果を組み立てるだけにしてある。
//! そうしないと、この機能の判断部分が丸ごとテスト不能になる。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, SRCCOPY,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowRect, IsIconic, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

use super::{
    cap_total_text, describe_position, downscale_bgra_bottom_up, encode_png, is_usable_text,
    claim_scan, may_release_scan, needs_screenshot, now_ms, plan_scale, scan_decision, MonitorPick,
    ScanDecision, ScanHandle, ScannedWindow, ScreenScan, Screenshot, SkipReason, WindowCandidate,
    MAX_IMAGE_EDGE, MAX_TEXT_PER_WINDOW, MAX_TOTAL_TEXT, MAX_WINDOWS, SCAN_BUDGET, SCAN_IN_FLIGHT,
    STALE_AFTER,
};
use crate::context::ContextSource;

use std::sync::atomic::Ordering;

/// 1 ウィンドウあたりに見る子ウィンドウの数。
///
/// 本文を持つコントロールは普通いちばん手前の数個に見つかる。
/// 上限が無いと、コントロールを何百個も持つアプリ 1 つで予算を食い潰す。
const MAX_CHILDREN_PER_WINDOW: usize = 24;

/// 画面の走査を**開始する**。呼び出し側はブロックしない。
///
/// 戻り値の [`ScanHandle`] を [`ScanHandle::wait`] で回収する。
/// 走査は録音と並行して進むので、**録音開始は 1 ms も遅れない** —
/// deep context ([`crate::context::capture`]) が録音開始を最大 300ms
/// 待たせるのとはここが決定的に違う。画面質問モードは複数ウィンドウを
/// 読むので秒単位になりうるが、その時間は録音中に隠れる。
///
/// `enabled` が false なら `None` を返し、Win32 にも COM にも触れない。
pub fn start_scan(enabled: bool) -> Option<ScanHandle> {
    if !enabled {
        return None;
    }

    // 打ち切った走査は裏で走り続ける。積み上げない ([`SCAN_IN_FLIGHT`])。
    // ただし**返ってこない 1 本に永久に塞がれない**よう、古い占有は横取りする
    // ([`claim_scan`] の doc: UIA 呼び出し 1 回そのものには打ち切りが無い)。
    let Some(claim) = claim_in_flight() else {
        log::info!("前回の画面走査が未完了のため今回はスキップします");
        return None;
    };

    let (tx, rx) = crossbeam_channel::bounded::<ScreenScan>(1);
    let started = Instant::now();
    let spawned = std::thread::Builder::new()
        .name("nox-screen-scan".to_string())
        .spawn(move || {
            let scan = scan_blocking(started);
            // 打ち切られていても、ここまで来たら次の走査を許可する。
            release_in_flight(claim);
            // 受信側が既に見切っていれば送信は失敗する。それでよい。
            let _ = tx.send(scan);
        });

    match spawned {
        Ok(_) => Some(ScanHandle::new(rx, started)),
        Err(e) => {
            release_in_flight(claim);
            log::warn!("画面走査スレッドを起動できません: {e}");
            None
        }
    }
}

/// 走査の占有を取る。取れたら自分の印を返す。
fn claim_in_flight() -> Option<u64> {
    let stale = STALE_AFTER.as_millis() as u64;
    loop {
        let current = SCAN_IN_FLIGHT.load(Ordering::SeqCst);
        let claim = claim_scan(current, now_ms(), stale)?;
        // CAS で取る。素朴な store だと、同時に 2 本が「横取りできる」と
        // 判断したときに両方走ってしまう。
        if SCAN_IN_FLIGHT
            .compare_exchange(current, claim, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if current != 0 {
                log::warn!(
                    "前回の画面走査が {} 秒を過ぎても返らないため、占有を横取りします                      (返らないスレッドは裏に残ります)",
                    STALE_AFTER.as_secs()
                );
            }
            return Some(claim);
        }
        // 誰かに先を越された。取り直す。
    }
}

/// 占有を手放す。**横取りされていたら何もしない** — 0 を書くと、
/// 走り出したばかりの新しい走査の占有を消してしまう。
fn release_in_flight(claim: u64) {
    if !may_release_scan(SCAN_IN_FLIGHT.load(Ordering::SeqCst), claim) {
        return;
    }
    // 読んでから書くまでに横取りされる余地があるので、書き込みも CAS で行う。
    let _ = SCAN_IN_FLIGHT.compare_exchange(claim, 0, Ordering::SeqCst, Ordering::SeqCst);
}

/// 走査の本体。専用スレッドで呼ばれる前提。
fn scan_blocking(started: Instant) -> ScreenScan {
    // SAFETY: このスレッドで最初の初期化。対で CoUninitialize する。
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        log::warn!("COM を初期化できません: {com:?}");
        return ScreenScan::failed("COM を初期化できませんでした");
    }
    let scan = scan_monitor(started);
    // SAFETY: 上の CoInitializeEx と対。
    unsafe { CoUninitialize() };
    scan
}

fn scan_monitor(started: Instant) -> ScreenScan {
    let Some((monitor, pick)) = choose_monitor() else {
        return ScreenScan::failed("読み取るモニタを特定できませんでした");
    };
    let Some(rect) = monitor_rect(monitor) else {
        return ScreenScan::failed("モニタの大きさを取得できませんでした");
    };

    // **スクリーンショットは先に撮る。** 要るかどうかは UIA を全部
    // 試すまで分からないが、後から撮ると「ホットキーを押した瞬間の画面」
    // ではなく「数秒後の画面」になる。ユーザーが質問しているのは前者で、
    // その間にトーストが出たり画面が切り替わったりすれば答えは変わる。
    // 撮影自体は数十 ms なので、捨てることになっても損は小さい。
    let shot = capture_monitor(rect);

    let candidates = collect_candidates(monitor, rect);
    let candidate_count = candidates.len();
    let mut windows = Vec::new();

    match automation() {
        Some(automation) => {
            for candidate in candidates {
                if windows.len() >= MAX_WINDOWS {
                    log::debug!("ウィンドウ数の上限 ({MAX_WINDOWS}) に達したので打ち切ります");
                    break;
                }
                // 予算を超えたら、そこまでで確定させる。読めた分は資料として
                // 十分に役に立つので、全部捨てるのは損。
                if started.elapsed() >= SCAN_BUDGET {
                    log::info!("画面走査が予算を超えたので、読めたところまでで確定します");
                    break;
                }
                let (text, route) = read_window(&automation, candidate.hwnd);
                let usable = is_usable_text(&text, route);
                windows.push(ScannedWindow {
                    title: candidate.title,
                    process: crate::foreground::process_image_name(candidate.process_id)
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    position: candidate.position,
                    text: if usable { text } else { String::new() },
                    route,
                });
            }
        }
        None => {
            // UIA が使えなくても、画像があれば質問には答えられる。
            // ここで空を返すと、スクリーンショット経路まで道連れになる。
            log::warn!("UI Automation を生成できないため、画面の読み取りは画像だけになります");
        }
    }

    cap_total_text(&mut windows, MAX_TOTAL_TEXT);

    // 画像の要否は**切り詰めたあとの状態**で決める。上限で本文を落とした
    // ウィンドウは、UIA としては読めていても資料には残っていない。
    // 切り詰め前の判定で「全部読めた」と結論すると、落とした窓について
    // 訊かれたときに手がかりが何も無くなる。
    let readable: Vec<bool> = windows
        .iter()
        .map(|w| !w.text.trim().is_empty())
        .collect();

    // 件数上限・時間予算・UIA が使えない、のいずれで打ち切られても、
    // **手つかずの候補は「読めなかった窓」と同じ扱い**にする
    // ([`needs_screenshot`] の doc)。
    let unscanned = candidate_count.saturating_sub(windows.len());

    let screenshot = if needs_screenshot(&readable, unscanned) {
        shot
    } else {
        // 全部読めた。画像は捨てる — 送らずに済むならその方がよい
        // (プライバシーでも送信量でも)。
        None
    };

    let failure = (windows.iter().all(|w| w.text.trim().is_empty()) && screenshot.is_none())
        .then(|| "画面から読み取れる内容がありませんでした".to_string());

    // 中身は絶対に出さない。件数と量だけ。
    log::info!(
        "画面走査完了: {} ({} ウィンドウ (未走査 {unscanned}) / {} 文字 / 画像 {} / {} ms)",
        pick.label(),
        windows.len(),
        windows.iter().map(|w| w.text.chars().count()).sum::<usize>(),
        screenshot
            .as_ref()
            .map_or("なし".to_string(), |s| format!(
                "{}x{} {} KB",
                s.width,
                s.height,
                s.png.len() / 1024
            )),
        started.elapsed().as_millis()
    );

    ScreenScan {
        windows,
        screenshot,
        monitor: pick,
        failure,
    }
}

// ---------------------------------------------------------------------------
// モニタの選定
// ---------------------------------------------------------------------------

/// どのモニタを読むか決める。
///
/// # なぜ「前景ウィンドウのモニタ」なのか
///
/// ユーザーは今見ているものについて質問する。前景ウィンドウは
/// 「今見ているもの」に一番近い観測可能な値で、しかも録音開始時に
/// どのみち採っている (R7)。マルチモニタでも、質問の対象が別モニタなら
/// ユーザーは先にそちらをクリックしてから話す。
///
/// # 前景が無いとき
///
/// ロック画面・UAC・デスクトップだけが見えている状態では
/// `GetForegroundWindow` が NULL を返す。ここで諦めると、ペダル運用
/// (どこにもフォーカスを置かずに使う。design.md「用途別ホットキー」) で
/// この機能が使えなくなる。そこで**カーソル位置のモニタ**へ落とす —
/// 手はたいてい見ている画面の上にある。それも取れなければ主モニタ。
///
/// `EnumDisplayMonitors` で全モニタを列挙して全部読む案は採らなかった。
/// 資料が倍以上になり、送信量と待ち時間がそのまま倍になるのに対し、
/// ユーザーの決めた仕様は「モニタ単位」であって「全画面」ではない。
fn choose_monitor() -> Option<(HMONITOR, MonitorPick)> {
    // SAFETY: 引数なし。NULL でありうるので下でチェックする。
    let foreground = unsafe { GetForegroundWindow() };
    if !foreground.0.is_null() {
        // SAFETY: hwnd は非 NULL。DEFAULTTONEAREST は常に有効なモニタを返す。
        let monitor = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
        if !monitor.is_invalid() {
            return Some((monitor, MonitorPick::Foreground));
        }
    }

    let mut point = POINT::default();
    // SAFETY: point はスタック上の有効な POINT。
    if unsafe { GetCursorPos(&mut point) }.is_ok() {
        // SAFETY: DEFAULTTONEAREST なので範囲外の座標でも有効なモニタを返す。
        let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if !monitor.is_invalid() {
            return Some((monitor, MonitorPick::Cursor));
        }
    }

    // SAFETY: DEFAULTTOPRIMARY は座標を問わず主モニタを返す。
    let primary = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    (!primary.is_invalid()).then_some((primary, MonitorPick::Primary))
}

/// モニタの矩形 `(x, y, 幅, 高さ)`。
fn monitor_rect(monitor: HMONITOR) -> Option<(i32, i32, i32, i32)> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: monitor は有効。cbSize を設定済みの MONITORINFO を渡している。
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let r = info.rcMonitor;
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    (w > 0 && h > 0).then_some((r.left, r.top, w, h))
}

// ---------------------------------------------------------------------------
// ウィンドウの列挙
// ---------------------------------------------------------------------------

/// 資料に載せる候補 (選別済み)。
struct Candidate {
    hwnd: isize,
    title: String,
    process_id: u32,
    position: String,
}

/// `EnumWindows` のコールバックへ渡す作業領域。
///
/// `static mut` ではなく `LPARAM` でポインタを渡す。列挙は同期的で
/// 単一スレッドから 1 回だけ走るが、`static mut` にすると 2 か所から
/// 呼んだ瞬間に静かに壊れる (context.rs の診断テストは `#[ignore]` の
/// 単発実行なのでそれで済んでいるだけ)。
struct EnumState {
    own_process_id: u32,
    monitor: HMONITOR,
    monitor_rect: (i32, i32, i32, i32),
    found: Vec<Candidate>,
    /// 外した理由の内訳 (件数だけ)。
    skipped: Vec<SkipReason>,
}

/// 対象モニタ上の可視ウィンドウを、Z オーダー順 (手前から) に集める。
fn collect_candidates(monitor: HMONITOR, rect: (i32, i32, i32, i32)) -> Vec<Candidate> {
    // SAFETY: 引数なし。
    let own_process_id = unsafe { GetCurrentProcessId() };
    let mut state = EnumState {
        own_process_id,
        monitor,
        monitor_rect: rect,
        found: Vec::new(),
        skipped: Vec::new(),
    };

    // SAFETY: コールバックは下の `enum_proc`。LPARAM には state への
    // 生ポインタを載せる。EnumWindows は同期的に完了するので、
    // state はコールバックが走る間ずっと生きている。
    // `&raw mut` を使うのは、中間の参照を作らずにポインタを取るため。
    let ptr = &raw mut state;
    let result = unsafe { EnumWindows(Some(enum_proc), LPARAM(ptr as isize)) };
    if let Err(e) = result {
        // 列挙の途中で止まっても、そこまでに集めた分は使える。
        log::debug!("EnumWindows が途中で終わりました: {e}");
    }
    log::debug!(
        "走査対象 {} 件 / 除外の内訳: {}",
        state.found.len(),
        summarize_skips(&state.skipped)
    );
    state.found
}

/// `EnumWindows` のコールバック。**必ず `true` を返して列挙を続ける。**
///
/// キーボードフックほど厳しくはない (OS 全体の入力経路上ではない) が、
/// **panic が FFI 境界を越えるとプロセスが落ちる**。ここで行うのは
/// 固定長バッファへの読み取りと `Vec` への push だけに留めてある。
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // panic を FFI 境界で止める。`keyboard_hook_proc` と同じ扱いにする —
    // あちらだけ包んであってこちらが素通し、では非対称で、いつか
    // 「なぜかプロセスが落ちる」になる。ここで起こりうる panic は実質
    // 確保の失敗だけだが、そのときも列挙を続けて (= true を返して)
    // 集まった分で走査を成立させる方がよい。
    let kept_going = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: lparam には collect_candidates が載せた &mut EnumState が入る。
        // EnumWindows は同期的なので、参照先は列挙中ずっと有効。
        let state = unsafe { &mut *(lparam.0 as *mut EnumState) };
        collect_one(hwnd, state);
    }));
    if kept_going.is_err() {
        log::warn!("ウィンドウ列挙中に内部エラーが発生しました (この 1 件は飛ばします)");
    }
    true.into()
}

/// 候補 1 件を選別して `state` へ積む ([`enum_proc`] の中身)。
fn collect_one(hwnd: HWND, state: &mut EnumState) {

    let mut process_id = 0u32;
    // SAFETY: hwnd は列挙で得た有効な値。出力先はスタック上の u32。
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            hwnd,
            Some(&mut process_id),
        )
    };

    let mut rect = RECT::default();
    // SAFETY: hwnd は有効。失敗時は Err なので既定値のままになる。
    let has_rect = unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok();

    // SAFETY: hwnd は有効。DEFAULTTONEAREST は常に有効なモニタを返す。
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };

    // SAFETY: hwnd は有効。GWL_EXSTYLE は常に読める。
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;

    let candidate = WindowCandidate {
        hwnd: hwnd.0 as isize,
        title: crate::foreground::window_title(hwnd),
        process_id,
        // SAFETY: hwnd は有効。
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        // SAFETY: hwnd は有効。
        minimized: unsafe { IsIconic(hwnd) }.as_bool(),
        cloaked: is_cloaked(hwnd),
        tool_window: ex_style & WS_EX_TOOLWINDOW.0 != 0,
        width: if has_rect { rect.right - rect.left } else { 0 },
        height: if has_rect { rect.bottom - rect.top } else { 0 },
        on_target_monitor: monitor == state.monitor,
    };

    match scan_decision(&candidate, state.own_process_id) {
        ScanDecision::Scan => {
            let position = describe_position(
                (rect.left, rect.top, candidate.width, candidate.height),
                state.monitor_rect,
            );
            state.found.push(Candidate {
                hwnd: candidate.hwnd,
                title: candidate.title,
                process_id: candidate.process_id,
                position,
            });
        }
        ScanDecision::Skip(reason) => {
            // **どの窓を外したかは出さない。** 出すにはタイトルが要り、
            // タイトルは画面の中身である。理由ごとの件数だけ数えておく —
            // 「自分の窓を読んでいないか」「全部別モニタ扱いになっていないか」
            // は、件数の内訳だけで切り分けられる。
            state.skipped.push(reason);
        }
    }
}

/// 除外理由の内訳を「理由: 件数」の形にする (ログ用)。
fn summarize_skips(skipped: &[SkipReason]) -> String {
    let mut counts: Vec<(SkipReason, usize)> = Vec::new();
    for reason in skipped {
        match counts.iter_mut().find(|(r, _)| r == reason) {
            Some((_, n)) => *n += 1,
            None => counts.push((*reason, 1)),
        }
    }
    counts
        .iter()
        .map(|(r, n)| format!("{}={n}", r.label()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// DWM から見て隠されているか (仮想デスクトップの別ページ、未表示の UWP)。
///
/// `IsWindowVisible` はこれらに対して **true を返す**。判定を入れないと、
/// 「別の仮想デスクトップに置いてあるチャットの中身」まで読んで送ることになる。
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: hwnd は有効。出力先はスタック上の u32 で、サイズも一致させている。
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    // 取れなかったときは「隠されていない」に倒す。DWM が無効な環境
    // (リモートセッション等) で全ウィンドウが消えるのを避ける。
    result.is_ok() && cloaked != 0
}

// ---------------------------------------------------------------------------
// UIA でウィンドウを読む
// ---------------------------------------------------------------------------

fn automation() -> Option<IUIAutomation> {
    // SAFETY: COM は scan_blocking で初期化済み。CLSID は UIA のもの。
    match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
        Ok(a) => Some(a),
        Err(e) => {
            log::warn!("UI Automation を生成できません: {e}");
            None
        }
    }
}

/// 1 ウィンドウ分のテキストを読む。
///
/// トップレベル要素の `Name` はウィンドウタイトルにすぎないので、
/// **本文が読めるかは子コントロールまで降りないと分からない**
/// (design.md Q1 の実測)。降りても駄目なら、そのウィンドウはスクリーン
/// ショット側に任せる。
fn read_window(automation: &IUIAutomation, hwnd: isize) -> (String, ContextSource) {
    let hwnd = HWND(hwnd as *mut _);
    // SAFETY: automation と hwnd は有効。相手が応答しなければ Err。
    let Ok(element) = (unsafe { automation.ElementFromHandle(hwnd) }) else {
        return (String::new(), ContextSource::Unavailable);
    };

    let mut best = read_one(&element);
    if is_usable_text(&best.0, best.1) {
        return best;
    }

    for child in child_windows(hwnd) {
        // SAFETY: child は列挙で得た有効な HWND。
        let Ok(child_element) = (unsafe { automation.ElementFromHandle(child) }) else {
            continue;
        };
        let found = read_one(&child_element);
        if route_rank(found.1) > route_rank(best.1)
            || (route_rank(found.1) == route_rank(best.1)
                && found.0.chars().count() > best.0.chars().count())
        {
            best = found;
        }
        if is_usable_text(&best.0, best.1) {
            break; // 十分読めた。これ以上降りる理由が無い。
        }
    }
    best
}

/// 1 要素からテキストを読み、上限まで切り詰める。
///
/// deep context は「キャレット付近 = 末尾」を残すが、こちらは**先頭を残す**。
/// 画面は上から読むもので、一覧の先頭が消えたら「一覧を出して」の答えに
/// ならないため。
fn read_one(element: &IUIAutomationElement) -> (String, ContextSource) {
    // パスワード欄は絶対に読まない。deep context と**同じ判定**を通す
    // ([`crate::context::must_not_read`])。片方だけが避けていると、UI の
    // 「パスワード欄は読み取りません」という約束が用途によって嘘になる。
    //
    // なお**スクリーンショットに写った「表示中のパスワード」はこの仕組みでは
    // 防げない**。UI の警告文にその旨を書いてある。
    if crate::context::must_not_read(element) {
        return (String::new(), ContextSource::PasswordSkipped);
    }
    let context = crate::context::read_element(element);
    let text: String = context
        .text
        .replace('\r', "")
        .chars()
        .take(MAX_TEXT_PER_WINDOW)
        .collect();
    (text.trim().to_string(), context.source)
}

/// 経路の望ましさ。大きいほど本文に近い。
fn route_rank(source: ContextSource) -> u8 {
    match source {
        ContextSource::TextPattern => 3,
        ContextSource::ValuePattern => 2,
        ContextSource::ElementName => 1,
        _ => 0,
    }
}

/// 子ウィンドウを深さ 1 段だけ列挙する (上限つき)。
fn child_windows(parent: HWND) -> Vec<HWND> {
    let mut found: Vec<HWND> = Vec::new();

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam には child_windows が載せた &mut Vec<HWND> が入る。
        // EnumChildWindows は同期的なので参照先は列挙中ずっと有効。
        let found = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        if found.len() >= MAX_CHILDREN_PER_WINDOW {
            return false.into(); // 打ち切る。
        }
        found.push(hwnd);
        true.into()
    }

    // SAFETY: コールバックは上の定義。parent は有効な HWND。
    // 列挙は同期的なので、found はコールバックが走る間ずっと生きている。
    let ptr = &raw mut found;
    unsafe {
        let _ = EnumChildWindows(Some(parent), Some(collect), LPARAM(ptr as isize));
    }
    found
}

// ---------------------------------------------------------------------------
// スクリーンショット
// ---------------------------------------------------------------------------

/// モニタ 1 枚を撮って PNG にする。失敗しても `None` を返すだけ。
///
/// `CAPTUREBLT` を付けるのは、レイヤードウィンドウ (半透明のツール窓、
/// 一部の IME 候補窓) を含めるため。付けないと、画面に見えているものと
/// 撮れたものが食い違う。
fn capture_monitor(rect: (i32, i32, i32, i32)) -> Option<Screenshot> {
    let (x, y, width, height) = rect;
    if width <= 0 || height <= 0 {
        return None;
    }

    // SAFETY: None は画面全体の DC を意味する。対で ReleaseDC する。
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        log::warn!("画面の DC を取得できません");
        return None;
    }
    let result = capture_with_screen_dc(screen, x, y, width, height);
    // SAFETY: 上の GetDC と対。以降 screen は使わない。
    unsafe { ReleaseDC(None, screen) };
    result
}

fn capture_with_screen_dc(
    screen: HDC,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Option<Screenshot> {
    // SAFETY: screen は有効な DC。対で DeleteDC する。
    let mem = unsafe { CreateCompatibleDC(Some(screen)) };
    if mem.is_invalid() {
        return None;
    }
    // SAFETY: screen は有効。サイズは正であることを確認済み。
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    if bitmap.is_invalid() {
        // SAFETY: mem は上で作った有効な DC。
        unsafe {
            let _ = DeleteDC(mem);
        }
        return None;
    }

    // SAFETY: mem と bitmap は有効。戻り値は元のオブジェクトで、
    // 画素を読む前に選び直す (下のコメント参照)。
    let old = unsafe { SelectObject(mem, bitmap.into()) };

    // 転写はビットマップを選択した状態で行う。
    // SAFETY: 両 DC とも有効。転送元はモニタの矩形。
    let blit = unsafe { BitBlt(mem, 0, 0, width, height, Some(screen), x, y, SRCCOPY | CAPTUREBLT) };

    // **画素を読む前に選択を外す。** `GetDIBits` は「ビットマップが DC に
    // 選択されていてはならない」と明記されている API で、選択したまま呼ぶと
    // ドライバによっては 0 を返す。そうなるとスクリーンショットが常に
    // `None` になり、**UIA で読めないアプリ (Chrome・Electron 系) への
    // 質問が全部「読み取れませんでした」で終わる** — この機能の主要な
    // 用途が環境によって丸ごと死ぬ。
    //
    // SAFETY: mem は有効。old は直前の SelectObject の戻り値。
    unsafe { SelectObject(mem, old) };

    let pixels = blit
        .ok()
        .and_then(|()| read_bitmap_pixels(mem, bitmap, width, height));

    // SAFETY: bitmap はもう DC に選択されていないので削除できる。
    unsafe {
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem);
    }

    let pixels = pixels?;
    let (dw, dh) = plan_scale(width as u32, height as u32, MAX_IMAGE_EDGE);
    let rgb = downscale_bgra_bottom_up(&pixels, width as u32, height as u32, dw, dh)?;
    match encode_png(&rgb, dw, dh) {
        Ok(png) => Some(Screenshot {
            png,
            width: dw,
            height: dh,
        }),
        Err(e) => {
            log::warn!("スクリーンショットを PNG にできません: {e}");
            None
        }
    }
}

/// DIB として画素を吸い出す (32bpp BGRA、ボトムアップ)。
fn read_bitmap_pixels(
    mem: HDC,
    bitmap: HBITMAP,
    width: i32,
    height: i32,
) -> Option<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // 正の高さ = ボトムアップ。
            // 負にすればトップダウンで取れるが、ドライバによっては
            // 対応が怪しいので、既定の向きで取って自前で反転させる
            // ([`downscale_bgra_bottom_up`])。
            biHeight: height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    // SAFETY: mem / bitmap は有効。pixels は width*height*4 バイト確保済みで、
    // info の記述 (32bpp / BI_RGB / 同サイズ) と一致している。
    let copied = unsafe {
        GetDIBits(
            mem,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut std::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if copied == 0 {
        log::warn!("画面の画素を取得できません (GetDIBits が 0 を返した)");
        return None;
    }
    Some(pixels)
}
