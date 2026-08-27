//! テキスト注入 — クリップボード + Ctrl+V (SendInput)。
//!
//! design.md の技術メモどおり、業界標準のクリップボード経由方式を採る
//! (SendInput の Unicode 直打ちはサロゲートペア・長文で不安定なため不採用)。
//!
//! # 実装している設計要件
//!
//! - **R7 フォーカス検証**: SendInput の直前に `GetForegroundWindow` を
//!   録音開始時の HWND と照合する。不一致なら貼付を中止し、整形テキストは
//!   クリップボードに残したまま通知する。`SetForegroundWindow` による強制復帰は
//!   フォアグラウンドロックで失敗しうるので主手段にしない。
//! - **R3-a 退避側**: `CF_UNICODETEXT` のみ退避する。他プロセスがクリップボードを
//!   握っていると `OpenClipboard` は失敗するので、短い間隔で数回リトライする。
//! - **R3-b 復元側**: 貼付が消費される前に復元すると旧内容が貼られ、逆に長く待つと
//!   ユーザーの新しいコピーを壊す。復元直前に `GetClipboardSequenceNumber` を
//!   自分が設定した時点の値と比べ、**変わっていたら復元を中止する**。
//! - **R4 データ保全**: 貼付が不達・中止になったときは復元せず、整形テキストを
//!   クリップボードに残す。ユーザーは手で Ctrl+V できる。
//! - **R6 非テキスト破壊の可視化**: 画像 (`CF_DIB`) やファイル (`CF_HDROP`) は
//!   退避できない。それらが主内容だった場合は破棄されることを通知する。
//!
//! # 「injected」の意味 (到達確認の運用則)
//!
//! [`InjectOutcome::Injected`] は **SendInput が成功した = 送出した**という意味で、
//! 「相手アプリに貼られた」ことの保証ではない。UIPI により管理者昇格アプリへは
//! 届かないし、相手がキー入力をどう読むかにも依存する。
//! E2E の確認は受け手側の証跡 (メモ帳の中身など) で行うこと。

use std::time::Duration;

use serde::Serialize;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_V,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// 自プロセスが送出した合成入力の印。`KEYBDINPUT::dwExtraInfo` に載せる。
///
/// 現在の [`crate::hotkey`] は `LLKHF_INJECTED` を一律で無視するので実害は
/// 出ないが、将来「自分の注入だけを無視し、他ツール由来のリマップは通す」
/// (design.md 技術メモ) へ絞り込むときの識別子になる。
/// 値は ASCII の "NOXV"。
pub const SELF_INJECTED_MARKER: usize = 0x4E4F_5856;

/// `OpenClipboard` のリトライ回数。
///
/// クリップボードはプロセスをまたぐ排他資源で、他アプリが開いている間は
/// 失敗する。人間の操作に対しては数十 ms 待てばまず空く。
const CLIPBOARD_OPEN_ATTEMPTS: u32 = 12;
/// リトライ間隔。
const CLIPBOARD_OPEN_RETRY_DELAY: Duration = Duration::from_millis(15);
/// 物理修飾キーが離されるのを待つ時間。
const MODIFIER_SETTLE_DELAY: Duration = Duration::from_millis(250);
/// 復元までの既定待ち時間。
pub const DEFAULT_RESTORE_DELAY_MS: u64 = 300;

/// 貼付を終えたあとクリップボードをどう扱うか。
///
/// `delay` は `Restore` のときにしか意味を持たない。bool 引数を足して
/// 「復元しないのに待ち時間だけ渡される」呼び出しを作れる状態にするより、
/// **関係を型で表す**ほうが後から壊れない (design.md「設定に従う分岐は型にする」)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardPolicy {
    /// 貼付後に元の内容へ戻す。`delay` は貼付が消費されるのを待つ時間 (R3-b)。
    Restore { delay: Duration },
    /// 整形テキストをクリップボードに残す (Typeless 互換)。復元しない。
    ///
    /// R7 のフォーカス照合をすり抜けた失敗 — 同じウィンドウだがキャレットが
    /// 入力欄に無い / 相手が Ctrl+V を無視した / UIPI で弾かれた — では
    /// 「送出は成功したのに何も入っていない」ことが起きる。復元してしまうと
    /// そこで発話が消え、言い直しになる。残しておけば Ctrl+V でやり直せる。
    Keep,
}

// --- クリップボード形式の定数 -----------------------------------------------
//
// `windows` crate では `Win32_System_Ole` feature の下にあるが、
// 数個の定数のために巨大な feature を有効化したくないので直接書く
// (Win32 の標準形式 ID は不変)。

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_SYLK: u32 = 4;
const CF_DIF: u32 = 5;
const CF_TIFF: u32 = 6;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_PALETTE: u32 = 9;
const CF_RIFF: u32 = 11;
const CF_WAVE: u32 = 12;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;
const CF_HDROP: u32 = 15;
const CF_LOCALE: u32 = 16;
const CF_DIBV5: u32 = 17;

/// クリップボード履歴・クラウド同期から除外するための登録形式。
///
/// 発話テキストが Win+V の履歴や他デバイスへ流れるのを防ぐ。
/// いずれも値 0 の DWORD を入れる。
const EXCLUSION_FORMATS: [&str; 3] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
];

/// 注入の結末。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectOutcome {
    /// Ctrl+V を送出した。**貼られた保証ではない** (モジュール doc 参照)。
    Injected,
    /// 設定で注入が無効。
    Disabled,
    /// 「クリップボードのみ」モード。意図的に貼り付けていない。
    ClipboardOnly,
    /// 挿入するテキストが空。
    EmptyText,
    /// R7: 前景ウィンドウが録音開始時と違う。
    AbortedFocusChanged,
    /// R7: 録音開始時の前景ウィンドウが不明で、照合できない。
    AbortedTargetUnknown,
    /// 物理修飾キーが押されたままで、Ctrl+V が別の操作になる恐れがある。
    AbortedModifierStuck,
    /// クリップボードを他プロセスが握っていて開けなかった。
    ClipboardBusy,
    /// クリップボードへの書き込みに失敗した。
    ClipboardFailed,
    /// SendInput が 0 を返した (入力がブロックされた等)。
    SendFailed,
}

/// 処理を終えた時点でクリップボードに何が入っているか。
///
/// 「整形テキストが残っている」と「元に戻した」と「ユーザーが別のものを
/// コピーした」は**まったく違う状態**で、UI の案内 (「Ctrl+V で貼れます」)
/// を出してよいのは最初のケースだけ。単一の bool にすると、
/// ユーザーが新しくコピーした場合に「Ctrl+V で貼れます」と嘘をつく。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardState {
    /// クリップボードには触れていない。
    Untouched,
    /// 整形テキストが入っている。手動 Ctrl+V で貼り付けられる。
    HoldsInjectedText,
    /// 元の内容へ戻した。
    RestoredOriginal,
    /// 処理中にユーザーが別の内容をコピーした。整形テキストはもう無い。
    ReplacedByUser,
    /// 書き込みに失敗し、元の内容も整形テキストも残っていない。
    Lost,
}

impl ClipboardState {
    /// 「Ctrl+V で貼り付けられます」と案内してよいか。
    pub fn holds_injected_text(self) -> bool {
        matches!(self, ClipboardState::HoldsInjectedText)
    }
}

/// 注入の結果。
#[derive(Debug, Clone, Serialize)]
pub struct InjectReport {
    pub outcome: InjectOutcome,
    /// Ctrl+V を送出したか。
    pub injected: bool,
    /// 終了時点のクリップボードの状態。
    pub clipboard_state: ClipboardState,
    /// R6: 退避できず失われた非テキスト形式の名前。
    pub lost_formats: Vec<String>,
    /// ユーザーに見せる説明 (無いこともある)。
    pub message: Option<String>,
}

impl InjectReport {
    fn aborted(
        outcome: InjectOutcome,
        clipboard_state: ClipboardState,
        lost_formats: Vec<String>,
    ) -> Self {
        Self {
            outcome,
            injected: false,
            clipboard_state,
            lost_formats,
            message: outcome_message(outcome, clipboard_state),
        }
    }

    /// 何もせず終わった場合 (無効・空文字)。クリップボードには触れていない。
    fn untouched(outcome: InjectOutcome) -> Self {
        Self {
            outcome,
            injected: false,
            clipboard_state: ClipboardState::Untouched,
            lost_formats: Vec::new(),
            message: outcome_message(outcome, ClipboardState::Untouched),
        }
    }

    /// ユーザーの操作を促す通知 (OS トースト) を出すべきか。
    ///
    /// 「見れば分かる情報」ではなく「**放置すると発話が失われる/
    /// 気づかないうちに何かが壊れた**」ものだけを対象にする。
    /// 常駐運用ではウィンドウが閉じていて WebView のイベントは誰も見ない。
    pub fn needs_user_action(&self) -> bool {
        // 手で貼り付けないと発話が使われないまま終わる。
        //
        // **`injected` の否定が要る**。`ClipboardPolicy::Keep` が既定なので、
        // 貼付に成功した通常の完了も `HoldsInjectedText` で終わる。
        // 状態だけで判定すると「毎回トーストが鳴る」構造になり、
        // 通知が意味を失う (design.md R6 の「毎回鳴る通知は無意味」と同じ)。
        // 手当てが要るのは**送出できずにクリップボードだけが残った**場合。
        // 「クリップボードのみ」は貼らないのが仕様。ここを除かないと
        // 成功のたびにトーストが鳴り、通知が意味を失う。
        (!self.injected
            && self.outcome != InjectOutcome::ClipboardOnly
            && self.clipboard_state.holds_injected_text())
            // 何かが失われたことは必ず知らせる。
            || !self.lost_formats.is_empty()
            || self.clipboard_state == ClipboardState::Lost
            // 貼付そのものができなかった。
            || matches!(
                self.outcome,
                InjectOutcome::ClipboardBusy | InjectOutcome::ClipboardFailed
            )
    }
}

