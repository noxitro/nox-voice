//! グローバルホットキー (PTT) — `WH_KEYBOARD_LL` 低レベルキーボードフック。
//!
//! `RegisterHotKey` はキーアップを取れず PTT にできないため低レベルフックを使う (設計の技術メモ)。
//!
//! # スレッド構成
//!
//! ```text
//! [フックスレッド]  SetWindowsHookExW + GetMessageW ループ
//!        │ フックコールバックは「フィルタして送るだけ」で即 return
//!        ▼ crossbeam channel (unbounded / 非ブロッキング送信)
//! [コントローラスレッド]  PttInterpreter で押下パターンを解釈 → 録音制御
//! ```
//!
//! フックコールバックは OS 全体のキー入力経路上で走るため、ここをブロックすると
//! システム全体の入力が遅延する。したがってコールバック内では
//! **確保・ロック・IO・panic しうる操作を一切行わない**。

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RCONTROL;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// 短押し判定のしきい値。これ未満で離すとトグルモードに入る。
pub const TAP_THRESHOLD: Duration = Duration::from_millis(300);

/// 既定のホットキー: 右 Ctrl。
pub const DEFAULT_HOTKEY_VK: u32 = VK_RCONTROL.0 as u32;

/// 現在のホットキー。設定から差し替えられる。
///
/// フックは 1 度しか設置しない (再設置は OS 全体の入力経路を触り直すことになる)。
/// 比較する仮想キーだけを atomic で差し替えれば、変更は次のキー入力から効く。
static HOTKEY_VK: AtomicU32 = AtomicU32::new(DEFAULT_HOTKEY_VK);

/// キー捕獲モード。設定 UI の「キーを押して設定」で使う。
///
/// ON の間、フックは PTT の解釈をやめて**押されたキーをそのまま報告する**。
/// 捕獲中に録音が始まってしまうのを防ぐため、モードは排他にする。
static CAPTURE_MODE: AtomicBool = AtomicBool::new(false);

/// 捕獲セッションの世代。タイムアウトが**古い**捕獲を打ち切らないようにする。
static CAPTURE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 「離されるまで無視する」キー (0 = なし)。
///
/// 捕獲を確定した瞬間、そのキーはまだ物理的に押されたままである。
/// 捕獲モードを抜けた直後にオートリピートの keydown が通常経路へ流れると、
/// **設定しただけで録音が始まり、短押し判定でトグルにラッチする**。
/// 離すまで通常経路から締め出して、その事故を防ぐ。
static SUPPRESS_UNTIL_RELEASE: AtomicU32 = AtomicU32::new(0);

/// 録音キャンセルキーの既定。Esc。
///
/// ホットキーには選べないキー ([`is_allowed_hotkey`] が弾く) だが、
/// キャンセル専用としては「取り消し」の意味がそのまま生きる。
pub const DEFAULT_CANCEL_VK: u32 = 0x1B;

/// 現在のキャンセルキー (0 = 無効)。設定から差し替えられる。
static CANCEL_VK: AtomicU32 = AtomicU32::new(DEFAULT_CANCEL_VK);
/// キャンセルキーの押下状態。オートリピート除去用 ([`KEY_IS_DOWN`] と同じ手法)。
static CANCEL_IS_DOWN: AtomicBool = AtomicBool::new(false);
/// 録音中のみ立てる。キャンセルキーは録音中しか効かない (武装フラグ)。
static RECORDING_ACTIVE: AtomicBool = AtomicBool::new(false);

/// ホットキーを差し替える。フックの再設置は不要。
pub fn set_hotkey_vk(vk: u32) {
    HOTKEY_VK.store(vk, Ordering::SeqCst);
    // 押しっぱなしの状態が残っていると、次の離しだけが届いて状態がねじれる。
    KEY_IS_DOWN.store(false, Ordering::SeqCst);
    log::info!("ホットキーを変更: {} (VK 0x{vk:02X})", key_label(vk));
}

/// キャンセルキーを差し替える (0 = 無効)。フックの再設置は不要。
pub fn set_cancel_vk(vk: u32) {
    CANCEL_VK.store(vk, Ordering::SeqCst);
    // 押しっぱなしの状態が残っていると、次の離しだけが届いて状態がねじれる。
    CANCEL_IS_DOWN.store(false, Ordering::SeqCst);
    log::info!("キャンセルキーを変更: {} (VK 0x{vk:02X})", key_label(vk));
}

/// 録音の有無をフックへ教える。**録音中のみ**キャンセルキーが効く。
///
/// 非武装へ戻すときは押下状態も畳む。録音終了の瞬間にキャンセルキーが
/// 押しっぱなしだった場合、そのままにしておくと次の録音の最初の keydown が
/// オートリピート扱いで捨てられ、キャンセルが一度効かなくなる。
pub fn set_recording_active(active: bool) {
    RECORDING_ACTIVE.store(active, Ordering::SeqCst);
    if !active {
        CANCEL_IS_DOWN.store(false, Ordering::SeqCst);
    }
}

/// キー捕獲モードを開始する。戻り値はこの捕獲セッションの世代。
///
/// タイムアウト側はこの世代を持ち回り、**自分が始めた捕獲だけ**を打ち切る。
/// そうしないと、素早くやり直したときに新しい捕獲を古いタイマーが殺す。
pub fn begin_capture() -> u64 {
    // 捕獲へ入る時点の押下状態は持ち越さない。
    KEY_IS_DOWN.store(false, Ordering::SeqCst);
    CAPTURE_MODE.store(true, Ordering::SeqCst);
    let generation = CAPTURE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    log::info!("キー捕獲モード: 開始 (世代 {generation})");
    generation
}

