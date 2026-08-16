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
const HOTKEY_VK: u32 = VK_RCONTROL.0 as u32;

/// フックが観測した生のキーイベントの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEventKind {
    /// ホットキーが押された (オートリピートは除去済み)。
    Press,
    /// ホットキーが離された。
    Release,
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

    if info.vkCode != HOTKEY_VK {
        return;
    }
    // 合成入力 (自分自身が将来 SendInput する Ctrl+V 等) は無視する。
    if info.flags.contains(LLKHF_INJECTED) {
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
    let event = HotkeyEvent::new(kind);

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
    log::info!("キーボードフックを設置 (ホットキー: 右 Ctrl)");

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
}