/// 結末に対応するユーザー向けメッセージ。
///
/// 「何が起きたか」だけでなく「どうすれば復旧できるか」を必ず含める。
pub fn outcome_message(outcome: InjectOutcome, clipboard: ClipboardState) -> Option<String> {
    // 復旧手段は「今クリップボードに何があるか」で決まる。中止理由だけで
    // 「Ctrl+V で貼れます」と書くと、ユーザーが別のものをコピーしていた場合に嘘になる。
    let recovery = if clipboard.holds_injected_text() {
        "クリップボードに入っています (Ctrl+V で貼り付け可)"
    } else {
        "結果は画面からコピーできます"
    };

    let reason = match outcome {
        InjectOutcome::Injected => {
            // 送出は成功。クリップボードが壊れたときだけ知らせる。
            return (clipboard == ClipboardState::Lost).then(|| {
                "貼り付けは送出しましたが、元のクリップボードを復元できませんでした".to_string()
            });
        }
        InjectOutcome::Disabled | InjectOutcome::EmptyText => return None,
        // 貼らなかったのは仕様どおり。手当ては要らないので黙る
        // (毎回鳴る通知は意味を失う / R6)。クリップボードが壊れたときだけ知らせる。
        InjectOutcome::ClipboardOnly => {
            return (clipboard != ClipboardState::HoldsInjectedText)
                .then(|| "クリップボードへコピーできませんでした".to_string())
        }
        InjectOutcome::AbortedFocusChanged => "挿入先が変わったため貼り付けを中止しました",
        InjectOutcome::AbortedTargetUnknown => {
            "録音開始時の挿入先を特定できなかったため貼り付けを中止しました"
        }
        InjectOutcome::AbortedModifierStuck => {
            "修飾キー (Ctrl / Shift / Alt / Win) が押されたままのため貼り付けを中止しました"
        }
        InjectOutcome::ClipboardBusy => {
            "他のアプリがクリップボードを使用中で貼り付けできませんでした"
        }
        InjectOutcome::ClipboardFailed => "クリップボードへの書き込みに失敗しました",
        InjectOutcome::SendFailed => "キー入力の送出に失敗しました",
    };
    Some(format!("{reason}。{recovery}"))
}

/// R6: 失われる非テキスト形式の通知文。
pub fn lost_formats_message(lost: &[String]) -> Option<String> {
    if lost.is_empty() {
        return None;
    }
    Some(format!(
        "クリップボードにあった {} は退避できないため失われました",
        lost.join("・")
    ))
}

/// 注入先の識別情報。
///
/// HWND だけでは足りない。ウィンドウが閉じたあとハンドル値は**再利用される**ので、
/// 別アプリの新しいウィンドウがたまたま同じ値になりうる。所有プロセスも照合する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectTarget {
    pub hwnd: isize,
    pub process_id: u32,
}

impl InjectTarget {
    pub fn new(hwnd: isize, process_id: u32) -> Self {
        Self { hwnd, process_id }
    }

    fn is_known(&self) -> bool {
        self.hwnd != 0
    }
}

/// テキストをクリップボードへ入れるだけで終える (貼り付けない)。
///
/// # なぜ [`inject`] と別経路なのか
///
/// [`inject`] の中止経路 (`AbortedTargetUnknown` 等) でも結果的に
/// クリップボードにはテキストが残るが、それは**失敗の副産物**として
/// 扱われる — 通知が飛び、履歴には中止として記録される。
/// 「クリップボードのみ」は失敗ではなく指定された動作なので、
/// 前景の照合も修飾キーの確認も行わず、通知も出さない。
/// フォーカスが無い状態 (デスクトップ / 入力欄に居ない) でも成立するのが
/// この経路の存在理由なので、照合を通してはいけない。
///
/// 貼付後の復元 ([`ClipboardPolicy`]) も行わない。ユーザーはこの後
/// 自分の手で Ctrl+V する — その前に元へ戻したら何も貼れない。
pub fn copy_only(text: &str) -> InjectReport {
    if text.trim().is_empty() {
        return InjectReport::untouched(InjectOutcome::EmptyText);
    }
    match prepare_clipboard(text) {
        Ok(prepared) => {
            log::info!(
                "クリップボードのみ: {} 文字を入れました (貼り付けはしません)",
                text.chars().count()
            );
            InjectReport {
                outcome: InjectOutcome::ClipboardOnly,
                injected: false,
                clipboard_state: ClipboardState::HoldsInjectedText,
                lost_formats: prepared.lost_formats,
                message: None,
            }
        }
        Err(failure) => InjectReport::aborted(
            InjectOutcome::ClipboardOnly,
            failure.clipboard_state,
            failure.lost_formats,
        ),
    }
}

/// テキストを注入する。
///
/// `target` は録音開始時に保存した前景ウィンドウ (hwnd = 0 は不明)。
/// `policy` は貼付後のクリップボードの扱い ([`ClipboardPolicy`])。
pub fn inject(text: &str, target: InjectTarget, policy: ClipboardPolicy) -> InjectReport {
    if text.trim().is_empty() {
        return InjectReport::untouched(InjectOutcome::EmptyText);
    }

    // --- クリップボードを開いて退避 + 設定 ---
    let prepared = match prepare_clipboard(text) {
        Ok(p) => p,
        Err(failure) => {
            return InjectReport::aborted(
                failure.outcome,
                failure.clipboard_state,
                failure.lost_formats,
            )
        }
    };
    let PreparedClipboard {
        backup,
        lost_formats,
        sequence,
    } = prepared;

    // ここから先の中止はすべて「整形テキストがクリップボードに残る」状態になる。
    // これは R4 のとおり意図的で、ユーザーは手で Ctrl+V できる。
    let holding = ClipboardState::HoldsInjectedText;

    // --- 修飾キーの確認 ---
    //
    // 右 Ctrl 長押しの PTT では離した直後なので通常は問題ないが、
    // トグル停止 (短押し) の直後は指がまだ乗っていることがある。
    // 押されたままで Ctrl+V を送ると Ctrl+Shift+V などに化ける。
    if let Some(keys) = modifiers_held_after_settling() {
        log::warn!("修飾キーが押されたままなので貼付を中止します: {}", keys.join("+"));
        return InjectReport::aborted(InjectOutcome::AbortedModifierStuck, holding, lost_formats);
    }

    // --- R7: 送出直前のフォーカス照合 ---
    //
    // ここより前に置くと、照合から送出までの間に前景が変わる隙が広がる。
    if !target.is_known() {
        log::warn!("録音開始時の前景ウィンドウが不明なため貼付を中止します");
        return InjectReport::aborted(InjectOutcome::AbortedTargetUnknown, holding, lost_formats);
    }
    let current = current_foreground();
    if current != target.hwnd {
        log::warn!(
            "前景ウィンドウが変わったため貼付を中止します (録音時 0x{:X} → 現在 0x{current:X})",
            target.hwnd
        );
        return InjectReport::aborted(InjectOutcome::AbortedFocusChanged, holding, lost_formats);
    }
    // HWND 値の再利用対策。元のウィンドウが閉じ、別アプリが同じハンドル値を
    // 得ていた場合、HWND だけの照合は通ってしまう。
    if target.process_id != 0 {
        let current_pid = window_process_id(current);
        if current_pid != target.process_id {
            log::warn!(
                "前景ウィンドウの所有プロセスが変わったため貼付を中止します (録音時 pid={} → 現在 pid={current_pid} / hwnd は同値 0x{current:X})",
                target.process_id
            );
            return InjectReport::aborted(
                InjectOutcome::AbortedFocusChanged,
                holding,
                lost_formats,
            );
        }
    }

    // --- Ctrl+V 送出 ---
    if let Err(e) = send_ctrl_v() {
        log::error!("Ctrl+V の送出に失敗しました: {e}");
        return InjectReport::aborted(InjectOutcome::SendFailed, holding, lost_formats);
    }
    log::info!("Ctrl+V を送出しました ({} 文字)", text.chars().count());

    // --- 貼付後のクリップボード ---
    //
    // R3-b: 復元する場合、判定と書き込みは**クリップボードを握ったまま**行う。
    // 分離すると、「変わっていない」と判定してからガードを取り直す間
    // (最大 180ms) にユーザーがコピーし、それを上書きで壊す (TOCTOU)。
    let clipboard_state = match post_paste_action(policy, backup) {
        PostPasteAction::KeepInjectedText { reason } => {
            log::info!("クリップボードは整形テキストのままにします ({reason})");
            ClipboardState::HoldsInjectedText
        }
        PostPasteAction::RestoreAfter { delay, backup } => {
            std::thread::sleep(delay);
            match restore_if_unchanged(&backup, sequence) {
                RestoreOutcome::Restored => {
                    log::info!("クリップボードを復元しました");
                    ClipboardState::RestoredOriginal
                }
                RestoreOutcome::SkippedChanged => {
                    // ユーザーが新しくコピーした。上書きすればその操作を壊す。
                    // 整形テキストはもう残っていないので、そう報告する。
                    log::info!("復元中止: 貼付後にクリップボードが変更されています");
                    ClipboardState::ReplacedByUser
                }
                RestoreOutcome::Failed(outcome) => {
                    log::warn!("クリップボードを復元できませんでした ({outcome:?})");
                    match outcome {
                        // 開けなかっただけなら中身は整形テキストのまま。
                        InjectOutcome::ClipboardBusy => ClipboardState::HoldsInjectedText,
                        // 空にした後で書けなかった = 何も残っていない。
                        _ => ClipboardState::Lost,
                    }
                }
            }
        }
    };

    InjectReport {
        outcome: InjectOutcome::Injected,
        injected: true,
        clipboard_state,
        lost_formats,
        message: outcome_message(InjectOutcome::Injected, clipboard_state),
    }
}

/// Ctrl+V を送出したあとに取る手。
///
/// Win32 に触れないので純関数として判定でき、
/// 「`Keep` では復元経路へ一切入らない」ことをテストで固定できる。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PostPasteAction {
    /// 何もしない。整形テキストがクリップボードに残る。
    KeepInjectedText { reason: &'static str },
    /// `delay` だけ待ってから、割り込みが無ければ `backup` へ戻す。
    RestoreAfter { delay: Duration, backup: String },
}

/// 方針と退避の有無から、貼付後の手を決める。
fn post_paste_action(policy: ClipboardPolicy, backup: Option<String>) -> PostPasteAction {
    match policy {
        // 設定で「残す」を選んでいる。待ちも復元も丸ごと行わない。
        ClipboardPolicy::Keep => PostPasteAction::KeepInjectedText {
            reason: "設定: 録音結果をクリップボードに残す",
        },
        // 戻す先が無い (元が空・画像だけ) なら待つ意味も無い。
        ClipboardPolicy::Restore { .. } if backup.is_none() => {
            PostPasteAction::KeepInjectedText {
                reason: "復元するテキストが無い",
            }
        }
        ClipboardPolicy::Restore { delay } => PostPasteAction::RestoreAfter {
            delay,
            backup: backup.expect("直前のガードで None を除いてある"),
        },
    }
}

/// 退避 + 設定の結果。
struct PreparedClipboard {
    /// 退避できた元テキスト。
    backup: Option<String>,
    /// R6: 退避できなかった非テキスト形式。
    lost_formats: Vec<String>,
    /// 自分が設定した直後のシーケンス番号 (**CloseClipboard の後**に採る)。
    sequence: u32,
}