/// 捕獲モードを終える。`generation` を渡すと、その世代のときだけ終了する。
pub fn end_capture(generation: Option<u64>) -> bool {
    if let Some(generation) = generation {
        if CAPTURE_GENERATION.load(Ordering::SeqCst) != generation {
            return false; // 既に別の捕獲が始まっている。
        }
    }
    let was_capturing = CAPTURE_MODE.swap(false, Ordering::SeqCst);
    if was_capturing {
        log::info!("キー捕獲モード: 終了");
    }
    was_capturing
}

pub fn is_capturing() -> bool {
    CAPTURE_MODE.load(Ordering::SeqCst)
}

/// 指定キーを「離されるまで通常経路で無視する」状態にする。
pub fn suppress_until_release(vk: u32) {
    SUPPRESS_UNTIL_RELEASE.store(vk, Ordering::SeqCst);
    KEY_IS_DOWN.store(false, Ordering::SeqCst);
}

/// 仮想キーコードを人間が読める名前にする。
///
/// 設定 UI に「右 Ctrl」と出すためのもの。網羅ではなく、
/// ホットキーに選ばれそうなキーを優先して並べてある。
pub fn key_label(vk: u32) -> String {
    let name = match vk {
        0xA2 => "左 Ctrl",
        0xA3 => "右 Ctrl",
        0xA0 => "左 Shift",
        0xA1 => "右 Shift",
        0xA4 => "左 Alt",
        0xA5 => "右 Alt",
        0x5B => "左 Win",
        0x5C => "右 Win",
        0x14 => "CapsLock",
        0x09 => "Tab",
        0x1B => "Esc",
        0x20 => "Space",
        0x0D => "Enter",
        0x08 => "BackSpace",
        0x2D => "Insert",
        0x2E => "Delete",
        0x24 => "Home",
        0x23 => "End",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x91 => "ScrollLock",
        0x13 => "Pause",
        0x1D => "無変換",
        0x1C => "変換",
        0xF3 | 0xF4 => "半角/全角",
        0x70..=0x7B => return format!("F{}", vk - 0x6F),
        0x30..=0x39 => return format!("{}", vk - 0x30),
        0x41..=0x5A => return char::from(vk as u8).to_string(),
        0x60..=0x69 => return format!("テンキー {}", vk - 0x60),
        _ => return format!("VK 0x{vk:02X}"),
    };
    name.to_string()
}

/// フックが観測した生のキーイベントの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEventKind {
    /// ホットキーが押された (オートリピートは除去済み)。
    Press,
    /// ホットキーが離された。
    Release,
    /// 捕獲モード中に押されたキー。PTT の解釈は行わない。
    Captured(u32),
    /// 録音中にキャンセルキーが押された。PTT の解釈は行わない
    /// (コントローラが録音を破棄する)。
    Cancel,
}

/// フックが観測したキーイベント。**発生時刻を必ず伴う**。
///
/// # なぜ時刻を載せるのか
///
/// 長押し / 短押しの判定を「コントローラがデキューした時刻」で行うと、
/// コントローラが直前のイベント処理 (録音開始やその後の STT 呼び出し) で
/// 詰まっている間に滞留したイベントの保持時間が水増しされ、
/// 短押し (トグル) が長押しと誤分類される。
/// 判定は必ずフックが観測した時刻 [`HotkeyEvent::at`] で行うこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyEvent {
    pub kind: HotkeyEventKind,
    /// フックコールバックが観測した時刻。
    ///
    /// `KBDLLHOOKSTRUCT.time` (`GetTickCount` ベース、49.7 日でラップし
    /// 分解能も 10〜16 ms 程度) ではなく `Instant` を使う。
    /// 単調・高分解能・ラップなしで、取得コストも QPC 相当で十分に軽い。
    pub at: Instant,
}

impl HotkeyEvent {
    fn new(kind: HotkeyEventKind) -> Self {
        Self {
            kind,
            at: Instant::now(),
        }
    }
}

/// [`PttInterpreter`] が出す録音制御の指示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    StartRecording,
    StopRecording,
}

// --- フックコールバックが触れるグローバル状態 -------------------------------
//
// フックプロシージャは extern "system" fn でユーザーデータを渡せないため、
// 送信端はプロセスグローバルに置く。書き込みはフック設置時の一度きり。

static EVENT_TX: OnceLock<Sender<HotkeyEvent>> = OnceLock::new();
/// オートリピート除去用。フックスレッドからのみ更新される。
static KEY_IS_DOWN: AtomicBool = AtomicBool::new(false);
/// フックスレッドの ID。停止要求 (`WM_QUIT`) の宛先。
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// チャネル満杯で捨てたイベント数。コントローラ側が観測してログに出す。
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

/// ホットキーイベントチャネルの容量。
///
/// 有界にするのは、`unbounded` の送信がブロック満杯時に**ヒープ確保を行う**ため。
/// フックコールバックで確保するのはモジュール冒頭の不変条件 (確保・ロック・IO 禁止)
/// に反する。人間のキー操作で 64 件が詰まることは実質ありえず、
/// 詰まるとすれば受信側の恒久的な停止なので、その場合は捨ててよい。
const EVENT_CHANNEL_CAPACITY: usize = 64;

/// 取りこぼしたイベントの累計。
pub fn dropped_events() -> u64 {
    DROPPED_EVENTS.load(Ordering::Relaxed)
}