/// 退避 + 設定に失敗したときの情報。
///
/// 失敗しても「何が失われたか」は報告しなければならない。
#[derive(Debug)]
struct PrepareFailure {
    outcome: InjectOutcome,
    clipboard_state: ClipboardState,
    lost_formats: Vec<String>,
}

/// クリップボードを開き、退避してから整形テキストを設定する。
fn prepare_clipboard(text: &str) -> Result<PreparedClipboard, PrepareFailure> {
    let guard = ClipboardGuard::open().map_err(|outcome| PrepareFailure {
        outcome,
        // 開けなかったので中身は無傷。
        clipboard_state: ClipboardState::Untouched,
        lost_formats: Vec::new(),
    })?;

    let formats = enumerate_formats();
    let lost_formats = lost_format_names(&formats);
    // SAFETY: クリップボードは開いている (guard が生きている)。
    let backup = unsafe { read_unicode_text() };

    if let Err(e) = write_payload(text) {
        log::error!("クリップボードへの書き込みに失敗: {e}");
        // ここに来た時点で EmptyClipboard は成功しているかもしれない =
        // 元の内容を壊した可能性がある。退避したテキストを書き戻す。
        let clipboard_state = write_failure_state(backup.as_deref(), |original| {
            match set_unicode_text(original) {
                Ok(()) => {
                    log::info!("書き込み失敗後に元のテキストを書き戻しました");
                    true
                }
                Err(e) => {
                    log::error!("元のテキストの書き戻しにも失敗しました: {e}");
                    false
                }
            }
        });
        return Err(PrepareFailure {
            outcome: InjectOutcome::ClipboardFailed,
            clipboard_state,
            // 破壊してしまった以上、失われた形式は必ず報告する。
            lost_formats,
        });
    }

    // シーケンス番号は **CloseClipboard の後**に採る。
    //
    // 一度ガードを握ったまま採る実装にしたが、それでは常に不一致になった
    // (実測 2026-08-17)。変更が公開されるのは閉じた時点で、握っている間に
    // 読むと「自分の書き込みを反映する前の値」を掴む。結果として復元判定が
    // 毎回 SkipClipboardChanged に倒れ、**復元が一切行われなくなる**。
    //
    // 閉じてから採るまでの隙 (数マイクロ秒) にユーザーがコピーすると
    // 「変わった」と誤検知するが、その場合は復元を見送るだけで
    // ユーザーのコピーは壊さない (安全側に倒れる)。
    // 本当の TOCTOU (最大 180ms のガード取得待ち) は復元側で塞いである
    // — [`restore_if_unchanged`] を参照。
    drop(guard);
    let sequence = current_sequence();

    Ok(PreparedClipboard {
        backup,
        lost_formats,
        sequence,
    })
}

/// 整形テキストの書き込みに失敗したあとの状態を決める。
///
/// `EmptyClipboard` は成功していた可能性があるので、この時点で元の内容は
/// 壊れている。退避テキストがあれば書き戻しを試み、その成否で状態が決まる。
/// 判定だけを純関数に切り出してあるのでテストできる。
fn write_failure_state(backup: Option<&str>, rewrite: impl FnOnce(&str) -> bool) -> ClipboardState {
    match backup {
        Some(original) if rewrite(original) => ClipboardState::RestoredOriginal,
        Some(_) => ClipboardState::Lost,
        // 退避できるテキストが無かった (画像など)。復旧手段はない。
        None => ClipboardState::Lost,
    }
}

/// 復元の結末。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreOutcome {
    Restored,
    /// 誰かが割り込んでクリップボードを変えた。
    SkippedChanged,
    Failed(InjectOutcome),
}

/// クリップボードを握った状態でシーケンス番号を再確認し、
/// 変わっていなければ復元する (R3-b の TOCTOU 対策)。
fn restore_if_unchanged(text: &str, recorded: u32) -> RestoreOutcome {
    let _guard = match ClipboardGuard::open() {
        Ok(g) => g,
        Err(outcome) => return RestoreOutcome::Failed(outcome),
    };

    // ガードを取るまでに最大 180ms のリトライが入りうる。その間に
    // ユーザーがコピーしていないかを、**握った今この瞬間**に確かめる。
    if restore_decision(recorded, current_sequence()) == RestoreDecision::SkipClipboardChanged {
        return RestoreOutcome::SkippedChanged;
    }

    // 復元時は履歴除外を付けない。元の内容はユーザー自身がコピーしたもので、
    // 既に履歴に入っている前提だから。
    // SAFETY: クリップボードは開いている。
    if let Err(e) = unsafe { EmptyClipboard() } {
        log::warn!("復元のための EmptyClipboard に失敗: {e}");
        return RestoreOutcome::Failed(InjectOutcome::ClipboardFailed);
    }
    match set_unicode_text(text) {
        Ok(()) => RestoreOutcome::Restored,
        Err(e) => {
            log::error!("復元の書き込みに失敗: {e}");
            RestoreOutcome::Failed(InjectOutcome::ClipboardFailed)
        }
    }
}

/// 退避しておいたテキストへ無条件で戻す。
///
/// 本番経路はシーケンス確認つきの [`restore_if_unchanged`] を使う。
/// こちらはテストの前準備・後片付け専用。
#[cfg(test)]
fn restore_text(text: &str) -> Result<(), InjectOutcome> {
    let _guard = ClipboardGuard::open()?;
    // 復元時は履歴除外を付けない。元の内容はユーザー自身がコピーしたもので、
    // 既に履歴に入っている前提だから。
    // SAFETY: クリップボードは開いている。
    unsafe {
        EmptyClipboard().map_err(|_| InjectOutcome::ClipboardFailed)?;
    }
    set_unicode_text(text).map_err(|_| InjectOutcome::ClipboardFailed)
}

/// テキストをクリップボードへ入れる (履歴からの再貼付用)。
///
/// 注入はしないので、履歴除外フォーマットも付ける。発話内容が
/// Win+V 履歴やクラウドへ流れるのは通常の貼付時と同じく避けたい。
pub fn set_clipboard_text(text: &str) -> Result<(), InjectOutcome> {
    let _guard = ClipboardGuard::open()?;
    write_payload(text).map_err(|e| {
        log::error!("クリップボードへの書き込みに失敗: {e}");
        InjectOutcome::ClipboardFailed
    })
}

/// クリップボードを開いている間だけ生きる RAII ガード。
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Result<Self, InjectOutcome> {
        for attempt in 1..=CLIPBOARD_OPEN_ATTEMPTS {
            // SAFETY: 所有ウィンドウ無し (None) で開く。EmptyClipboard は
            // 所有者 NULL でも成功する (遅延レンダリングは使わない)。
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            if attempt < CLIPBOARD_OPEN_ATTEMPTS {
                std::thread::sleep(CLIPBOARD_OPEN_RETRY_DELAY);
            }
        }
        log::warn!(
            "クリップボードを {CLIPBOARD_OPEN_ATTEMPTS} 回試しても開けませんでした (他プロセスが使用中)"
        );
        Err(InjectOutcome::ClipboardBusy)
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: 開いているクリップボードを閉じるだけ。
        if let Err(e) = unsafe { CloseClipboard() } {
            log::warn!("クリップボードを閉じられません: {e}");
        }
    }
}

/// 現在クリップボードにある形式を列挙する。クリップボードを開いた状態で呼ぶこと。
fn enumerate_formats() -> Vec<u32> {
    let mut formats = Vec::new();
    let mut current = 0u32;
    loop {
        // SAFETY: クリップボードは開いている。0 を渡すと最初の形式から始まる。
        current = unsafe { EnumClipboardFormats(current) };
        if current == 0 {
            break;
        }
        formats.push(current);
        // 壊れた実装で無限ループしないよう上限を設ける。
        if formats.len() > 256 {
            log::warn!("クリップボード形式が多すぎるため列挙を打ち切ります");
            break;
        }
    }
    formats
}

/// R6: 退避できず失われる形式の名前を返す。
///
/// **`CF_UNICODETEXT` があるときは何も返さない。** リッチテキストのコピーでは
/// HTML/RTF と一緒にテキストも載っているのが普通で、そこで毎回警告を出すと
/// 通知が無意味になる。本当に困るのは「テキストが無く、画像やファイルが
/// 主内容だった」場合 — そのときだけ鳴らす。
///
/// Win32 に触れない純関数なのでテストできる。
fn lost_format_names(formats: &[u32]) -> Vec<String> {
    if formats.contains(&CF_UNICODETEXT) {
        return Vec::new();
    }
    // 隣接要素しか見ない `Vec::dedup` では「画像・ファイル・画像」のような
    // 並びが畳めない。出現順を保ったまま既出を弾く。
    let mut seen = std::collections::HashSet::new();
    formats
        .iter()
        .filter_map(|f| significant_format_name(*f))
        .filter(|name| seen.insert(*name))
        .map(str::to_string)
        .collect()
}

/// 失われると困る形式に日本語名を与える。テキスト系や未知の形式は `None`。
fn significant_format_name(format: u32) -> Option<&'static str> {
    match format {
        CF_BITMAP | CF_DIB | CF_DIBV5 => Some("画像"),
        CF_HDROP => Some("ファイル"),
        CF_TIFF => Some("画像 (TIFF)"),
        CF_METAFILEPICT | CF_ENHMETAFILE => Some("図形"),
        CF_WAVE | CF_RIFF => Some("音声"),
        CF_PALETTE => Some("パレット"),
        CF_SYLK | CF_DIF => Some("表データ"),
        // テキスト系と付随情報は「失われた」と言うほどのものではない。
        CF_TEXT | CF_OEMTEXT | CF_UNICODETEXT | CF_LOCALE => None,
        _ => None,
    }
}