/// 低レベルキーボードフックのコールバック。
///
/// # 制約
/// - ここでの処理は数マイクロ秒に収める。確保・ロック・IO は禁止。
/// - panic は絶対に外へ出さない (FFI 境界を越える panic は UB)。
///   本体は panic しうる操作を含まないが、多重防御として `catch_unwind` で包む。
/// - キーは抑制しない (`CallNextHookEx` へ必ず流す)。右 Ctrl を握り潰すと
///   右 Ctrl + C などの通常操作が壊れるため。
unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // AssertUnwindSafe: 触れるのは atomic と channel の共有参照のみで、
        // panic で壊れて困る不変条件を持たない。
        let _ = catch_unwind(AssertUnwindSafe(|| {
            handle_key_event(wparam.0 as u32, lparam);
        }));
    }
    // SAFETY: 呼び出し元から渡された引数をそのまま次のフックへ渡すだけ。
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// フックコールバックの本体。確保も待機も行わない。
fn handle_key_event(message: u32, lparam: LPARAM) {
    if lparam.0 == 0 {
        return;
    }
    // SAFETY: HC_ACTION の WH_KEYBOARD_LL では lParam は OS 所有の
    // KBDLLHOOKSTRUCT を指す。コールバックの間だけ有効で、読み取りのみ行う。
    let info: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };

    // 合成入力 (自分が SendInput する Ctrl+V 等) は無視する。
    if info.flags.contains(LLKHF_INJECTED) {
        return;
    }

    // 捕獲モード中は、どのキーでも「押された」ことだけを報告する。
    // ここで PTT の判定に混ぜると、設定中に録音が始まってしまう。
    if CAPTURE_MODE.load(Ordering::SeqCst) {
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) {
            send(HotkeyEvent::new(HotkeyEventKind::Captured(info.vkCode)));
        }
        return;
    }

    // 捕獲直後の押しっぱなしを締め出す。離した時点で解除する。
    let suppressed = SUPPRESS_UNTIL_RELEASE.load(Ordering::SeqCst);
    if suppressed != 0 && info.vkCode == suppressed {
        if matches!(message, WM_KEYUP | WM_SYSKEYUP) {
            SUPPRESS_UNTIL_RELEASE.store(0, Ordering::SeqCst);
        }
        return;
    }

    // 録音中のキャンセルキー。押下で Cancel を 1 回だけ報告し、離しで状態を戻す。
    // ホットキー経路の前に判定する (録音中は PTT の解釈より破棄が優先)。
    let cancel_vk = CANCEL_VK.load(Ordering::SeqCst);
    if cancel_vk != 0
        && info.vkCode == cancel_vk
        && !CAPTURE_MODE.load(Ordering::SeqCst)
        && RECORDING_ACTIVE.load(Ordering::SeqCst)
    {
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN) {
            // オートリピートの連打を 1 回の押下に畳む。
            if !CANCEL_IS_DOWN.swap(true, Ordering::SeqCst) {
                send(HotkeyEvent::new(HotkeyEventKind::Cancel));
            }
        } else if matches!(message, WM_KEYUP | WM_SYSKEYUP) {
            CANCEL_IS_DOWN.store(false, Ordering::SeqCst);
        }
        // キー自体は抑制しない。Esc は挿入先アプリでも「取り消し」として
        // 働くべきなので、そのまま流す (CallNextHookEx は呼び出し側で必ず通る)。
        return;
    }

    if info.vkCode != HOTKEY_VK.load(Ordering::SeqCst) {
        return;
    }

    let kind = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            // オートリピートの連打を 1 回の押下に畳む。
            if KEY_IS_DOWN.swap(true, Ordering::SeqCst) {
                return;
            }
            HotkeyEventKind::Press
        }
        WM_KEYUP | WM_SYSKEYUP => {
            if !KEY_IS_DOWN.swap(false, Ordering::SeqCst) {
                return;
            }
            HotkeyEventKind::Release
        }
        _ => return,
    };

    // 時刻はここで採る。デキュー時刻で判定すると、コントローラの詰まりが
    // そのまま「長押し」に化ける (HotkeyEvent の doc 参照)。
    send(HotkeyEvent::new(kind));
}

/// フックからイベントを送る。確保もブロックもしない。
fn send(event: HotkeyEvent) {
    if let Some(tx) = EVENT_TX.get() {
        // bounded の try_send は確保もブロックもしない。満杯なら捨てて数える。
        if tx.try_send(event).is_err() {
            DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// フックスレッドを起動し、イベント受信端を返す。
///
/// チャネルはここで作る (容量を [`EVENT_CHANNEL_CAPACITY`] に固定するため)。
/// 戻り値の [`HookHandle`] を drop するとフックスレッドへ停止を要求する。
/// 失敗した場合はホットキーなしで (エラー通知の上) アプリを継続できる。
pub fn spawn() -> Result<(HookHandle, Receiver<HotkeyEvent>), String> {
    let (tx, rx) = crossbeam_channel::bounded::<HotkeyEvent>(EVENT_CHANNEL_CAPACITY);
    if EVENT_TX.set(tx).is_err() {
        return Err("キーボードフックは既に設置済み".to_string());
    }

    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);

    let join = thread::Builder::new()
        .name("nox-hotkey".to_string())
        .spawn(move || hook_thread_main(ready_tx))
        .map_err(|e| format!("フックスレッドの起動に失敗: {e}"))?;

    // フック設置の成否が判るまで待つ (設置は数 ms で終わる)。
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok((HookHandle { join: Some(join) }, rx)),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("フック設置の応答待ちに失敗: {e}")),
    }
}