/// 登録形式の名前を引く (デバッグ用)。
#[allow(dead_code)]
fn registered_format_name(format: u32) -> Option<String> {
    let mut buf = [0u16; 256];
    // SAFETY: buf は有効なスライス。標準形式では 0 が返る。
    let len = unsafe { GetClipboardFormatNameW(format, &mut buf) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// `CF_UNICODETEXT` を読む。クリップボードを開いた状態で呼ぶこと。
///
/// # Safety
/// クリップボードが開かれている必要がある。
unsafe fn read_unicode_text() -> Option<String> {
    // SAFETY: 呼び出し側がクリップボードを開いている。
    let handle: HANDLE = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
    if handle.0.is_null() {
        return None;
    }
    let hglobal = HGLOBAL(handle.0);

    // SAFETY: handle はクリップボード所有の有効な HGLOBAL。
    let ptr = unsafe { GlobalLock(hglobal) } as *const u16;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: ロック済みのブロックのサイズを問い合わせる。
    let bytes = unsafe { GlobalSize(hglobal) };
    let max_chars = bytes / 2;

    let mut text = Vec::with_capacity(max_chars);
    for i in 0..max_chars {
        // SAFETY: i < max_chars なのでブロック内。
        let ch = unsafe { *ptr.add(i) };
        if ch == 0 {
            break;
        }
        text.push(ch);
    }
    // SAFETY: 上で成功した GlobalLock と対。ロック数が 0 になると Err を返すが
    // それは正常なので無視する。
    let _ = unsafe { GlobalUnlock(hglobal) };

    if text.is_empty() {
        // 空文字は「退避すべき内容が無い」とみなす。
        return None;
    }
    Some(String::from_utf16_lossy(&text))
}

/// 整形テキストと履歴除外フォーマットを書き込む。
fn write_payload(text: &str) -> windows::core::Result<()> {
    // SAFETY: クリップボードは開いている。
    unsafe { EmptyClipboard() }?;
    set_unicode_text(text)?;

    // 履歴・クラウド同期からの除外。失敗しても致命ではない
    // (貼付自体は成立する) のでログに留める。
    for name in EXCLUSION_FORMATS {
        match register_format(name) {
            Some(format) => {
                if let Err(e) = set_dword(format, 0) {
                    log::warn!("{name} を設定できません: {e}");
                }
            }
            None => log::warn!("{name} を登録できません"),
        }
    }
    Ok(())
}

fn register_format(name: &str) -> Option<u32> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: wide は NUL 終端の有効な UTF-16 文字列。
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    (id != 0).then_some(id)
}

/// UTF-16 + NUL を `CF_UNICODETEXT` として設定する。
fn set_unicode_text(text: &str) -> windows::core::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = std::mem::size_of_val(wide.as_slice());
    let handle = alloc_and_fill(bytes, |dst| {
        // SAFETY: dst は bytes バイト確保済み。wide も同じ長さ。
        unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, dst, bytes) };
    })?;
    set_clipboard_handle(CF_UNICODETEXT, handle)
}

/// 値 0 の DWORD をその形式で設定する (履歴除外フラグ用)。
fn set_dword(format: u32, value: u32) -> windows::core::Result<()> {
    let bytes = std::mem::size_of::<u32>();
    let handle = alloc_and_fill(bytes, |dst| {
        // SAFETY: dst は 4 バイト確保済み。
        unsafe { std::ptr::copy_nonoverlapping(value.to_ne_bytes().as_ptr(), dst, bytes) };
    })?;
    set_clipboard_handle(format, handle)
}

/// `GMEM_MOVEABLE` で確保して中身を埋める。
fn alloc_and_fill(
    bytes: usize,
    fill: impl FnOnce(*mut u8),
) -> windows::core::Result<HGLOBAL> {
    // SAFETY: サイズは正で、クリップボードへ渡すため GMEM_MOVEABLE を使う。
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }?;
    // SAFETY: 直前に確保したハンドル。
    let ptr = unsafe { GlobalLock(handle) } as *mut u8;
    if ptr.is_null() {
        // SAFETY: まだクリップボードに渡していないので解放責任はこちらにある。
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err(windows::core::Error::from_thread());
    }
    fill(ptr);
    // SAFETY: 上の GlobalLock と対。
    let _ = unsafe { GlobalUnlock(handle) };
    Ok(handle)
}

/// `SetClipboardData` を呼ぶ。**成功したらメモリの所有権は OS に移る**ので
/// 解放してはいけない。失敗したときだけこちらで解放する。
fn set_clipboard_handle(format: u32, handle: HGLOBAL) -> windows::core::Result<()> {
    // SAFETY: クリップボードは開いており、handle は GMEM_MOVEABLE で確保済み。
    match unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) } {
        Ok(_) => Ok(()),
        Err(e) => {
            // SAFETY: 失敗したので所有権はまだこちらにある。
            let _ = unsafe { GlobalFree(Some(handle)) };
            Err(e)
        }
    }
}

/// 現在のクリップボードシーケンス番号。クリップボードを開く必要はない。
fn current_sequence() -> u32 {
    // SAFETY: 引数なし。
    unsafe { GetClipboardSequenceNumber() }
}

/// ウィンドウを所有するプロセス ID (取れなければ 0)。
fn window_process_id(hwnd: isize) -> u32 {
    if hwnd == 0 {
        return 0;
    }
    let mut pid = 0u32;
    // SAFETY: hwnd は非 0、出力先はスタック上の u32。
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
            HWND(hwnd as *mut _),
            Some(&mut pid),
        )
    };
    pid
}

/// 現在の前景ウィンドウ。
fn current_foreground() -> isize {
    // SAFETY: 引数なし。NULL でありうる。
    let hwnd: HWND = unsafe { GetForegroundWindow() };
    hwnd.0 as isize
}

/// 復元するかどうかの判断 (純関数なのでテストできる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreDecision {
    Restore,
    /// 貼付後にクリップボードが変わっている = ユーザーが新しくコピーした。
    SkipClipboardChanged,
}

/// R3-b の判断。
///
/// 自分が設定した直後のシーケンス番号と、復元しようとしている今の番号を比べる。
/// 変わっていれば誰かが割り込んでいるので、**復元してその操作を壊してはいけない**。
///
/// 呼び出し側は**クリップボードを握った状態**でこれを呼ぶこと。
/// 判定と書き込みが分かれていると、その隙にコピーされた内容を壊す。
pub fn restore_decision(recorded: u32, current: u32) -> RestoreDecision {
    if recorded != current {
        return RestoreDecision::SkipClipboardChanged;
    }
    RestoreDecision::Restore
}

/// 物理的に押されている修飾キーを返す。
fn modifiers_down() -> Vec<&'static str> {
    let checks: [(VIRTUAL_KEY, &'static str); 5] = [
        (VK_CONTROL, "Ctrl"),
        (VK_SHIFT, "Shift"),
        (VK_MENU, "Alt"),
        (VK_LWIN, "Win"),
        (VK_RWIN, "Win"),
    ];
    let mut held = Vec::new();
    for (key, name) in checks {
        // SAFETY: 引数は仮想キーコードのみ。
        let state = unsafe { GetAsyncKeyState(key.0 as i32) };
        // 最上位ビットが立っていれば現在押されている。
        if (state as u16) & 0x8000 != 0 && !held.contains(&name) {
            held.push(name);
        }
    }
    held
}

/// 修飾キーが離されるのを一度だけ待ち、それでも押されていれば返す。
///
/// PTT (右 Ctrl 長押し) では離した直後なので普通は空。
/// トグル停止 (短押し) の直後は指が残っていることがあるので一度待つ。
fn modifiers_held_after_settling() -> Option<Vec<&'static str>> {
    let held = modifiers_down();
    if held.is_empty() {
        return None;
    }
    log::debug!("修飾キーが押されています ({})。離されるのを待ちます", held.join("+"));
    std::thread::sleep(MODIFIER_SETTLE_DELAY);

    let still = modifiers_down();
    if still.is_empty() {
        None
    } else {
        Some(still)
    }
}

/// Ctrl+V を送出する。
///
/// `dwExtraInfo` に [`SELF_INJECTED_MARKER`] を載せ、自分が出した入力だと
/// 後から判別できるようにする。
fn send_ctrl_v() -> Result<(), String> {
    let inputs = [
        key_input(VK_CONTROL, false),
        key_input(VK_V, false),
        key_input(VK_V, true),
        key_input(VK_CONTROL, true),
    ];

    // SAFETY: inputs は有効な INPUT 配列で、cbsize は正しい構造体サイズ。
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) } as usize;
    if sent == inputs.len() {
        return Ok(());
    }

    // 途中まで送れた場合、押しっぱなしのキーが残る。Ctrl が押されたままだと
    // 以降ユーザーのキー入力がすべてショートカット扱いになり、
    // 「キーボードが壊れた」ようにしか見えない状態を残してしまう。
    // 送出済みの押下に対応する解放を補償する。
    release_pressed_keys(&inputs[..sent]);

    // 0 は UIPI や他プロセスの入力ブロックで起きる。
    Err(format!(
        "{} 件中 {sent} 件しか送出できませんでした (昇格アプリが前景の可能性)",
        inputs.len()
    ))
}

/// 送出済みの入力列から「押したまま」のキーを拾って解放する。
///
/// 純粋な判定部分は [`keys_needing_release`] に切り出してあり、テストできる。
fn release_pressed_keys(sent: &[INPUT]) {
    let pending = keys_needing_release(sent);
    if pending.is_empty() {
        return;
    }
    log::warn!(
        "部分送出のため {} 個のキーを解放します",
        pending.len()
    );
    let releases: Vec<INPUT> = pending.iter().map(|k| key_input(*k, true)).collect();
    // SAFETY: releases は有効な INPUT 配列。ここが失敗しても打つ手はない。
    let released = unsafe { SendInput(&releases, std::mem::size_of::<INPUT>() as i32) };
    if released as usize != releases.len() {
        log::error!(
            "キーの解放も送出できませんでした ({released}/{})。修飾キーが押されたままの可能性があります",
            releases.len()
        );
    }
}

/// 送出済み入力列のうち、まだ解放されていないキーを**押した逆順**で返す。
///
/// 逆順なのは、修飾キーを最後に離すのが自然な順序だから
/// (Ctrl 押下 → V 押下 まで送れたなら V → Ctrl の順で離す)。
fn keys_needing_release(sent: &[INPUT]) -> Vec<VIRTUAL_KEY> {
    let mut pressed: Vec<VIRTUAL_KEY> = Vec::new();
    for input in sent {
        if input.r#type != INPUT_KEYBOARD {
            continue;
        }
        // SAFETY: type が INPUT_KEYBOARD なので ki が有効。
        let ki = unsafe { input.Anonymous.ki };
        if ki.dwFlags.0 & KEYEVENTF_KEYUP.0 == 0 {
            pressed.push(ki.wVk);
        } else if let Some(pos) = pressed.iter().rposition(|k| *k == ki.wVk) {
            pressed.remove(pos);
        }
    }
    pressed.reverse();
    pressed
}