/// フックスレッド本体。フックの設置とメッセージループを持つ。
///
/// `WH_KEYBOARD_LL` はフックを設置したスレッドにメッセージループがないと
/// コールバックが呼ばれないため、専用スレッドでループを回す。
fn hook_thread_main(ready_tx: Sender<Result<(), String>>) {
    // SAFETY: hmod は LL フックでは不要 (None)。dwThreadId=0 でグローバルフック。
    let hook = match unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
    } {
        Ok(h) => h,
        Err(e) => {
            let _ = ready_tx.try_send(Err(format!("SetWindowsHookExW に失敗: {e}")));
            return;
        }
    };

    // SAFETY: 引数なし。
    HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let _ = ready_tx.try_send(Ok(()));
    log::info!(
        "キーボードフックを設置 (ホットキー: {})",
        key_label(HOTKEY_VK.load(Ordering::SeqCst))
    );

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg はスタック上の有効な MSG。
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            // 0 = WM_QUIT, -1 = エラー。どちらもループを抜ける。
            break;
        }
        // SAFETY: msg は GetMessageW が埋めた有効な値。
        unsafe { DispatchMessageW(&msg) };
    }

    // SAFETY: hook は SetWindowsHookExW 由来。以降は使わない。
    if let Err(e) = unsafe { UnhookWindowsHookEx(hook) } {
        log::warn!("UnhookWindowsHookEx に失敗: {e}");
    }
    HOOK_THREAD_ID.store(0, Ordering::SeqCst);
    log::info!("キーボードフックを解除");
}

/// フックの生存を表すハンドル。drop でフックスレッドを停止させる。
pub struct HookHandle {
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
        if tid != 0 {
            // SAFETY: 引数は単なる数値。宛先スレッドが既に無ければ Err が返るだけ。
            if let Err(e) =
                unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) }
            {
                log::warn!("フックスレッドへの WM_QUIT 送信に失敗: {e}");
            }
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

// --- PTT / トグルの解釈 -----------------------------------------------------

/// キーの押下パターンを録音制御へ翻訳する純粋な状態機械。
///
/// 仕様:
/// - **長押し (PTT)**: 押下で録音開始 → [`TAP_THRESHOLD`] 以上保持して離すと停止。
/// - **短押し (トグル)**: 押下で録音開始 → しきい値未満で離すと録音継続 (ラッチ)。
///   次にもう一度押した時点で停止する (その押下に対応する離しは無視)。
///
/// Win32 に依存しないので単体テストできる。
#[derive(Debug)]
pub struct PttInterpreter {
    tap_threshold: Duration,
    pressed_at: Option<Instant>,
    recording: bool,
    /// トグルモードで録音が継続している。
    latched: bool,
    /// 次の Release を捨てる (トグル停止した押下に対応する離し)。
    ignore_next_release: bool,
}

impl PttInterpreter {
    pub fn new(tap_threshold: Duration) -> Self {
        Self {
            tap_threshold,
            pressed_at: None,
            recording: false,
            latched: false,
            ignore_next_release: false,
        }
    }

    /// イベントを 1 件処理する。
    ///
    /// 判定は必ず [`HotkeyEvent::at`] (フックが観測した時刻) で行う。
    /// ここで `Instant::now()` を使うと、コントローラが重い処理で詰まった分だけ
    /// 保持時間が伸び、短押しが長押しに化ける。
    pub fn on_event(&mut self, event: HotkeyEvent) -> Option<HotkeyAction> {
        match event.kind {
            HotkeyEventKind::Press => self.on_press(event.at),
            HotkeyEventKind::Release => self.on_release(event.at),
            // 捕獲は設定操作であって録音操作ではない。
            HotkeyEventKind::Captured(_) => None,
            // キャンセルも解釈器を通らない。コントローラが直接処理し、
            // この解釈器の押下状態は reset() で捨てられる。
            HotkeyEventKind::Cancel => None,
        }
    }

    /// 録音が外部要因 (デバイスエラー等) で止まった場合に状態を戻す。
    pub fn reset(&mut self) {
        self.pressed_at = None;
        self.recording = false;
        self.latched = false;
        // 押しっぱなしの最中に失敗した場合、その離しは捨てる。
        self.ignore_next_release = true;
    }

    pub fn on_press(&mut self, now: Instant) -> Option<HotkeyAction> {
        if self.recording {
            if self.latched {
                // トグル録音中の再押下 = 停止。
                self.recording = false;
                self.latched = false;
                self.pressed_at = None;
                self.ignore_next_release = true;
                return Some(HotkeyAction::StopRecording);
            }
            // PTT 保持中に押下が来ることは無いはず (リピートは除去済み)。
            return None;
        }
        self.recording = true;
        self.latched = false;
        self.pressed_at = Some(now);
        self.ignore_next_release = false;
        Some(HotkeyAction::StartRecording)
    }

    pub fn on_release(&mut self, now: Instant) -> Option<HotkeyAction> {
        if self.ignore_next_release {
            self.ignore_next_release = false;
            return None;
        }
        if !self.recording || self.latched {
            return None;
        }
        let held = self
            .pressed_at
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(self.tap_threshold);

        if held < self.tap_threshold {
            // 短押し → トグルモードへ移行 (録音は続ける)。
            self.latched = true;
            self.pressed_at = None;
            None
        } else {
            self.recording = false;
            self.pressed_at = None;
            Some(HotkeyAction::StopRecording)
        }
    }
}

/// キー捕獲の状態機械 (UI 側の「キーを押して設定」用)。
///
/// Win32 に触れないのでテストできる。捕獲したキーをそのまま採用せず、
/// ここで**採用してよいか**を決める:
///
/// - Esc は「取り消し」。ホットキーには選べない (設定をやり直せなくなる)
/// - 同じキーを選び直した場合も成功として扱う (UI が固まらない)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// このキーを採用する。
    Accept(u32),
    /// 取り消し。
    Cancel,
    /// ホットキーには使えないキー (押しっぱなしで実害が出る)。
    Rejected(String),
}