fn key_input(key: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: SELF_INJECTED_MARKER,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- R3-b: 復元判断 ---

    #[test]
    fn restores_when_nothing_touched_the_clipboard() {
        assert_eq!(restore_decision(42, 42), RestoreDecision::Restore);
    }

    #[test]
    fn skips_restore_when_the_user_copied_something_new() {
        // 貼付後にユーザーが別のものをコピーした。上書きすればその操作を壊す。
        assert_eq!(restore_decision(42, 43), RestoreDecision::SkipClipboardChanged);
    }

    #[test]
    fn sequence_number_wraparound_is_treated_as_a_change() {
        // u32 は一周する。等値比較しかしないので、一周後の一致は
        // 事実上起こらない (起きても「変わっていない」と誤判定するだけで、
        // その確率は 2^32 分の 1)。ここでは境界値で panic しないことを確認する。
        assert_eq!(restore_decision(u32::MAX, 0), RestoreDecision::SkipClipboardChanged);
        assert_eq!(restore_decision(u32::MAX, u32::MAX), RestoreDecision::Restore);
    }

    // --- 貼付後の方針 (Typeless 互換の「残す」) ---

    #[test]
    fn keep_policy_never_enters_the_restore_path() {
        // 退避があっても復元しない = sleep も restore_if_unchanged も通らない。
        let action = post_paste_action(ClipboardPolicy::Keep, Some("元の内容".to_string()));
        assert!(
            matches!(action, PostPasteAction::KeepInjectedText { .. }),
            "Keep なのに復元しようとしている: {action:?}"
        );
    }

    #[test]
    fn restore_policy_still_restores_the_backup() {
        // 陰性コントロール。上のテストが「常に Keep」で通ってしまわないための対。
        let action = post_paste_action(
            ClipboardPolicy::Restore {
                delay: Duration::from_millis(300),
            },
            Some("元の内容".to_string()),
        );
        assert_eq!(
            action,
            PostPasteAction::RestoreAfter {
                delay: Duration::from_millis(300),
                backup: "元の内容".to_string(),
            },
            "Restore の既存挙動が変わっている"
        );
    }

    #[test]
    fn restore_policy_without_a_backup_keeps_the_text() {
        // 元が空・画像だけで退避できていない場合。戻す先が無いので待つ意味も無い。
        let action = post_paste_action(
            ClipboardPolicy::Restore {
                delay: Duration::from_millis(300),
            },
            None,
        );
        assert!(matches!(action, PostPasteAction::KeepInjectedText { .. }));
    }

    #[test]
    fn keep_policy_holds_the_text_even_without_a_backup() {
        let action = post_paste_action(ClipboardPolicy::Keep, None);
        assert!(matches!(action, PostPasteAction::KeepInjectedText { .. }));
    }

    #[test]
    fn keeping_the_text_promises_a_manual_paste_that_is_true() {
        // Keep の成功時は HoldsInjectedText になる。この状態の案内文
        // (「Ctrl+V で貼り付け可」) は実際に正しい。
        assert!(ClipboardState::HoldsInjectedText.holds_injected_text());
        // 送出が成功していれば、たとえテキストが残っていてもトーストは出さない
        // (メッセージが無いので notify も呼ばれない)。毎回鳴ると無意味になる。
        assert!(
            outcome_message(InjectOutcome::Injected, ClipboardState::HoldsInjectedText).is_none(),
            "Keep 既定で毎回メッセージが出る"
        );
    }

    // --- m7: クリップボードの状態と案内文の整合 ---

    #[test]
    fn only_holding_state_promises_manual_paste() {
        assert!(ClipboardState::HoldsInjectedText.holds_injected_text());
        for state in [
            ClipboardState::Untouched,
            ClipboardState::RestoredOriginal,
            // ユーザーが新しくコピーした = 整形テキストはもう無い。
            ClipboardState::ReplacedByUser,
            ClipboardState::Lost,
        ] {
            assert!(
                !state.holds_injected_text(),
                "{state:?} で「Ctrl+V で貼れます」と案内してしまう"
            );
        }
    }

    #[test]
    fn recovery_text_matches_the_actual_clipboard_state() {
        let holding = outcome_message(
            InjectOutcome::AbortedFocusChanged,
            ClipboardState::HoldsInjectedText,
        )
        .expect("説明がある");
        assert!(holding.contains("Ctrl+V"), "{holding}");

        // 整形テキストが残っていないなら Ctrl+V を案内してはいけない。
        let replaced = outcome_message(
            InjectOutcome::AbortedFocusChanged,
            ClipboardState::ReplacedByUser,
        )
        .expect("説明がある");
        assert!(!replaced.contains("Ctrl+V"), "嘘の案内: {replaced}");
        assert!(replaced.contains("画面からコピー"), "{replaced}");
    }

    #[test]
    fn failed_restore_after_a_successful_send_is_reported() {
        // 送出は成功したがクリップボードが壊れた場合、黙って終わらない。
        let msg = outcome_message(InjectOutcome::Injected, ClipboardState::Lost)
            .expect("説明がある");
        assert!(msg.contains("復元できませんでした"), "{msg}");
        // 正常系は黙っている。
        assert!(
            outcome_message(InjectOutcome::Injected, ClipboardState::RestoredOriginal).is_none()
        );
    }

    // --- M3: 通知の出し分け ---

    fn report_with(outcome: InjectOutcome, state: ClipboardState, lost: &[&str]) -> InjectReport {
        InjectReport {
            outcome,
            injected: outcome == InjectOutcome::Injected,
            clipboard_state: state,
            lost_formats: lost.iter().map(|s| s.to_string()).collect(),
            message: outcome_message(outcome, state),
        }
    }

    #[test]
    fn user_action_is_required_when_text_waits_in_the_clipboard() {
        let report = report_with(
            InjectOutcome::AbortedFocusChanged,
            ClipboardState::HoldsInjectedText,
            &[],
        );
        assert!(report.needs_user_action(), "手動貼り付けの案内が埋もれる");
    }

    #[test]
    fn user_action_is_required_when_something_was_destroyed() {
        // R6: 画像が失われたことは常駐中でも気づけないといけない。
        let report = report_with(
            InjectOutcome::Injected,
            ClipboardState::RestoredOriginal,
            &["画像"],
        );
        assert!(report.needs_user_action());

        // 復元に失敗してクリップボードが空になった場合も同じ。
        let report = report_with(InjectOutcome::Injected, ClipboardState::Lost, &[]);
        assert!(report.needs_user_action());
    }

    #[test]
    fn keeping_the_text_after_a_successful_paste_needs_no_toast() {
        // `ClipboardPolicy::Keep` が既定なので、**貼付に成功した通常の完了も**
        // HoldsInjectedText で終わる。ここでトーストを出すと毎回鳴り、
        // 通知そのものが意味を失う。手当てが要るのは送出できなかった場合だけ
        // (それは user_action_is_required_when_text_waits_in_the_clipboard が固定)。
        let report = report_with(
            InjectOutcome::Injected,
            ClipboardState::HoldsInjectedText,
            &[],
        );
        assert!(report.injected, "前提: 送出は成功している");
        assert!(!report.needs_user_action(), "毎回トーストが鳴る構造になっている");
    }

    #[test]
    fn a_clean_injection_needs_no_toast() {
        let report = report_with(
            InjectOutcome::Injected,
            ClipboardState::RestoredOriginal,
            &[],
        );
        assert!(!report.needs_user_action(), "正常系で通知を出している");
    }

    #[test]
    fn a_user_replaced_clipboard_needs_no_toast() {
        // ユーザー自身の操作の結果なので、驚きはない。
        let report = report_with(
            InjectOutcome::Injected,
            ClipboardState::ReplacedByUser,
            &[],
        );
        assert!(!report.needs_user_action());
    }

    // --- M2: 書き込み失敗時の復旧 ---

    #[test]
    fn write_failure_restores_the_backup_when_it_can() {
        let mut called_with = None;
        let state = write_failure_state(Some("元のテキスト"), |t| {
            called_with = Some(t.to_string());
            true
        });
        assert_eq!(state, ClipboardState::RestoredOriginal);
        assert_eq!(
            called_with.as_deref(),
            Some("元のテキスト"),
            "書き戻しを試みていない"
        );
    }

    #[test]
    fn write_failure_reports_loss_when_the_rewrite_also_fails() {
        let state = write_failure_state(Some("元のテキスト"), |_| false);
        assert_eq!(state, ClipboardState::Lost);
    }

    #[test]
    fn write_failure_without_a_backup_is_a_loss() {
        // 元が画像などでテキストを退避できていなかった場合。
        let mut attempted = false;
        let state = write_failure_state(None, |_| {
            attempted = true;
            true
        });
        assert_eq!(state, ClipboardState::Lost);
        assert!(!attempted, "退避が無いのに書き戻そうとしている");
    }

    #[test]
    fn a_destroyed_clipboard_always_needs_user_action() {
        // M2 の要点: 失敗経路でも「何が失われたか」を握り潰さない。
        let report = report_with(
            InjectOutcome::ClipboardFailed,
            ClipboardState::Lost,
            &["画像"],
        );
        assert!(report.needs_user_action());
        assert!(!report.lost_formats.is_empty(), "失われた形式が空になっている");
    }

    // --- m4: 部分送出の補償 ---

    #[test]
    fn partial_send_releases_the_keys_already_pressed() {
        // Ctrl 押下 + V 押下 まで送れて止まった場合。
        let sent = [key_input(VK_CONTROL, false), key_input(VK_V, false)];
        // 押した逆順で離す。
        assert_eq!(keys_needing_release(&sent), vec![VK_V, VK_CONTROL]);
    }

    #[test]
    fn only_ctrl_pressed_releases_only_ctrl() {
        let sent = [key_input(VK_CONTROL, false)];
        assert_eq!(keys_needing_release(&sent), vec![VK_CONTROL]);
    }

    #[test]
    fn a_complete_sequence_needs_no_release() {
        let sent = [
            key_input(VK_CONTROL, false),
            key_input(VK_V, false),
            key_input(VK_V, true),
            key_input(VK_CONTROL, true),
        ];
        assert!(
            keys_needing_release(&sent).is_empty(),
            "全部送れたのに解放しようとしている"
        );
    }

    #[test]
    fn a_partially_released_sequence_releases_the_rest() {
        // Ctrl 押下 → V 押下 → V 解放 まで送れた。Ctrl だけ残る。
        let sent = [
            key_input(VK_CONTROL, false),
            key_input(VK_V, false),
            key_input(VK_V, true),
        ];
        assert_eq!(keys_needing_release(&sent), vec![VK_CONTROL]);
    }

    #[test]
    fn nothing_sent_means_nothing_to_release() {
        assert!(keys_needing_release(&[]).is_empty());
    }

    // --- R6: 失われる形式の判定 ---

    #[test]
    fn image_only_clipboard_reports_a_loss() {
        assert_eq!(lost_format_names(&[CF_DIB, CF_BITMAP]), vec!["画像"]);
    }

    #[test]
    fn file_drop_reports_a_loss() {
        assert_eq!(lost_format_names(&[CF_HDROP]), vec!["ファイル"]);
    }

    #[test]
    fn rich_text_with_unicode_text_reports_nothing() {
        // ブラウザからのコピーは HTML と一緒にテキストも載る。
        // ここで毎回警告すると通知が意味を失う。
        let html = 49_405; // 登録形式 "HTML Format" の典型値
        assert!(lost_format_names(&[CF_UNICODETEXT, CF_TEXT, CF_LOCALE, html]).is_empty());
    }

    #[test]
    fn image_with_text_reports_nothing() {
        // テキストが取れているなら復元できるので騒がない。
        assert!(lost_format_names(&[CF_UNICODETEXT, CF_DIB]).is_empty());
    }

    #[test]
    fn empty_clipboard_reports_nothing() {
        assert!(lost_format_names(&[]).is_empty());
    }

    #[test]
    fn unknown_formats_are_not_reported() {
        // 未知の登録形式は「失われて困るもの」と決めつけない。
        assert!(lost_format_names(&[49_999]).is_empty());
    }

    #[test]
    fn multiple_losses_are_listed_once_each() {
        let names = lost_format_names(&[CF_DIB, CF_DIBV5, CF_HDROP]);
        assert_eq!(names, vec!["画像", "ファイル"], "重複が畳まれていない");
    }

    #[test]
    fn lost_formats_message_is_built_only_when_something_was_lost() {
        assert!(lost_formats_message(&[]).is_none());
        let msg = lost_formats_message(&["画像".to_string(), "ファイル".to_string()])
            .expect("メッセージがある");
        assert!(msg.contains("画像・ファイル"), "{msg}");
        assert!(msg.contains("失われました"), "{msg}");
    }

    // --- 結末とメッセージ ---

    #[test]
    fn aborted_outcomes_leave_the_text_in_the_clipboard() {
        // R4: 貼付が不達なら復元せず、手で貼れる状態にしておく。
        for outcome in [
            InjectOutcome::AbortedFocusChanged,
            InjectOutcome::AbortedTargetUnknown,
            InjectOutcome::AbortedModifierStuck,
            InjectOutcome::SendFailed,
        ] {
            let msg = outcome_message(outcome, ClipboardState::HoldsInjectedText)
                .expect("説明がある");
            assert!(
                msg.contains("クリップボード") && msg.contains("Ctrl+V"),
                "復旧方法が伝わらない: {msg}"
            );
        }
    }

    #[test]
    fn successful_and_skipped_outcomes_have_no_message() {
        assert!(
            outcome_message(InjectOutcome::Injected, ClipboardState::RestoredOriginal).is_none()
        );
        assert!(outcome_message(InjectOutcome::Disabled, ClipboardState::Untouched).is_none());
        assert!(outcome_message(InjectOutcome::EmptyText, ClipboardState::Untouched).is_none());
    }

    #[test]
    fn focus_change_message_tells_the_user_how_to_recover() {
        let msg = outcome_message(
            InjectOutcome::AbortedFocusChanged,
            ClipboardState::HoldsInjectedText,
        )
        .expect("説明がある");
        assert!(msg.contains("挿入先が変わった"), "{msg}");
        assert!(msg.contains("Ctrl+V"), "{msg}");
    }

    #[test]
    fn outcome_serializes_in_snake_case_for_the_ui() {
        let json = serde_json::to_string(&InjectOutcome::AbortedFocusChanged).expect("直列化");
        assert_eq!(json, r#""aborted_focus_changed""#);
        let json = serde_json::to_string(&InjectOutcome::Injected).expect("直列化");
        assert_eq!(json, r#""injected""#);
    }

    #[test]
    fn empty_text_is_rejected_without_touching_the_clipboard() {
        // 実クリップボードに触れないことが要点なので、実機でも安全に走る。
        let report = inject(
            "   \n  ",
            InjectTarget::new(0x1234, 42),
            ClipboardPolicy::Restore {
                delay: Duration::ZERO,
            },
        );
        assert_eq!(report.outcome, InjectOutcome::EmptyText);
        assert!(!report.injected);
        assert_eq!(report.clipboard_state, ClipboardState::Untouched);
        assert!(report.lost_formats.is_empty());
    }

    #[test]
    fn self_injected_marker_is_stable() {
        // hotkey 側の絞り込みで参照する定数。値が変わると識別できなくなる。
        assert_eq!(SELF_INJECTED_MARKER, 0x4E4F_5856);
        assert_eq!(&SELF_INJECTED_MARKER.to_be_bytes()[4..], b"NOXV");
    }

    #[test]
    fn ctrl_v_sequence_is_press_press_release_release() {
        // 送出順を間違えると別のキー操作になる。構造体の中身で確認する。
        let inputs = [
            key_input(VK_CONTROL, false),
            key_input(VK_V, false),
            key_input(VK_V, true),
            key_input(VK_CONTROL, true),
        ];
        let flags: Vec<u32> = inputs
            .iter()
            // SAFETY: すべて key_input が作った INPUT_KEYBOARD。
            .map(|i| unsafe { i.Anonymous.ki }.dwFlags.0)
            .collect();
        assert_eq!(flags, vec![0, 0, KEYEVENTF_KEYUP.0, KEYEVENTF_KEYUP.0]);

        let keys: Vec<u16> = inputs
            .iter()
            .map(|i| unsafe { i.Anonymous.ki }.wVk.0)
            .collect();
        assert_eq!(keys, vec![VK_CONTROL.0, VK_V.0, VK_V.0, VK_CONTROL.0]);
    }

    #[test]
    fn every_synthetic_key_carries_the_marker() {
        for input in [key_input(VK_CONTROL, false), key_input(VK_V, true)] {
            // SAFETY: key_input が作った INPUT_KEYBOARD。
            assert_eq!(
                unsafe { input.Anonymous.ki }.dwExtraInfo,
                SELF_INJECTED_MARKER
            );
            assert_eq!(input.r#type, INPUT_KEYBOARD);
        }
    }

    // --- 実クリップボードを触る統合テスト ---
    //
    // クリップボードはプロセスをまたぐグローバル資源なので、並列実行すると
    // 互いに干渉する。`#[ignore]` にした上で、実行時は必ず
    // `--test-threads=1` を付けること:
    //
    //   cargo test -- --ignored --test-threads=1 clipboard_
    //
    // どのテストも終了時に元の内容へ戻す。

    /// 実際に退避 → 設定 → 復元が一巡することを確認する。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn clipboard_round_trip_preserves_the_original_text() {
        // ユーザーの実クリップボードを壊さないよう、先に退避しておく。
        let user_original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        let sentinel = "nox-voice テスト用の元テキスト";
        restore_text(sentinel).expect("前準備: 元テキストを置く");

        let prepared = prepare_clipboard("整形後のテキスト").expect("設定できる");
        assert_eq!(
            prepared.backup.as_deref(),
            Some(sentinel),
            "元テキストを退避できていない"
        );

        // 設定した内容が読めること。
        {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            let now = unsafe { read_unicode_text() };
            assert_eq!(now.as_deref(), Some("整形後のテキスト"));
        }

        // 復元する。
        assert_eq!(
            restore_decision(prepared.sequence, prepared.sequence),
            RestoreDecision::Restore
        );
        restore_text(&prepared.backup.expect("退避がある")).expect("復元できる");

        {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            let restored = unsafe { read_unicode_text() };
            assert_eq!(restored.as_deref(), Some(sentinel), "元に戻っていない");
        }

        // 後片付け: ユーザーのクリップボードへ戻す。
        if let Some(text) = user_original {
            restore_text(&text).expect("ユーザーの内容へ戻す");
        }
    }

    /// `Keep` の貼付後処理が、実クリップボードの内容を変えないことを見る。
    ///
    /// **範囲の限界を正直に書いておく**: このテストは `inject()` を呼ばず、
    /// `prepare_clipboard` + 純関数 `post_paste_action` の組で確かめる
    /// (`inject()` は前景の奪取と実キー送出を伴い、自動テストでは成立しない
    /// — design.md「E2Eテストハーネスの安全則」)。したがって
    /// **`inject()` 側の `KeepInjectedText` 分岐が誤って書き込む退行は捕捉できない**。
    /// その分岐が不活性であることは純関数テスト側で固定している。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn clipboard_keep_policy_leaves_the_injected_text() {
        let user_original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        let sentinel = "nox-voice テスト用の元テキスト";
        restore_text(sentinel).expect("前準備: 元テキストを置く");

        let prepared = prepare_clipboard("残すべき整形テキスト").expect("設定できる");
        assert_eq!(
            prepared.backup.as_deref(),
            Some(sentinel),
            "Keep でも退避自体は続けること (書き込み失敗時の唯一の救済)"
        );

        // Keep なら復元経路へ入らない。
        let action = post_paste_action(ClipboardPolicy::Keep, prepared.backup);
        assert!(matches!(action, PostPasteAction::KeepInjectedText { .. }));

        let after = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };
        assert_eq!(
            after.as_deref(),
            Some("残すべき整形テキスト"),
            "整形テキストが残っていない (Ctrl+V でやり直せない)"
        );

        // 後片付け: ユーザーのクリップボードへ必ず戻す。
        if let Some(text) = user_original {
            restore_text(&text).expect("ユーザーの内容へ戻す");
        }
    }

    /// クリップボードのみモード: 貼らずに、確かにテキストが入っていること。
    ///
    /// この経路は前景照合を通らない (フォーカスが無くても成立するのが
    /// 存在理由)。「中止されたが副産物として残った」ではなく
    /// **指定どおりの結末**として `ClipboardOnly` を返す必要がある。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn copy_only_leaves_the_text_without_pasting() {
        let user_original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        restore_text("nox-voice テスト用の元テキスト").expect("前準備");

        let report = copy_only("コピーだけされるテキスト");
        assert_eq!(report.outcome, InjectOutcome::ClipboardOnly);
        assert!(!report.injected, "貼り付けを送出してはいけない");
        assert_eq!(report.clipboard_state, ClipboardState::HoldsInjectedText);
        assert!(
            report.message.is_none(),
            "指定どおりの結末で通知文を出してはいけない: {:?}",
            report.message
        );
        assert!(
            !report.needs_user_action(),
            "成功のたびにトーストが鳴ると通知が意味を失う (R6)"
        );

        let after = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };
        assert_eq!(
            after.as_deref(),
            Some("コピーだけされるテキスト"),
            "クリップボードに入っていない (この経路の結果そのものが消えている)"
        );

        // 後片付け: ユーザーのクリップボードへ必ず戻す。
        if let Some(text) = user_original {
            restore_text(&text).expect("ユーザーの内容へ戻す");
        }
    }

    /// 空文字ではクリップボードに触れない (前の内容を無意味に壊さない)。
    #[test]
    fn copy_only_does_not_touch_the_clipboard_for_empty_text() {
        let report = copy_only("   
  ");
        assert_eq!(report.outcome, InjectOutcome::EmptyText);
        assert_eq!(report.clipboard_state, ClipboardState::Untouched);
    }

    /// M1 回帰: ガードを握った状態でシーケンスを再確認し、
    /// 割り込まれていたら上書きしない。
    ///
    /// チェックとガード取得が分離していると、リトライ待ちの間 (最大 180ms) に
    /// ユーザーがコピーした内容を「待ってから」破壊してしまう。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn clipboard_restore_aborts_when_someone_intervenes() {
        let user_original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        // 自分が整形テキストを置き、その時点のシーケンス番号を得る。
        let prepared = prepare_clipboard("整形テキスト").expect("設定できる");

        // ここでユーザーが別のものをコピーした、という状況を作る。
        restore_text("ユーザーが後からコピーした内容").expect("割り込みを再現");

        // 退避内容で復元しようとしても、割り込みを検知して何もしないこと。
        let outcome = restore_if_unchanged("退避しておいた元テキスト", prepared.sequence);
        assert_eq!(
            outcome,
            RestoreOutcome::SkippedChanged,
            "割り込みを検知できていない"
        );

        let after = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };
        assert_eq!(
            after.as_deref(),
            Some("ユーザーが後からコピーした内容"),
            "ユーザーのコピーを上書きで壊した"
        );

        if let Some(text) = user_original {
            restore_text(&text).expect("ユーザーの内容へ戻す");
        }
    }

    /// 割り込みが無ければ、同じ経路でちゃんと復元されること
    /// (上のテストが「常にスキップ」で通ってしまわないための対)。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn clipboard_restore_proceeds_without_intervention() {
        let user_original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        let prepared = prepare_clipboard("整形テキスト").expect("設定できる");
        let outcome = restore_if_unchanged("退避しておいた元テキスト", prepared.sequence);
        assert_eq!(outcome, RestoreOutcome::Restored);

        let after = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };
        assert_eq!(after.as_deref(), Some("退避しておいた元テキスト"));

        if let Some(text) = user_original {
            restore_text(&text).expect("ユーザーの内容へ戻す");
        }
    }

    /// 設定するとシーケンス番号が進むこと (R3-b の前提)。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn clipboard_writes_advance_the_sequence_number() {
        let original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        let before = current_sequence();
        let prepared = prepare_clipboard("シーケンス確認").expect("設定できる");
        assert_ne!(
            before, prepared.sequence,
            "書き込んでもシーケンス番号が進んでいない (介入検知が働かない)"
        );

        // 後片付け: 元に戻す。
        if let Some(text) = original {
            restore_text(&text).expect("復元できる");
        }
    }

    /// 履歴除外フォーマットが実際に載ること。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn clipboard_payload_carries_history_exclusion_formats() {
        let original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        prepare_clipboard("履歴除外の確認").expect("設定できる");

        let _guard = ClipboardGuard::open().expect("開ける");
        let formats = enumerate_formats();
        for name in EXCLUSION_FORMATS {
            let id = register_format(name).expect("形式を登録できる");
            assert!(
                formats.contains(&id),
                "{name} がクリップボードに載っていない (Win+V 履歴に残る)"
            );
        }
        drop(_guard);

        if let Some(text) = original {
            restore_text(&text).expect("復元できる");
        }
    }

    // --- E2E: 受け手側の証跡で確認する ---
    //
    // design.md の運用則「送信成功≠到達」に従い、`InjectOutcome::Injected`
    // (= SendInput が成功した) だけでは貼られた証明にならない。
    // メモ帳を起こして、その**中身**を読んで確認する。
    //
    //   cargo test -- --ignored --test-threads=1 --nocapture e2e_paste
    //
    // # ユーザーの環境を壊さないための約束
    //
    // このテストは他人のプロセスを操作する。壊し方を具体的に潰しておく:
    //
    // - **既にメモ帳が動いていたら何もせずスキップする。** Windows 11 の
    //   メモ帳は 1 プロセスで複数ウィンドウを持つため、プロセス単位の操作は
    //   ユーザーの未保存タブを巻き込む。動いていなければ、これから起こす
    //   メモ帳は自分のものだと確定できる。
    // - **操作対象は自分が起動した PID のウィンドウだけ**に限定する。
    // - **後片付けは対象 HWND への `WM_CLOSE`。** プロセスの強制終了は
    //   自分が spawn した子ハンドルに対してのみ、最後の手段として行う。
    // - 貼付は inject() 自身が送出直前に前景 HWND を照合するので、
    //   メモ帳を前面にできなければ何も貼られずに中止される。

    /// メモ帳へ実際に貼り付き、受け手側から読み出せることを確認する。
    #[test]
    #[ignore = "メモ帳を起動する E2E。--test-threads=1 で実行すること"]
    fn e2e_paste_reaches_notepad() {
        use std::time::Instant;

        // ユーザーのメモ帳が開いていたら触らない。
        // (Win11 のメモ帳は単一プロセス複数ウィンドウなので、
        //  自分のウィンドウだけを閉じたつもりでも巻き添えが出やすい)
        let existing = notepad_windows();
        if !existing.is_empty() {
            println!(
                "メモ帳が既に {} ウィンドウ開いています。ユーザーの作業を壊さないためスキップします。\n\
                 メモ帳をすべて閉じてから実行してください",
                existing.len()
            );
            return;
        }

        let user_original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };
        // クリップボードは何があっても戻す。
        let restore_user_clipboard = || {
            if let Some(text) = &user_original {
                let _ = restore_text(text);
            }
        };

        let mut child = std::process::Command::new("notepad.exe")
            .spawn()
            .expect("メモ帳を起動できる");
        let child_pid = child.id();

        // 自分が起こしたメモ帳のウィンドウを探す。
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut target = 0isize;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            let windows = notepad_windows();
            // まず PID 一致で探す。
            if let Some((hwnd, _)) = windows.iter().find(|(_, pid)| *pid == child_pid) {
                target = *hwnd;
                break;
            }
            // 起動が別プロセスへ引き継がれることがある (アプリ実行エイリアス)。
            // 開始前にメモ帳が 1 つも無かったことは確認済みなので、
            // ここに在るメモ帳は自分が起こしたものと断定してよい。
            if let Some((hwnd, pid)) = windows.first() {
                println!("メモ帳が別プロセスへ引き継がれました (pid {pid})");
                target = *hwnd;
                break;
            }
        }
        if target == 0 {
            reap(&mut child);
            restore_user_clipboard();
            println!("メモ帳のウィンドウが見つかりませんでした。スキップします");
            return;
        }

        // 起動した文書が空であることを確認する。
        //
        // # 「プロセスが無い」≠「空の文書」 (実測でユーザーのメモを消しかけた)
        //
        // Win11 のメモ帳はセッション復元により、新プロセスでも**前回の
        // 未保存タブを内容ごと復元する**。プロセス不在の確認だけでは
        // 「これから開くウィンドウは無地」を保証できず、復元されたユーザーの
        // 文書へ貼り付けた上、後片付けの WM_SETTEXT("") が内容を消してしまう
        // (実測 2026-08-17。テスト出力に写っていた本文から復旧した)。
        // 空でなければ一切書き込まず、変更を加えないまま閉じてスキップする
        // (未変更なら閉じてもセッション復元の内容は保全される)。
        let restored = read_notepad_text(target).unwrap_or_default();
        if !restored.trim().is_empty() {
            // WM_SETTEXT で空にしてはいけない (それがまさに事故の経路)。
            // 変更していないので WM_CLOSE だけで保存確認も出ない。
            close_without_clearing(target, &mut child);
            restore_user_clipboard();
            println!(
                "起動したメモ帳がセッション復元で文書を持っています ({} 文字)。\n\
                 ユーザーの内容を壊さないためスキップします。\n\
                 メモ帳で該当タブを閉じて (保存するか破棄するか選んで) から再実行してください",
                restored.chars().count()
            );
            return;
        }

        // 前景を取りに行く。バックグラウンドのコンソールプロセスからは
        // フォアグラウンドロックで拒否されることがある (wiki の知見どおり)。
        // 取れなければ環境要因なので、失敗ではなくスキップにする。
        if !take_foreground(target) {
            close_test_notepad(target, &mut child);
            restore_user_clipboard();
            println!(
                "メモ帳を前景にできませんでした (フォアグラウンドロック)。スキップします。\n\
                 対話セッションで実行すると検証できます"
            );
            return;
        }

        let payload = "nox-voice E2E 確認テキスト";
        let target_pid = window_process_id(target);
        let report = inject(
            payload,
            InjectTarget::new(target, target_pid),
            ClipboardPolicy::Restore {
                delay: Duration::from_millis(300),
            },
        );
        let pasted = read_notepad_text(target);

        // 後片付けは検証より先に済ませる (assert で落ちてもメモ帳を残さない)。
        close_test_notepad(target, &mut child);
        restore_user_clipboard();

        assert_eq!(report.outcome, InjectOutcome::Injected, "送出できていない");
        let pasted = pasted.expect("メモ帳の本文を読めない");
        assert!(
            pasted.contains(payload),
            "受け手側に届いていない (送出成功≠到達): 実際の内容 = {pasted:?}"
        );
        println!("メモ帳の内容: {pasted:?}");
        println!("クリップボードの状態: {:?}", report.clipboard_state);
    }

    /// 動作中のメモ帳のトップレベルウィンドウを (HWND, PID) で列挙する。
    #[cfg(test)]
    fn notepad_windows() -> Vec<(isize, u32)> {
        use windows::Win32::UI::WindowsAndMessaging::FindWindowExW;

        let class: Vec<u16> = "Notepad".encode_utf16().chain(std::iter::once(0)).collect();
        let mut found = Vec::new();
        let mut prev: Option<HWND> = None;
        loop {
            // SAFETY: class は NUL 終端。prev は直前に得た有効な HWND。
            let hwnd = unsafe {
                FindWindowExW(None, prev, PCWSTR(class.as_ptr()), PCWSTR::null())
            };
            let Ok(hwnd) = hwnd else { break };
            if hwnd.0.is_null() {
                break;
            }
            found.push((hwnd.0 as isize, window_process_id(hwnd.0 as isize)));
            prev = Some(hwnd);
            if found.len() > 64 {
                break; // 想定外の数。無限ループを避ける。
            }
        }
        found
    }

    /// テストで起こしたメモ帳を閉じる。
    ///
    /// `taskkill` でオーナー PID を落とすのは**やってはいけない**。
    /// Windows 11 のメモ帳は 1 プロセスで複数ウィンドウを持つので、
    /// ユーザーの未保存タブごと巻き添えにする。
    /// 対象ウィンドウへ `WM_CLOSE` を送り、駄目なときだけ
    /// **自分が spawn した子プロセス**を回収する。
    #[cfg(test)]
    fn close_test_notepad(hwnd: isize, child: &mut std::process::Child) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            IsWindow, PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_CLOSE, WM_SETTEXT,
        };

        // 本文を空にしてから閉じる。変更が残っていると保存確認が出て、
        // ウィンドウが閉じずに残ってしまう。
        if let Some(edit) = notepad_edit_control(hwnd) {
            let empty: Vec<u16> = std::iter::once(0).collect();
            // SAFETY: edit は有効な HWND、empty は NUL 終端の UTF-16。
            unsafe {
                SendMessageTimeoutW(
                    edit,
                    WM_SETTEXT,
                    WPARAM(0),
                    LPARAM(empty.as_ptr() as isize),
                    SMTO_ABORTIFHUNG,
                    1_000,
                    None,
                )
            };
        }

        // SAFETY: hwnd は有効。PostMessage は相手スレッドを待たない。
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };

        // 閉じるのを少し待つ。
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            // SAFETY: 破棄済みの HWND を渡しても FALSE が返るだけ。
            if !unsafe { IsWindow(Some(HWND(hwnd as *mut _))) }.as_bool() {
                break;
            }
        }

        // SAFETY: 破棄済みなら FALSE。
        if unsafe { IsWindow(Some(HWND(hwnd as *mut _))) }.as_bool() {
            println!(
                "メモ帳のウィンドウが閉じませんでした (保存確認が出ている可能性)。\n\
                 自分が起動したプロセスだけを回収します"
            );
        }
        // 自分が spawn した子だけを終了させる。他プロセスには触らない。
        reap(child);
    }

    /// 文書に**一切触れずに**メモ帳を閉じる。
    ///
    /// セッション復元でユーザーの文書が開いていた場合に使う。
    /// `WM_SETTEXT("")` は禁止 — 復元された内容ごと消してしまう
    /// (実測 2026-08-17 の事故経路)。未変更のまま閉じれば
    /// セッション復元の内容は保全される。
    #[cfg(test)]
    fn close_without_clearing(hwnd: isize, child: &mut std::process::Child) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_CLOSE};

        // SAFETY: hwnd は有効。PostMessage は相手スレッドを待たない。
        let _ = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            // SAFETY: 破棄済みの HWND を渡しても FALSE が返るだけ。
            if !unsafe { IsWindow(Some(HWND(hwnd as *mut _))) }.as_bool() {
                break;
            }
        }
        // spawn ハンドルの回収のみ (プロセスが引き継がれていれば既に終了している)。
        // 未変更のウィンドウに保存確認は出ないため、ここでの kill は
        // 自分の子プロセスにしか影響しない。
        reap(child);
    }

    /// メモ帳の編集コントロールを探す。
    ///
    /// # 直接の子ではなく全子孫を探す理由 (実測で見つからなかった)
    ///
    /// `FindWindowExW` は**直下の子ウィンドウしか**列挙しない。Windows 11 の
    /// メモ帳は編集コントロール (`RichEditD2DPT`) がコンテナ
    /// (`NotepadTextBox` 等) の下の孫階層にあり、トップレベル直下を探すと
    /// 見つからない (実測 2026-08-17)。`EnumChildWindows` は子孫全体を
    /// 列挙するのでこちらを使う。
    #[cfg(test)]
    fn notepad_edit_control(parent: isize) -> Option<HWND> {
        use windows::core::BOOL;
        use windows::Win32::Foundation::LPARAM;
        use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};

        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // SAFETY: lparam は呼び出し元スタック上の Option<HWND> を指す。
            let found = unsafe { &mut *(lparam.0 as *mut Option<HWND>) };
            let mut buf = [0u16; 64];
            // SAFETY: hwnd は列挙中の有効な HWND、buf は書き込み可能。
            let len = unsafe { GetClassNameW(hwnd, &mut buf) };
            if len > 0 {
                let class = String::from_utf16_lossy(&buf[..len as usize]);
                if matches!(
                    class.as_str(),
                    "Edit" | "RichEditD2DPT" | "RICHEDIT50W" | "RichEdit20W"
                ) {
                    *found = Some(hwnd);
                    return BOOL(0); // 発見したら列挙を止める
                }
            }
            BOOL(1)
        }

        let mut found: Option<HWND> = None;
        // SAFETY: parent は有効な HWND。found は列挙の間だけ生きるスタック変数で、
        // EnumChildWindows は同期的に戻るため参照は列挙終了まで有効。
        unsafe {
            let _ = EnumChildWindows(
                Some(HWND(parent as *mut _)),
                Some(enum_proc),
                LPARAM(&mut found as *mut _ as isize),
            );
        }
        found
    }

    /// 対象ウィンドウを前景にする。取れたら `true`。
    ///
    /// `SetForegroundWindow` の単発呼び出しでは足りない (フォアグラウンドロック)。
    /// 相手スレッドへ `AttachThreadInput` してから要求し、終わったら必ず外す。
    #[cfg(test)]
    fn take_foreground(hwnd: isize) -> bool {
        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowThreadProcessId, SetForegroundWindow,
        };

        let target = HWND(hwnd as *mut _);
        // SAFETY: hwnd は有効。PID は不要なので None。
        let target_thread = unsafe { GetWindowThreadProcessId(target, None) };
        // SAFETY: 引数なし。
        let this_thread = unsafe { GetCurrentThreadId() };

        for _ in 0..10 {
            // SAFETY: 入力キューを繋いでから前景要求を出し、必ず外す。
            unsafe {
                let attached = target_thread != this_thread
                    && AttachThreadInput(this_thread, target_thread, true).as_bool();
                let _ = SetForegroundWindow(target);
                if attached {
                    let _ = AttachThreadInput(this_thread, target_thread, false);
                }
            }
            std::thread::sleep(Duration::from_millis(150));
            if current_foreground() == hwnd {
                return true;
            }
        }
        false
    }

    /// メモ帳の編集コントロールから本文を読む。
    ///
    /// クラス名は Windows のバージョンで変わる (従来は `Edit`、
    /// Windows 11 の新メモ帳は RichEdit 系) ので候補を順に試す。
    ///
    /// # `SendMessageW` を使わない理由 (実測でハングした)
    ///
    /// `SendMessageW` は**受け手スレッドがそのメッセージを処理し終えるまで
    /// 戻らない**。相手が別プロセスで、UI スレッドがポンプしていなかったり
    /// モーダルを出していると、呼び出し側が無期限にブロックする。
    /// 実際にこのテストで固まった (5 分経っても戻らず、メモ帳もテストプロセスも
    /// 生き残った)。他プロセスのウィンドウへ問い合わせるときは必ず
    /// `SendMessageTimeoutW` + `SMTO_ABORTIFHUNG` を使うこと。
    #[cfg(test)]
    fn read_notepad_text(parent: isize) -> Option<String> {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_GETTEXT, WM_GETTEXTLENGTH,
        };

        const TIMEOUT_MS: u32 = 2_000;

        {
            let child = notepad_edit_control(parent)?;

            // SendMessageTimeoutW の戻り値は成功フラグであって、メッセージの
            // 結果 (テキスト長・コピー文字数) は最終引数 lpdwResult で受ける。
            // 戻り値を長さとして使うと常に 1 前後の値になる (実測 2026-08-17)。
            let mut len: usize = 0;
            // SAFETY: child は有効な HWND。ハングしたら諦める。
            let ok = unsafe {
                SendMessageTimeoutW(
                    child,
                    WM_GETTEXTLENGTH,
                    WPARAM(0),
                    LPARAM(0),
                    SMTO_ABORTIFHUNG,
                    TIMEOUT_MS,
                    Some(&mut len),
                )
            }
            .0;
            if ok == 0 || len == 0 {
                return None;
            }

            let mut buf = vec![0u16; len + 1];
            let mut copied: usize = 0;
            // SAFETY: buf は len+1 要素あり、WPARAM にその長さを渡す。
            let ok = unsafe {
                SendMessageTimeoutW(
                    child,
                    WM_GETTEXT,
                    WPARAM(buf.len()),
                    LPARAM(buf.as_mut_ptr() as isize),
                    SMTO_ABORTIFHUNG,
                    TIMEOUT_MS,
                    Some(&mut copied),
                )
            }
            .0;
            if ok != 0 && copied > 0 && copied <= len {
                return Some(String::from_utf16_lossy(&buf[..copied]));
            }
        }
        None
    }

    /// 子プロセスを確実に終了させて回収する。
    ///
    /// `kill()` だけではゾンビが残る。早期 return する経路でも必ず通すこと。
    #[cfg(test)]
    fn reap(child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// 空でないテキストなら、フォーカス不一致で中止しても
    /// クリップボードには整形テキストが残る (R4)。
    #[test]
    #[ignore = "実クリップボードを使う。--test-threads=1 で実行すること"]
    fn clipboard_retains_text_when_focus_check_aborts() {
        let original = {
            let _guard = ClipboardGuard::open().expect("開ける");
            // SAFETY: クリップボードは開いている。
            unsafe { read_unicode_text() }
        };

        // 実在しない HWND を渡してフォーカス照合を必ず失敗させる。
        let report = inject(
            "中止されるテキスト",
            InjectTarget::new(0x7FFF_FFFF, 0),
            ClipboardPolicy::Restore {
                delay: Duration::ZERO,
            },
        );
        assert_eq!(report.outcome, InjectOutcome::AbortedFocusChanged);
        assert!(!report.injected);
        assert_eq!(
            report.clipboard_state,
            ClipboardState::HoldsInjectedText,
            "テキストが残る扱いになっていない"
        );

        let _guard = ClipboardGuard::open().expect("開ける");
        // SAFETY: クリップボードは開いている。
        let now = unsafe { read_unicode_text() };
        assert_eq!(
            now.as_deref(),
            Some("中止されるテキスト"),
            "中止時に整形テキストが残っていない (手で貼り付けられない)"
        );
        drop(_guard);

        if let Some(text) = original {
            restore_text(&text).expect("復元できる");
        }
    }
}