/// ホットキーに選んでよいキーか。
///
/// # 許可リストにする理由
///
/// フックは**キーを抑制しない** (右 Ctrl を握り潰すと右 Ctrl+C 等が壊れるため)。
/// つまり PTT 中、選んだキーは挿入先アプリにも流れ続ける。
/// ここで `Enter` を選べてしまうと、長押しのあいだチャットに空行が
/// 連投される。`A` を選べば文字が入り続ける。
///
/// そこで「押しっぱなしでも実害が出ない」キーだけを許す:
/// 修飾キー・ファンクションキー・ロック系・IME 系。
pub fn is_allowed_hotkey(vk: u32) -> bool {
    matches!(vk,
        // 修飾キー (左右個別)
        0xA0..=0xA5
        // Win キー、アプリケーションキー
        | 0x5B | 0x5C | 0x5D
        // CapsLock / ScrollLock / Pause
        | 0x14 | 0x91 | 0x13
        // F1..F24
        | 0x70..=0x87
        // IME 系: 変換 / 無変換
        //
        // 半角/全角 (0xF3 / 0xF4) は**押すたびに別の VK が来る**ため除外する。
        // 片方だけを登録すると 2 回に 1 回しか効かない。
        // かな (0x15) も、実キーが返す 0xF2 と対応が取れないので外す。
        | 0x1C | 0x1D
    )
}

/// キャンセルキーに選んでよいキーか。
///
/// ホットキー (`is_allowed_hotkey`) とは要件が違う。キャンセルは録音中の
/// **1 回押し**であり押しっぱなしにしないため、`Enter` や文字キーでも
/// 実害は 1 回きり。一方で Esc は捕獲モード脱出の専用キーとしてホットキー側では
/// 拒否されるが、キャンセルキーの**既定**なので許さないと始まらない。
///
/// そこで拒否するのは次だけ:
/// - マウスボタン (LL キーボードフックには来ない。設定ファイル経由でのみ入りうる)
/// - 半角/全角 (0xF3 / 0xF4)・かな (0x15) (押すたびに VK が変わるため確実に発火しない)
/// - 0 および 0xFF 超 (0 は「無効化」の意味だが呼び出し側でも弾く。範囲外は手編集の誤り)
pub fn is_allowed_cancel_vk(vk: u32) -> bool {
    (1..=0xFF).contains(&vk) && !matches!(vk, 0x01..=0x06 | 0xF3 | 0xF4 | 0x15)
}

/// 捕獲を始めてよいか。
///
/// **録音中は始めない。** PTT を押している最中に捕獲へ入ると、離した瞬間の
/// Release が捕獲側へ吸われ、`PttInterpreter` は押されたままだと思い込む。
/// その結果、録音が止まらなくなる。
pub fn can_begin_capture(is_recording: bool) -> Result<(), &'static str> {
    if is_recording {
        return Err("録音中はホットキーを変更できません。録音を止めてからやり直してください");
    }
    Ok(())
}

/// 捕獲したキーをどう扱うか決める。
pub fn decide_capture(vk: u32) -> CaptureOutcome {
    const VK_ESCAPE: u32 = 0x1B;
    if vk == VK_ESCAPE {
        return CaptureOutcome::Cancel;
    }
    if !is_allowed_hotkey(vk) {
        return CaptureOutcome::Rejected(key_label(vk));
    }
    CaptureOutcome::Accept(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp() -> PttInterpreter {
        PttInterpreter::new(Duration::from_millis(300))
    }

    #[test]
    fn long_press_is_push_to_talk() {
        let mut it = interp();
        let t0 = Instant::now();
        assert_eq!(it.on_press(t0), Some(HotkeyAction::StartRecording));
        let t1 = t0 + Duration::from_millis(1500);
        assert_eq!(it.on_release(t1), Some(HotkeyAction::StopRecording));
        // もう一度押せばまた開始できる。
        assert_eq!(
            it.on_press(t1 + Duration::from_millis(10)),
            Some(HotkeyAction::StartRecording)
        );
    }

    /// 長押し PTT を続けて 2 回行っても 2 回目の離しが飲み込まれないこと。
    /// (停止のたびに `reset()` を呼ぶと 2 回目が止まらなくなる回帰の防止)
    #[test]
    fn consecutive_long_presses_each_stop() {
        let mut it = interp();
        let mut t = Instant::now();
        for round in 0..3 {
            assert_eq!(
                it.on_press(t),
                Some(HotkeyAction::StartRecording),
                "round {round}"
            );
            t += Duration::from_millis(900);
            assert_eq!(
                it.on_release(t),
                Some(HotkeyAction::StopRecording),
                "round {round}"
            );
            t += Duration::from_millis(400);
        }
    }

    #[test]
    fn short_press_latches_into_toggle_mode() {
        let mut it = interp();
        let t0 = Instant::now();
        assert_eq!(it.on_press(t0), Some(HotkeyAction::StartRecording));
        // 短押しで離す → 停止しない (ラッチ)。
        assert_eq!(it.on_release(t0 + Duration::from_millis(120)), None);

        // 2 回目の押下で停止。
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(it.on_press(t1), Some(HotkeyAction::StopRecording));
        // 対応する離しは無視される。
        assert_eq!(it.on_release(t1 + Duration::from_millis(80)), None);
        // その後は通常どおり開始できる。
        assert_eq!(
            it.on_press(t1 + Duration::from_secs(1)),
            Some(HotkeyAction::StartRecording)
        );
    }

    #[test]
    fn toggle_stop_by_long_press_also_works() {
        let mut it = interp();
        let t0 = Instant::now();
        it.on_press(t0);
        it.on_release(t0 + Duration::from_millis(50)); // ラッチ
        let t1 = t0 + Duration::from_secs(3);
        assert_eq!(it.on_press(t1), Some(HotkeyAction::StopRecording));
        // 長く保持してから離しても二重停止しない。
        assert_eq!(it.on_release(t1 + Duration::from_secs(2)), None);
    }

    /// M-1 回帰: 判定はイベントの発生時刻で行われ、処理の遅延に影響されない。
    ///
    /// コントローラが `audio::start()` (BT マイクで数百 ms) や
    /// STT 呼び出しで詰まっても、滞留した短押しが長押しに化けてはならない。
    #[test]
    fn classification_uses_event_time_not_dequeue_time() {
        let t0 = Instant::now();
        // 100ms の短押し = トグルラッチになるはずのイベント列。
        let events = [
            HotkeyEvent {
                kind: HotkeyEventKind::Press,
                at: t0,
            },
            HotkeyEvent {
                kind: HotkeyEventKind::Release,
                at: t0 + Duration::from_millis(100),
            },
        ];

        let mut it = interp();
        assert_eq!(
            it.on_event(events[0]),
            Some(HotkeyAction::StartRecording)
        );

        // ここで処理側が 2 秒詰まったことを模す。デキュー時刻で判定していれば
        // 保持時間が 2 秒扱いになり StopRecording が返ってしまう。
        let dequeued_at = t0 + Duration::from_millis(2_100);
        assert!(dequeued_at.saturating_duration_since(events[1].at) > TAP_THRESHOLD);

        assert_eq!(
            it.on_event(events[1]),
            None,
            "遅延デキューで短押しが長押しに誤分類された"
        );

        // ラッチできているので、次の押下で停止する。
        assert_eq!(
            it.on_event(HotkeyEvent {
                kind: HotkeyEventKind::Press,
                at: t0 + Duration::from_secs(4),
            }),
            Some(HotkeyAction::StopRecording)
        );
    }

    /// 逆向きの確認: 実際の長押しは遅延の有無にかかわらず停止になる。
    #[test]
    fn real_long_press_still_stops_when_dequeued_late() {
        let t0 = Instant::now();
        let mut it = interp();
        it.on_event(HotkeyEvent {
            kind: HotkeyEventKind::Press,
            at: t0,
        });
        assert_eq!(
            it.on_event(HotkeyEvent {
                kind: HotkeyEventKind::Release,
                at: t0 + Duration::from_millis(800),
            }),
            Some(HotkeyAction::StopRecording)
        );
    }

    // --- ホットキーの差し替えと捕獲 ---

    #[test]
    fn the_default_hotkey_is_right_ctrl() {
        assert_eq!(DEFAULT_HOTKEY_VK, 0xA3);
        assert_eq!(key_label(DEFAULT_HOTKEY_VK), "右 Ctrl");
    }

    #[test]
    fn key_labels_cover_the_likely_choices() {
        assert_eq!(key_label(0xA2), "左 Ctrl");
        assert_eq!(key_label(0x14), "CapsLock");
        assert_eq!(key_label(0x70), "F1");
        assert_eq!(key_label(0x7B), "F12");
        assert_eq!(key_label(0x41), "A");
        assert_eq!(key_label(0x30), "0");
        assert_eq!(key_label(0x60), "テンキー 0");
        assert_eq!(key_label(0x1D), "無変換");
        // 知らないキーでも読める形にする (空文字にしない)。
        assert_eq!(key_label(0xFE), "VK 0xFE");
    }

    #[test]
    fn escape_cancels_the_capture() {
        // Esc を採用できると、設定をやり直す手段が無くなる。
        assert_eq!(decide_capture(0x1B), CaptureOutcome::Cancel);
    }

    #[test]
    fn cancel_key_rules_differ_from_hotkey_rules() {
        // 回帰: キャンセルキーの検証に is_allowed_hotkey を流用すると、
        // 既定の Esc 自身が「不正」と判定され毎回警告が出る (実機で発覚)。
        // キャンセルは 1 回押しなので Esc も文字キーも許す。要件が違うことをここで固定する。
        assert!(is_allowed_cancel_vk(DEFAULT_CANCEL_VK));
        assert!(is_allowed_cancel_vk(0x41)); // A
        assert!(is_allowed_cancel_vk(0x0D)); // Enter
        assert!(!is_allowed_hotkey(DEFAULT_CANCEL_VK)); // ホットキー側は従来どおり拒否
        // 拒否側: マウス・VK が揺れる IME 系・範囲外・0。
        assert!(!is_allowed_cancel_vk(0x01));
        assert!(!is_allowed_cancel_vk(0xF3));
        assert!(!is_allowed_cancel_vk(0x15));
        assert!(!is_allowed_cancel_vk(0x100));
        assert!(!is_allowed_cancel_vk(0));
    }

    #[test]
    fn modifier_and_function_keys_are_accepted() {
        // 押しっぱなしでも挿入先に実害が出ないキー。
        for vk in [0xA3, 0xA5, 0xA0, 0x5B, 0x5D, 0x14, 0x91, 0x13, 0x70, 0x87, 0x1C, 0x1D] {
            assert_eq!(decide_capture(vk), CaptureOutcome::Accept(vk), "VK 0x{vk:02X}");
        }
    }

    #[test]
    fn printable_keys_are_rejected() {
        // フックはキーを抑制しないので、PTT 中ずっと挿入先へ流れる。
        // Enter なら送信連発、A なら文字が入り続ける。
        for vk in [0x41, 0x5A, 0x30, 0x39, 0x0D, 0x20, 0x09, 0x08, 0xBC, 0x60] {
            assert!(
                matches!(decide_capture(vk), CaptureOutcome::Rejected(_)),
                "VK 0x{vk:02X} を許してしまった"
            );
        }
    }

    #[test]
    fn a_rejection_names_the_key_for_the_user() {
        match decide_capture(0x0D) {
            CaptureOutcome::Rejected(label) => assert_eq!(label, "Enter"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn toggling_ime_keys_are_rejected() {
        // 半角/全角は押すたびに 0xF3 と 0xF4 が交互に来る。片方を登録すると
        // 2 回に 1 回しか効かないホットキーになる。
        assert!(!is_allowed_hotkey(0xF3));
        assert!(!is_allowed_hotkey(0xF4));
        // かな (0x15) は実キーの 0xF2 と対応が取れない。
        assert!(!is_allowed_hotkey(0x15));
        // 変換 / 無変換は 1 キー 1 VK なので使える。
        assert!(is_allowed_hotkey(0x1C));
        assert!(is_allowed_hotkey(0x1D));
    }

    #[test]
    fn mouse_buttons_are_rejected() {
        // マウスは LL キーボードフックに来ないが、設定ファイル経由で
        // 入りうる値なので拒否側に倒す。
        for vk in [0x01, 0x02, 0x04, 0x05, 0x06] {
            assert!(!is_allowed_hotkey(vk), "VK 0x{vk:02X}");
        }
    }

    #[test]
    fn a_confirmed_capture_ignores_the_key_until_it_is_released() {
        // M1 回帰: 捕獲確定時、そのキーはまだ押されたまま。
        // オートリピートが通常経路へ流れると設定しただけで録音が始まる。
        suppress_until_release(0x70);
        assert_eq!(SUPPRESS_UNTIL_RELEASE.load(Ordering::SeqCst), 0x70);
        assert!(!KEY_IS_DOWN.load(Ordering::SeqCst), "押下状態が残っている");

        // 後片付け (プロセス共有の状態)。
        SUPPRESS_UNTIL_RELEASE.store(0, Ordering::SeqCst);
    }

    #[test]
    fn capture_generations_prevent_a_stale_timeout_from_cancelling() {
        // M2 の timeout が、やり直した新しい捕獲を殺さないこと。
        let first = begin_capture();
        let second = begin_capture();
        assert_ne!(first, second);

        // 古い世代での終了要求は無視される。
        assert!(!end_capture(Some(first)));
        assert!(is_capturing(), "古いタイマーが新しい捕獲を殺した");

        // 現世代なら終了する。
        assert!(end_capture(Some(second)));
        assert!(!is_capturing());
    }

    #[test]
    fn capture_is_refused_while_recording() {
        // m6 回帰: PTT 保持中に捕獲へ入ると Release が吸われ、
        // 解釈器が「押されたまま」と思い込んで録音が止まらなくなる。
        assert!(can_begin_capture(false).is_ok());
        let refused = can_begin_capture(true).expect_err("録音中は拒否する");
        assert!(refused.contains("録音中"), "理由が伝わらない: {refused}");
    }

    #[test]
    fn ending_a_capture_twice_is_harmless() {
        let generation = begin_capture();
        assert!(end_capture(Some(generation)));
        assert!(!end_capture(Some(generation)), "二重終了で true を返した");
        assert!(!end_capture(None));
    }

    #[test]
    fn a_captured_event_is_not_a_recording_action() {
        // 設定操作で録音が始まってはいけない。
        let mut it = interp();
        let event = HotkeyEvent {
            kind: HotkeyEventKind::Captured(0x70),
            at: Instant::now(),
        };
        assert_eq!(it.on_event(event), None);
    }

    #[test]
    fn changing_the_hotkey_clears_the_held_state() {
        // 押しっぱなしのまま差し替えると、次の離しだけが届いて状態がねじれる。
        KEY_IS_DOWN.store(true, Ordering::SeqCst);
        set_hotkey_vk(0x70);
        assert!(!KEY_IS_DOWN.load(Ordering::SeqCst));
        assert_eq!(HOTKEY_VK.load(Ordering::SeqCst), 0x70);
        // 後片付け (プロセス共有の状態なので戻す)。
        set_hotkey_vk(DEFAULT_HOTKEY_VK);
    }

    #[test]
    fn stray_release_is_ignored() {
        let mut it = interp();
        // 起動直後にキーを離しただけ (押下を観測していない)。
        assert_eq!(it.on_release(Instant::now()), None);
    }

    #[test]
    fn reset_after_failure_discards_pending_release() {
        let mut it = interp();
        let t0 = Instant::now();
        assert_eq!(it.on_press(t0), Some(HotkeyAction::StartRecording));
        // 録音開始に失敗したとみなしてリセット。
        it.reset();
        assert_eq!(it.on_release(t0 + Duration::from_secs(1)), None);
        // 次の押下は素直に開始になる。
        assert_eq!(
            it.on_press(t0 + Duration::from_secs(2)),
            Some(HotkeyAction::StartRecording)
        );
    }

    // --- 録音キャンセルキー -----------------------------------------------------
    //
    // フックの共有状態 (static) を直接書き換えるため、テスト同士で
    // 状態とイベントチャネルの奪い合いが起きないよう直列化する。

    /// キャンセル系テストを直列化するロック。
    fn hook_state_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// フック本体 (`send`) が届ける先。プロセスで 1 度だけ作る。
    ///
    /// 本来は [`spawn`] が設置するが、テストでは実フックを立てないため
    /// ここで同じ型のチャネルを差し込む。
    fn hook_events() -> Receiver<HotkeyEvent> {
        static RX: OnceLock<Receiver<HotkeyEvent>> = OnceLock::new();
        RX.get_or_init(|| {
            let (tx, rx) = crossbeam_channel::bounded(EVENT_CHANNEL_CAPACITY);
            let _ = EVENT_TX.set(tx);
            rx
        })
        .clone()
    }

    /// テスト用にキーイベントをフックへ流し込む。
    ///
    /// `KBDLLHOOKSTRUCT` を積んで [`handle_key_event`] を直接呼ぶ。
    /// 構造体は呼び出し内でコピーされるので、返ったあとの解放は安全。
    fn feed_key(vk_code: i32, message: u32) {
        let mut info = KBDLLHOOKSTRUCT::default();
        info.vkCode = vk_code as u32;
        let ptr = Box::into_raw(Box::new(info));
        handle_key_event(message, LPARAM(ptr as isize));
        // SAFETY: into_raw で渡した所有権を取り戻して解放するだけ。
        drop(unsafe { Box::from_raw(ptr) });
    }

    /// 溜まっているイベントを読み捨てる。
    fn drain(rx: &Receiver<HotkeyEvent>) {
        while rx.try_recv().is_ok() {}
    }

    /// テスト後の後片付け (プロセス共有の状態を既定へ戻す)。
    fn disarm_cancel_state() {
        set_recording_active(false);
        CANCEL_IS_DOWN.store(false, Ordering::SeqCst);
        set_cancel_vk(DEFAULT_CANCEL_VK);
    }

    #[test]
    fn the_default_cancel_key_is_escape() {
        assert_eq!(DEFAULT_CANCEL_VK, 0x1B);
        assert_eq!(key_label(DEFAULT_CANCEL_VK), "Esc");
    }

    #[test]
    fn the_cancel_key_fires_only_while_recording() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_cancel_vk(DEFAULT_CANCEL_VK);
        drain(&rx);

        // 非武装では何も報告しない。
        set_recording_active(false);
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        assert!(rx.try_recv().is_err(), "非武装なのに Cancel が発火した");

        // 武装すると keydown 1 回で Cancel。
        set_recording_active(true);
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        let event = rx.try_recv().expect("武装しているのに Cancel が届かない");
        assert_eq!(event.kind, HotkeyEventKind::Cancel);

        disarm_cancel_state();
    }

    #[test]
    fn a_disabled_cancel_key_never_fires() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_cancel_vk(0); // 無効化
        set_recording_active(true);
        drain(&rx);

        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYUP);
        assert!(rx.try_recv().is_err(), "無効化したキーが発火した");

        disarm_cancel_state();
    }

    #[test]
    fn cancel_autorepeat_collapses_into_one_event() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_cancel_vk(DEFAULT_CANCEL_VK);
        set_recording_active(true);
        drain(&rx);

        // 押しっぱなしによる keydown の連打は 1 回に畳まれる。
        for _ in 0..5 {
            feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        }
        assert_eq!(rx.try_recv().map(|e| e.kind).ok(), Some(HotkeyEventKind::Cancel));
        assert!(rx.try_recv().is_err(), "オートリピートごとに発火した");

        // 離せば再度押せる (次の録音でキャンセルできる)。
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYUP);
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        assert_eq!(rx.try_recv().map(|e| e.kind).ok(), Some(HotkeyEventKind::Cancel));

        disarm_cancel_state();
    }

    #[test]
    fn releasing_the_cancel_key_clears_the_held_flag() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_cancel_vk(DEFAULT_CANCEL_VK);
        set_recording_active(true);
        drain(&rx);

        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        assert!(CANCEL_IS_DOWN.load(Ordering::SeqCst), "押下状態が立っていない");
        // keyup 自体はイベントにならない (離しは報告する必要がない)。
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYUP);
        assert!(!CANCEL_IS_DOWN.load(Ordering::SeqCst), "離しても状態が残った");

        disarm_cancel_state();
    }

    #[test]
    fn capture_mode_takes_priority_over_the_cancel_key() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_cancel_vk(DEFAULT_CANCEL_VK);
        set_recording_active(true);
        drain(&rx);

        // 捕獲モード中は Esc も「押されたキー」として報告される (従来動作)。
        // ここで Cancel として扱うと、設定中に録音が破棄されてしまう。
        let previous = CAPTURE_MODE.swap(true, Ordering::SeqCst);
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        let event = rx.try_recv().expect("捕獲イベントが届かない");
        assert_eq!(
            event.kind,
            HotkeyEventKind::Captured(DEFAULT_CANCEL_VK),
            "捕獲モード中に Cancel 側へ吸われた"
        );
        assert!(!CANCEL_IS_DOWN.load(Ordering::SeqCst), "捕獲経路で押下状態が汚れた");

        CAPTURE_MODE.store(previous, Ordering::SeqCst);
        disarm_cancel_state();
    }

    #[test]
    fn disarming_clears_a_stuck_cancel_key() {
        let _guard = hook_state_lock();
        // 録音終了の瞬間に Esc が押しっぱなしだった場合を模す。
        RECORDING_ACTIVE.store(true, Ordering::SeqCst);
        CANCEL_IS_DOWN.store(true, Ordering::SeqCst);

        set_recording_active(false);
        assert!(!RECORDING_ACTIVE.load(Ordering::SeqCst));
        assert!(
            !CANCEL_IS_DOWN.load(Ordering::SeqCst),
            "非武装化で押下状態が残り、次の録音の最初の keydown が捨てられる"
        );
    }

    #[test]
    fn changing_the_cancel_key_clears_the_held_state() {
        let _guard = hook_state_lock();
        CANCEL_IS_DOWN.store(true, Ordering::SeqCst);
        set_cancel_vk(0x70);
        assert!(!CANCEL_IS_DOWN.load(Ordering::SeqCst));
        assert_eq!(CANCEL_VK.load(Ordering::SeqCst), 0x70);
        // 後片付け (プロセス共有の状態なので戻す)。
        set_cancel_vk(DEFAULT_CANCEL_VK);
    }
}
