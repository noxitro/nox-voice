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
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_LCONTROL, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    HC_ACTION, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// 短押し判定のしきい値。これ未満で離すとトグルモードに入る。
pub const TAP_THRESHOLD: Duration = Duration::from_millis(300);

/// 組み合わせホットキーに含められる修飾キーの最大数。
///
/// 3 つもあれば実用上足りる (Ctrl+Shift+Space など)。捕獲 UI の確定条件とも揃える。
pub const MAX_HOTKEY_MODS: usize = 3;

/// 既定のホットキートリガー: Space (左 Ctrl との組み合わせ)。
pub const DEFAULT_HOTKEY_VK: u32 = VK_SPACE.0 as u32;

/// 既定のホットキー修飾子: 左 Ctrl。
pub const DEFAULT_HOTKEY_MODS: [u32; 1] = [VK_LCONTROL.0 as u32];

/// 捕獲モードの取り消しに使うキー (Esc)。
///
/// ホットキーには選べない ([`is_allowed_hotkey`] が弾く) が、
/// 捕獲の中止専用としては「取り消し」の意味がそのまま生きる。
pub const ESCAPE_VK: u32 = 0x1B;

/// ホットキーの組み合わせ: 修飾キー 0〜[`MAX_HOTKEY_MODS`] 個 + トリガーキー 1 個。
///
/// 判定は「トリガーが押された瞬間、修飾キーがすべて下がっているか」で行う
/// ([`handle_key_event`] 参照)。修飾キーだけ離してトリガーを保持し続けた場合は
/// 録音が継続し、トリガーを離した時点で終わる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyCombo {
    /// 修飾キー (正規化済み: 重複なし・トリガー自身を含まない)。未使用スロットは 0。
    pub mods: [u32; MAX_HOTKEY_MODS],
    /// トリガーキー。押下の瞬間をフックが観測するキー。
    pub vk: u32,
}

impl Default for HotkeyCombo {
    fn default() -> Self {
        let mut mods = [0u32; MAX_HOTKEY_MODS];
        for (slot, vk) in mods.iter_mut().zip(DEFAULT_HOTKEY_MODS.iter()) {
            *slot = *vk;
        }
        Self {
            mods,
            vk: DEFAULT_HOTKEY_VK,
        }
    }
}

impl HotkeyCombo {
    /// 部品から組み立てる。無効な組み合わせなら `None` ([`sanitize_combo`])。
    pub fn from_parts(mods: &[u32], vk: u32) -> Option<Self> {
        sanitize_combo(mods, vk)
    }

    /// 使用中の修飾キーの数。
    ///
    /// **フックコールバックから呼ぶのはこちら**。[`Self::mods_vec`] は
    /// `Vec` を確保するので、コールバック内では使えない
    /// (モジュール冒頭の不変条件: 確保・ロック・IO 禁止)。
    fn mods_count(&self) -> usize {
        self.mods.iter().filter(|vk| **vk != 0).count()
    }

    /// 使用中の修飾キーだけを列挙する。
    pub fn mods_vec(&self) -> Vec<u32> {
        self.mods.iter().copied().filter(|vk| *vk != 0).collect()
    }

    /// 表示用の全キー (修飾キー → トリガーの順)。
    fn all_keys(&self) -> Vec<u32> {
        let mut keys = self.mods_vec();
        keys.push(self.vk);
        keys
    }

    /// 人間が読める名前 (「左 Ctrl + Space」など)。
    pub fn label(&self) -> String {
        describe_keys(&self.all_keys())
    }

    /// `vk` がこの組み合わせの何番目の修飾キーか (ビット位置)。違えば `None`。
    fn mod_bit(&self, vk: u32) -> Option<u32> {
        self.mods
            .iter()
            .position(|m| *m != 0 && *m == vk)
            .map(|i| i as u32)
    }

    /// `mask` ([`MODS_DOWN`]) が「すべての修飾キーが下がっている」状態か。
    fn mods_are_down(&self, mask: u32) -> bool {
        self.mods
            .iter()
            .enumerate()
            .all(|(i, m)| *m == 0 || mask & (1 << i) != 0)
    }
}

/// [`HotkeyCombo`] を 1 本の `u64` に詰める (16 ビット × 4 スロット)。
///
/// フックコールバックは確保・ロック禁止なので、比較対象は atomic 1 本で
/// 差し替えられるようにする。VK は 0..=0xFF なので 16 ビットで十分。
const fn pack_combo(combo: &HotkeyCombo) -> u64 {
    (combo.mods[0] as u64)
        | ((combo.mods[1] as u64) << 16)
        | ((combo.mods[2] as u64) << 32)
        | ((combo.vk as u64) << 48)
}

/// [`pack_combo`] の逆。
fn unpack_combo(raw: u64) -> HotkeyCombo {
    HotkeyCombo {
        mods: [
            (raw & 0xFFFF) as u32,
            ((raw >> 16) & 0xFFFF) as u32,
            ((raw >> 32) & 0xFFFF) as u32,
        ],
        vk: (raw >> 48) as u32,
    }
}

/// 既定の組み合わせを 1 本の `u64` へ詰めた値 (static 初期化用の定数式)。
const PACKED_DEFAULT_HOTKEY: u64 =
    (DEFAULT_HOTKEY_MODS[0] as u64) | ((DEFAULT_HOTKEY_VK as u64) << 48);

/// ホットキーの用途。用途ごとに別の組み合わせを割り当てられる。
///
/// # なぜ用途を分けるのか
///
/// 貼り付けは「録音開始時の前景ウィンドウ」を掴んでいないと成立しない
/// ([`crate::inject`] の R7 照合)。フォアグラウンドが無い状態 —
/// デスクトップだけが見えている、フォーカスが入力欄に無い、
/// 別の作業をしながらペダルを踏む — では、貼り付けは必ず中止になる。
/// その状況で欲しいのは「**黙ってクリップボードに入れておく**」であって、
/// 中止の通知ではない。用途ごとにキーを分ければ、ユーザーは踏む足で
/// 「今回は貼る / 今回は溜める」を選べる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyMode {
    /// 通常: 転写結果を前景アプリへ貼り付ける。
    Inject,
    /// クリップボードへ入れるだけ (貼り付けない)。
    ///
    /// 前景ウィンドウの有無を問わない。フォーカスが無くても成立する。
    ClipboardOnly,
    /// 画面質問モード: 発話を「画面への質問」として扱い、答えをコピーする。
    ///
    /// 転写結果を整形して届ける他の 2 つと違い、**発話は届けるものではなく
    /// 問いである**。届くのは画面を読んだうえでの回答で、出力先は
    /// クリップボード ([`crate::inject::copy_only`])。
    ///
    /// **既定で未設定**。これは `ClipboardOnly` と同じ理由 (勝手にキーを
    /// 占有しない) に加えて、**モニタに写っているものを全部クラウドへ送る**
    /// 機能だから — 誤爆したときの被害が他の 2 つとは桁違いになる。
    /// 設定で有効化し、なおかつ専用キーを割り当てて初めて動く。
    ScreenAsk,
}

/// ホットキーのスロット数 ([`HotkeyMode`] の数)。
///
/// 増やすときは、スロットごとの配列 ([`HOTKEY_COMBOS`] / [`KEY_IS_DOWN`] /
/// [`MODS_DOWN`]) と [`HotkeyMode::slot`] / [`HotkeyMode::from_slot`] が
/// 揃っていることを確かめること。**フックコールバックの不変条件
/// (確保・ロック・IO・panic 禁止) はスロット数によらず守る** —
/// [`handle_key_event`] の優先順の並べ替えが `Vec` を作らず固定長配列と
/// [`HotkeyCombo::mods_count`] で済ませているのはそのため。
pub const HOTKEY_SLOTS: usize = 3;

impl HotkeyMode {
    /// 静的配列の添字。
    pub const fn slot(self) -> usize {
        match self {
            HotkeyMode::Inject => 0,
            HotkeyMode::ClipboardOnly => 1,
            HotkeyMode::ScreenAsk => 2,
        }
    }

    /// 添字から戻す (範囲外は `Inject`)。
    pub const fn from_slot(slot: usize) -> Self {
        match slot {
            1 => HotkeyMode::ClipboardOnly,
            2 => HotkeyMode::ScreenAsk,
            _ => HotkeyMode::Inject,
        }
    }

    /// ログ・UI に出す日本語名。
    pub fn label(self) -> &'static str {
        match self {
            HotkeyMode::Inject => "貼り付け",
            HotkeyMode::ClipboardOnly => "クリップボードのみ",
            HotkeyMode::ScreenAsk => "画面に質問",
        }
    }
}

/// 現在のホットキー (用途ごと)。設定から差し替えられる。**0 は未設定**。
///
/// フックは 1 度しか設置しない (再設置は OS 全体の入力経路を触り直すことになる)。
/// 比較する組み合わせだけを atomic で差し替えれば、変更は次のキー入力から効く。
static HOTKEY_COMBOS: [AtomicU64; HOTKEY_SLOTS] = [
    AtomicU64::new(PACKED_DEFAULT_HOTKEY),
    // クリップボードのみは既定で未設定。勝手にキーを 1 つ占有すると、
    // その組み合わせを使っている他アプリの操作を黙って奪う。
    AtomicU64::new(0),
    // 画面に質問も既定で未設定。こちらは加えてプライバシー上の理由がある
    // ([`HotkeyMode::ScreenAsk`] の doc)。
    AtomicU64::new(0),
];

/// キー捕獲モード。設定 UI の「キーを押して設定」で使う。
///
/// # このフラグは今や「PTT の消音」だけを意味する (2026-08-28)
///
/// 以前はフックが捕獲モード中の押下を [`HotkeyEventKind`] として報告し、
/// それが捕獲 UI の入力源だった。**その経路は廃止した**。
/// `WH_KEYBOARD_LL` は応答が遅れると OS に無言で外され
/// ([`spawn_hook_watchdog`] の doc)、外れている間は捕獲だけが沈黙する。
/// 捕獲は「設定ウィンドウにフォーカスがある」場面の操作なので、
/// グローバルフックを使う必然性が無い。今は**フロントの DOM の
/// `keydown` / `keyup`** が入力源で、フックの生死と完全に無関係になった
/// (`docs/design.md`「キー捕獲を DOM イベントへ移す」)。
///
/// それでもこのフラグが要るのは**安全側の理由**: 捕獲中は現行ホットキーの
/// キー (既定なら 左Ctrl+Space) が押される。フックがそれを PTT として
/// 解釈すると、**キーを設定しようとしただけで録音が始まる**。
/// ON の間、フックは PTT・キャンセルの解釈を一切行わずに捨てる。
static CAPTURE_MODE: AtomicBool = AtomicBool::new(false);

/// 捕獲セッションの世代。タイムアウトが**古い**捕獲を打ち切らないようにする。
static CAPTURE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 「離されるまで無視する」キーのスロット (0 = 空)。
///
/// 捕獲を確定した瞬間、そのキーはまだ物理的に押されたままである。
/// 捕獲モードを抜けた直後にオートリピートの keydown が通常経路へ流れると、
/// **設定しただけで録音が始まり、短押し判定でトグルにラッチする**。
/// 組み合わせ確定では複数のキーが同時に押されたままになるためスロット化する。
/// スロット数は「修飾キー最大数 + トリガー」をカバーすれば足りる。
const SUPPRESS_SLOTS: usize = MAX_HOTKEY_MODS + 1;
static SUPPRESS_UNTIL_RELEASE: [AtomicU32; SUPPRESS_SLOTS] =
    [const { AtomicU32::new(0) }; SUPPRESS_SLOTS];

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

/// 用途ごとのホットキーを差し替える。`None` で無効化。フックの再設置は不要。
///
/// 差し替えの際に押下状態 (トリガー・修飾キーとも) を畳む。押しっぱなしの
/// 状態が残っていると、次の離しだけが届いて状態がねじれる。
pub fn set_mode_hotkey(mode: HotkeyMode, combo: Option<HotkeyCombo>) {
    let slot = mode.slot();
    HOTKEY_COMBOS[slot].store(combo.as_ref().map_or(0, pack_combo), Ordering::SeqCst);
    KEY_IS_DOWN[slot].store(false, Ordering::SeqCst);
    MODS_DOWN[slot].store(0, Ordering::SeqCst);
    match combo {
        Some(combo) => log::info!(
            "ホットキーを変更 [{}]: {} ({combo:?})",
            mode.label(),
            combo.label()
        ),
        None => log::info!("ホットキーを無効化 [{}]", mode.label()),
    }
}

/// 現在設定されている組み合わせ (未設定なら `None`)。
pub fn mode_hotkey(mode: HotkeyMode) -> Option<HotkeyCombo> {
    let raw = HOTKEY_COMBOS[mode.slot()].load(Ordering::SeqCst);
    (raw != 0).then(|| unpack_combo(raw))
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
    clear_all_key_down();
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

/// 指定キー群のうち**まだ物理的に押されているものだけ**を
/// 「離されるまで通常経路で無視する」状態にする。
///
/// # なぜ「押されているものだけ」なのか
///
/// 抑制は離した瞬間の keyup ([`clear_suppression`]) でしか解除されない。
/// **既に離されているキーを登録すると、解除する keyup が二度と来ない** —
/// つまりそのキーは恒久的に無視され、設定したばかりのホットキーが
/// アプリを再起動するまで無反応になる。
///
/// 捕獲は「押したキーをすべて離した瞬間」に確定する仕様なので、確定時点では
/// 通常どのキーも押されていない。それでも呼び出し側から渡された全キーを
/// 素通しで登録していたため、**捕獲 UI でホットキーを設定した直後から
/// そのホットキーが効かない**という形で表面化していた (E2E T4 で再現)。
///
/// 押下の判定はフックが持つ状態ではなく OS に聞く。捕獲経路のキーは
/// PTT 経路の押下追跡 ([`KEY_IS_DOWN`] / [`MODS_DOWN`]) を通っていないため、
/// フック側の状態は「押されているか」の答えを持っていない。
/// この関数はコントローラスレッドから呼ばれるので、フックの
/// 「確保・ロック・IO 禁止」制約はかからない。
///
/// 空きスロットを超えた分は無視される ([`SUPPRESS_SLOTS`] ≥ 組み合わせの最大長)。
pub fn suppress_until_release_keys(keys: &[u32]) {
    clear_all_key_down();
    let still_down: Vec<u32> = keys.iter().copied().filter(|vk| is_physically_down(*vk)).collect();
    if still_down.len() != keys.len() {
        log::info!(
            "捕獲確定: 押されたままのキーだけを抑制する ({:?} / 観測した全キー {:?})",
            still_down,
            keys
        );
    }
    for (slot, vk) in SUPPRESS_UNTIL_RELEASE.iter().zip(still_down.iter()) {
        slot.store(*vk, Ordering::SeqCst);
    }
}

/// `vk` が今この瞬間押されているか (OS に聞く)。
///
/// `GetAsyncKeyState` の最上位ビットが「今押されている」。
/// 下位ビットの「前回呼び出し以降に押された」は使わない。
///
/// テストでは実キーを押させられないので、`cfg(test)` では
/// [`tests::hold_keys`] が置いた「押されていることにするキー」を答える。
/// この分岐を入れずに OS へ問い合わせると、抑制の**解除**側の振る舞い
/// (どのキーの keyup でどのスロットが空くか) が一切テストできなくなる。
fn is_physically_down(vk: u32) -> bool {
    if vk == 0 || vk > 0xFF {
        return false;
    }
    #[cfg(test)]
    {
        tests::is_held_for_test(vk)
    }
    #[cfg(not(test))]
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        // SAFETY: 引数は仮想キーコードの範囲内。副作用は下位ビットのクリアのみ。
        let state = unsafe { GetAsyncKeyState(vk as i32) };
        (state as u16) & 0x8000 != 0
    }
}

/// `vk` が今「離されるまで無視」の対象か。
fn is_suppressed(vk: u32) -> bool {
    vk != 0
        && SUPPRESS_UNTIL_RELEASE
            .iter()
            .any(|slot| slot.load(Ordering::SeqCst) == vk)
}

/// 離されたキーの抑制を解除する。
fn clear_suppression(vk: u32) {
    for slot in SUPPRESS_UNTIL_RELEASE.iter() {
        if slot.load(Ordering::SeqCst) == vk {
            slot.store(0, Ordering::SeqCst);
        }
    }
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
        // 以下は「一覧から選ぶ」UI ([`hotkey_key_catalog`]) に**並べる**ために
        // 名前が要る。選べないキーも理由つきで一覧へ出す方針なので、名前が無いと
        // 「VK 0x2C はホットキーに使えません」という、どのキーの話か
        // 分からない文言が画面に出てしまう。
        0x5D => "アプリケーション",
        0x2C => "PrintScreen",
        0x90 => "NumLock",
        0x15 => "かな",
        0x25 => "←",
        0x26 => "↑",
        0x27 => "→",
        0x28 => "↓",
        // F1〜F24。**F13 以降まで名前を持つ**必要がある: `is_allowed_hotkey` は
        // 昔から F24 まで許しているのに、ここは F12 までしか名前を知らず、
        // F13 を選ぶと「VK 0x7C」と表示されていた。DOM 捕獲では F13〜F24 が
        // 物理キーの無い環境でも押せる (E2E も F13〜F15 を使う) ので、
        // 名前が無いままだと実際に目に見える。
        0x70..=0x87 => return format!("F{}", vk - 0x6F),
        0x30..=0x39 => return format!("{}", vk - 0x30),
        0x41..=0x5A => return char::from(vk as u8).to_string(),
        0x60..=0x69 => return format!("テンキー {}", vk - 0x60),
        _ => return format!("VK 0x{vk:02X}"),
    };
    name.to_string()
}

/// キー集合を人間が読める名前へ (「左 Ctrl + Space」など)。
///
/// 修飾キーを先に、トリガーを後に並べる慣習に合わせて並べ替える。
/// 捕獲中の進行表示でも使う (この時点ではトリガーは未確定)。
pub fn describe_keys(keys: &[u32]) -> String {
    let mut sorted: Vec<u32> = keys.to_vec();
    sorted.sort_by_key(|vk| (modifier_order(*vk), *vk));
    sorted
        .iter()
        .map(|vk| key_label(*vk))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// 修飾キーの表示順 (Ctrl → Alt → Shift → Win)。それ以外は最後。
fn modifier_order(vk: u32) -> u8 {
    match vk {
        0xA2 | 0xA3 => 0,
        0xA4 | 0xA5 => 1,
        0xA0 | 0xA1 => 2,
        0x5B | 0x5C => 3,
        _ => u8::MAX,
    }
}

/// `vk` が修飾キー (左右個別の Ctrl / Shift / Alt / Win) か。
pub fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0xA0..=0xA5 | 0x5B | 0x5C)
}

/// DOM の `KeyboardEvent.code` を Windows の仮想キーコードへ写す。
///
/// # なぜフロントで VK へ直さないのか
///
/// 許可判定 ([`is_allowed_hotkey`] / [`is_allowed_combo_key`])・正規化
/// ([`sanitize_combo`])・表示名 ([`key_label`]) はすべて VK を入り口にして
/// ここに揃っている。フロントでも VK を組み立てると同じ表が 2 か所に増え、
/// **必ず片方だけ更新されてずれる**。フロントは `code` の列を渡すだけにして、
/// 写像はこの純関数 1 か所に閉じる。
///
/// # なぜ `code` であって `key` ではないか
///
/// `KeyboardEvent.key` は「そのキーが今の配列で何を打つか」でレイアウトと
/// 修飾キーの状態に左右される (Shift+`2` が `"@"` になる、Ctrl 単独が
/// `"Control"` になり左右が区別できない、など)。ホットキーが要るのは
/// **物理キーの位置**なので `code` を使う。`code` は左右も区別する
/// (`ControlLeft` / `ControlRight`)。
///
/// 写せない `code` は `None`。呼び出し側は黙って捨てず、
/// 「そのキーは設定に使えない」とユーザーへ言うこと。
pub fn code_to_vk(code: &str) -> Option<u32> {
    // 修飾キー・ロック系・IME・機能キー。ホットキーに選べる本命。
    let fixed = match code {
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "AltLeft" => 0xA4,
        // 日本語配列の右 Alt は AltGr ではなく素の右 Alt。ブラウザは
        // どちらも `AltRight` で報告するので、VK も右 Alt に写す。
        "AltRight" => 0xA5,
        "MetaLeft" | "OSLeft" => 0x5B,
        "MetaRight" | "OSRight" => 0x5C,
        "ContextMenu" => 0x5D,
        "CapsLock" => 0x14,
        "ScrollLock" => 0x91,
        "NumLock" => 0x90,
        "Pause" => 0x13,
        // JIS 配列の IME キー。Chromium は 変換 = Convert、無変換 = NonConvert、
        // かな = KanaMode で報告する (かなは VK が実キーと食い違うため
        // `is_allowed_hotkey` 側で弾かれる — ここでは写すだけ)。
        "Convert" => 0x1C,
        "NonConvert" => 0x1D,
        "KanaMode" => 0x15,
        // 以下は許可判定で弾かれるが、**弾く理由を名前で言う**ために写す。
        // 写せないと「そのキーは認識できません」になり、
        // 「Enter は押しっぱなしで実害が出るから使えません」と言えなくなる。
        "Space" => 0x20,
        "Enter" | "NumpadEnter" => 0x0D,
        "Escape" => ESCAPE_VK,
        "Tab" => 0x09,
        "Backspace" => 0x08,
        "Insert" => 0x2D,
        "Delete" => 0x2E,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "PrintScreen" => 0x2C,
        "NumpadMultiply" => 0x6A,
        "NumpadAdd" => 0x6B,
        "NumpadSubtract" => 0x6D,
        "NumpadDecimal" => 0x6E,
        "NumpadDivide" => 0x6F,
        _ => 0,
    };
    if fixed != 0 {
        return Some(fixed);
    }

    // 連番になっているものは範囲で写す。表に 60 行並べても間違いが増えるだけ。
    if let Some(rest) = code.strip_prefix("Key") {
        // KeyA..KeyZ。`code` の仕様上ここは必ず ASCII 大文字 1 文字。
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_uppercase() {
                return Some(c as u32);
            }
        }
        return None;
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        // Digit0..Digit9 (数字列。テンキーは Numpad*)。
        if let Ok(n) = rest.parse::<u32>() {
            if n <= 9 && rest.len() == 1 {
                return Some(0x30 + n);
            }
        }
        return None;
    }
    if let Some(rest) = code.strip_prefix("Numpad") {
        if let Ok(n) = rest.parse::<u32>() {
            if n <= 9 && rest.len() == 1 {
                return Some(0x60 + n);
            }
        }
        return None;
    }
    if let Some(rest) = code.strip_prefix('F') {
        // F1..F24。**F13〜F24 は物理キーボードに無くても DOM に届く**ので、
        // 「他アプリと衝突しないキー」を選ぶ手段としてそのまま使える
        // (E2E も F13〜F15 を使っている)。
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) && !rest.starts_with('0') {
                return Some(0x6F + n);
            }
        }
        return None;
    }
    None
}

/// `code` の列を VK の列へ。1 つでも写せなければ、その `code` を返す。
///
/// 「1 つでも」で止めるのは、写せなかったキーを黙って落とすと
/// **ユーザーが押したのと違う組み合わせが保存される**ため。
/// 例: 「Win + F13」のつもりで押した結果が「F13」単独になる。
pub fn codes_to_vks(codes: &[String]) -> Result<Vec<u32>, String> {
    let mut out = Vec::with_capacity(codes.len());
    for code in codes {
        match code_to_vk(code) {
            Some(vk) => out.push(vk),
            None => return Err(code.clone()),
        }
    }
    Ok(out)
}

/// 組み合わせの構成キーに選んでよいか ([`is_allowed_hotkey`] の緩和版)。
///
/// 単独指定より緩いのは、「修飾キーと一緒に押されている間だけ」流れる
/// キーだから。Ctrl + Space は挿入先で意味を持つことが少なく、
/// 押しっぱなしの実害が単独の Space よりずっと小さい。
/// 一方で Ctrl+Enter (送信) や Ctrl+S (保存の連打) になりうる文字キー・
/// Enter・Tab・Backspace・ナビゲーションキーは引き続き拒否する。
pub fn is_allowed_combo_key(vk: u32) -> bool {
    is_allowed_hotkey(vk)
        // Space (既定のトリガー)
        || vk == 0x20
        // テンキー (Ctrl+テンキーは標準バインドがほぼ無い)
        || matches!(vk, 0x60..=0x69)
}

/// 設定値を正規化された [`HotkeyCombo`] へ組み立てる。
///
/// - 修飾子リストから非修飾キー・重複・トリガー自身を除去し、表示順に並べる
/// - トリガーの可否は「単独か組み合わせか」で基準が変わる
///   (単独なら [`is_allowed_hotkey`]、組み合わせなら [`is_allowed_combo_key`])
///
/// 無効なら `None`。呼び出し側 (設定の normalize) は既定値へフォールバックする。
pub fn sanitize_combo(mods: &[u32], vk: u32) -> Option<HotkeyCombo> {
    if vk == 0 || vk > 0xFF {
        return None;
    }
    let mut cleaned: Vec<u32> = mods
        .iter()
        .copied()
        .filter(|m| is_modifier(*m) && *m != vk)
        .collect();
    cleaned.sort_by_key(|m| (modifier_order(*m), *m));
    cleaned.dedup();
    cleaned.truncate(MAX_HOTKEY_MODS);

    let trigger_is_valid = if cleaned.is_empty() {
        // 単独キーは従来基準 (押しっぱなしで実害が出ないキーのみ)。
        is_allowed_hotkey(vk)
    } else {
        is_allowed_combo_key(vk)
    };
    if !trigger_is_valid {
        return None;
    }

    let mut arr = [0u32; MAX_HOTKEY_MODS];
    arr[..cleaned.len()].copy_from_slice(&cleaned);
    Some(HotkeyCombo { mods: arr, vk })
}

// --- 一覧から選ぶ (2026-08-31) ----------------------------------------------
//
// # なぜ捕獲だけでは足りないのか
//
// 捕獲 UI (「キーを押して設定」) は**押せるキーしか設定できない**。
// Stream Deck のペダルに割り当てた F13 / F14 は物理キーボードに存在せず、
// DOM の `keydown` が来る道が無い。実際に呼び出し元は config.json を
// 直接書いて回避していた。設定ファイルを手で書かせるのは UI の敗北なので、
// 「押さずに選ぶ」経路を足す。
//
// # なぜ一覧を Rust で組み立てるのか
//
// フロントにキー名の表を持たせると、[`key_label`] / [`is_allowed_hotkey`] /
// [`is_allowed_combo_key`] を直した日に片方だけが古いままになり、
// **一覧には出るのに確定できないキー**(あるいは逆)が生まれる。
// [`code_to_vk`] の doc と同じ理由で、表はこの 1 か所に閉じる。

/// 一覧のカテゴリ。フロントは並び順もこの配列のとおりに描く。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HotkeyKeyCategory {
    /// 安定した id ([`HotkeyKeyOption::category`] と対応)。
    pub id: &'static str,
    /// 見出しに出す名前。
    pub label: &'static str,
    /// そのカテゴリを開いた人への一言 (空なら出さない)。
    pub hint: &'static str,
}

/// 一覧に出す 1 キー分。
///
/// **「選べるか」を 2 つの真偽値で持つ**のが要点。修飾キーを 1 つも選んで
/// いない状態と、1 つ以上選んだ状態とで基準が変わる ([`sanitize_combo`]) ので、
/// フロントは「今チェックされている修飾キーの数」だけを見てどちらかを読めば、
/// 判定を再実装せずに**選べる/選べないの遷移**を描ける。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HotkeyKeyOption {
    pub vk: u32,
    /// 表示名 ([`key_label`])。捕獲で設定したときのラベルと同じ綴りになる。
    pub label: String,
    /// 所属カテゴリ ([`HotkeyKeyCategory::id`])。
    pub category: &'static str,
    /// 修飾キー無しの単独トリガーとして選べるか ([`is_allowed_hotkey`])。
    pub alone: bool,
    /// 修飾キーを 1 つ以上付ければ選べるか ([`is_allowed_combo_key`])。
    pub with_mods: bool,
    /// 修飾キーそのものか ([`is_modifier`])。修飾キーの選択肢にも出す。
    pub is_modifier: bool,
    /// 選べないときの理由 (選べるなら空)。
    ///
    /// **灰色にするだけにしない**ための文言。「なぜ Enter が選べないのか」に
    /// 答えられないと、利用者は壊れていると解釈して押し続ける。
    pub note: String,
}

const CAT_MODIFIER: &str = "modifier";
const CAT_FUNCTION: &str = "function";
const CAT_NUMPAD: &str = "numpad";
const CAT_EDIT: &str = "edit";
const CAT_IME: &str = "ime";
const CAT_ALNUM: &str = "alnum";
const CAT_OTHER: &str = "other";

/// カテゴリの一覧と並び順。
///
/// 並びは**このアプリで実際に選ばれるものが先**。Stream Deck のメニューは
/// 英数文字が先頭に来るが、nox-voice では文字キーは 1 つも選べない
/// ([`is_allowed_combo_key`] が拒否する) ので先頭に置く意味が無い。
/// ペダル運用の本命である F キーを 2 番目に置き、**F13 に 3 クリック
/// (一覧を開く → F キー → F13) で届く**ようにする。
pub fn hotkey_key_categories() -> Vec<HotkeyKeyCategory> {
    vec![
        HotkeyKeyCategory {
            id: CAT_MODIFIER,
            label: "修飾キー (左右を区別)",
            hint: "単独でも、組み合わせの一部にも使えます",
        },
        HotkeyKeyCategory {
            id: CAT_FUNCTION,
            label: "F キー (F1〜F24)",
            hint: "F13〜F24 は物理キーボードに無い分、他アプリと衝突しません (ペダル向き)",
        },
        HotkeyKeyCategory {
            id: CAT_NUMPAD,
            label: "テンキー",
            hint: "修飾キーと組み合わせたときだけ選べます",
        },
        HotkeyKeyCategory {
            id: CAT_EDIT,
            label: "編集・移動キー",
            hint: "Space 以外は、押している間ずっと入力先へ流れるため選べません",
        },
        HotkeyKeyCategory {
            id: CAT_IME,
            label: "IME・ロック系",
            hint: "",
        },
        HotkeyKeyCategory {
            id: CAT_ALNUM,
            label: "英数字",
            hint: "文字キーは Ctrl+S のような既存の操作を奪うため、このアプリでは選べません",
        },
        HotkeyKeyCategory {
            id: CAT_OTHER,
            label: "その他",
            hint: "",
        },
    ]
}

/// 選べないキーに添える理由。
///
/// 既定の文言は [`is_allowed_hotkey`] の doc がそのまま根拠 (押しっぱなしで
/// 挿入先へ流れ続ける)。**それが理由ではないキーだけ**を個別に言う —
/// Esc に「入力先へ流れる」と書いても嘘になり、利用者は納得できない。
fn unusable_reason(vk: u32) -> String {
    match vk {
        ESCAPE_VK => "Esc は設定の取り消しと録音のキャンセルに使うため、ホットキーには選べません"
            .to_string(),
        // 押すたびに別の VK が来るキー。片方だけ登録すると 2 回に 1 回しか
        // 効かない ([`is_allowed_hotkey`] の doc)。
        0xF3 | 0xF4 | 0x15 => {
            "このキーは押すたびに別のキーコードを送るため、確実に発火しません".to_string()
        }
        // 文字を流すわけでも、押すたびに VK が変わるわけでもないキー
        // (PrintScreen・NumLock)。既定の文言を付けると**理由が嘘になる**ので、
        // 中立に「対象外」とだけ言う。分からないことを分かった風に書かない。
        0x2C | 0x90 => "このアプリの許可規則の対象外のキーです".to_string(),
        _ => "押している間ずっと入力先アプリへ流れ続けるため、ホットキーには選べません".to_string(),
    }
}

/// 1 キー分の選択肢を作る。判定は既存の許可規則からしか作らない。
fn key_option(vk: u32, category: &'static str) -> HotkeyKeyOption {
    let alone = is_allowed_hotkey(vk);
    let with_mods = is_allowed_combo_key(vk);
    HotkeyKeyOption {
        vk,
        label: key_label(vk),
        category,
        alone,
        with_mods,
        is_modifier: is_modifier(vk),
        note: if with_mods {
            if alone {
                String::new()
            } else {
                // 状態遷移を言葉で示す。灰色になっている理由と、
                // それを解く方法が同じ 1 文に入っている必要がある。
                "修飾キー (Ctrl / Alt / Shift / Win) を 1 つ以上選ぶと使えます".to_string()
            }
        } else {
            unusable_reason(vk)
        },
    }
}

/// 一覧に出すキーの全体。
///
/// **選べないキーもあえて載せる**。載せないと「Enter が無いのは不具合か、
/// 意図か」が分からず、探し続けることになる。載せたうえで
/// [`HotkeyKeyOption::note`] に理由を添える方が、探す時間も問い合わせも減る。
pub fn hotkey_key_catalog() -> Vec<HotkeyKeyOption> {
    let mut out = Vec::new();
    let mut push = |vk: u32, category: &'static str| out.push(key_option(vk, category));

    // 修飾キー。並びは表示順 ([`modifier_order`]) に合わせる — 一覧と
    // 確定後のラベル (「左 Ctrl + F13」) で Ctrl / Alt / Shift / Win の
    // 順序が食い違うと、同じものを見ている気がしなくなる。
    for vk in [0xA2, 0xA3, 0xA4, 0xA5, 0xA0, 0xA1, 0x5B, 0x5C] {
        push(vk, CAT_MODIFIER);
    }
    // F1〜F24。**この経路の存在理由**なので、範囲で機械的に出す。
    for vk in 0x70..=0x87 {
        push(vk, CAT_FUNCTION);
    }
    for vk in 0x60..=0x69 {
        push(vk, CAT_NUMPAD);
    }
    for vk in [
        0x09, 0x20, 0x0D, 0x08, 0x2D, 0x2E, 0x24, 0x23, 0x21, 0x22, 0x25, 0x26, 0x27, 0x28, 0x1B,
    ] {
        push(vk, CAT_EDIT);
    }
    for vk in [0x14, 0x91, 0x13, 0x1C, 0x1D, 0xF3, 0x15] {
        push(vk, CAT_IME);
    }
    for vk in 0x30..=0x39 {
        push(vk, CAT_ALNUM);
    }
    for vk in 0x41..=0x5A {
        push(vk, CAT_ALNUM);
    }
    // PrintScreen (0x2C) と NumLock (0x90) は**中立の理由**で載せる。
    // どちらも「押している間ずっと入力先へ流れる」は当てはまらないので、
    // [`unusable_reason`] 側で分けてある。理由を書き分けられる以上、
    // 片方だけ載せない扱いにする根拠は無い。
    for vk in [0x5D, 0x2C, 0x90] {
        push(vk, CAT_OTHER);
    }
    out
}

/// フックが観測した生のキーイベントの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEventKind {
    /// ホットキーが押された (オートリピートは除去済み)。`mode` は用途。
    Press { mode: HotkeyMode },
    /// ホットキーが離された。`mode` は押されたときと同じ用途。
    Release { mode: HotkeyMode },
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
static KEY_IS_DOWN: [AtomicBool; HOTKEY_SLOTS] = [const { AtomicBool::new(false) }; HOTKEY_SLOTS];

/// すべてのスロットのトリガー押下状態を畳む。
fn clear_all_key_down() {
    for slot in KEY_IS_DOWN.iter() {
        slot.store(false, Ordering::SeqCst);
    }
}
/// 設定済み修飾キーの押下状態 (ビット i = [`HotkeyCombo::mods`] の i 番目)。
///
/// フックはグローバルなので、すべての keydown / keyup をここで数えられる。
/// `GetAsyncKeyState` を使わないのは、**単体テストから制御・検証できるように**
/// するため (実キーを打たせられない CI でも振る舞いを固定できる)。
/// 押しっぱなしのまま UAC 画面などへ遷移して keyup を取りこぼしても、
/// 次にその修飾キーを 1 回押して離せば自己回復する。
static MODS_DOWN: [AtomicU32; HOTKEY_SLOTS] = [const { AtomicU32::new(0) }; HOTKEY_SLOTS];
/// フックスレッドの ID。停止要求 (`WM_QUIT`) の宛先。
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// チャネル満杯で捨てたイベント数。コントローラ側が観測してログに出す。
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

/// フックが観測したキーイベントの累計 (生存確認用)。
static HOOK_EVENTS_SEEN: AtomicU64 = AtomicU64::new(0);
/// 生存確認のダミーキーをフックが観測した回数。
static HEARTBEAT_SEEN: AtomicU64 = AtomicU64::new(0);
/// フックを再設置した回数 (診断用)。
static HOOK_REINSTALLS: AtomicU64 = AtomicU64::new(0);
/// フックが最後にキーイベントを観測した時刻 (`GetTickCount` のミリ秒。0 = 未観測)。
///
/// ページから来たキー ([`feed_window_key`]) を使うかどうかの判断に使う。
static LAST_HOOK_KEY_TICK: AtomicU32 = AtomicU32::new(0);
/// フックがこの時間内にキーを観測していれば、ページから来たキーは使わない
/// ([`feed_window_key`] の doc)。
///
/// ページのキーは同じ打鍵をフックより数 ms 遅れて (IPC を 1 往復して) 届く。
/// 人が打つ間隔よりは十分短く、IPC の遅れよりは十分長い値にする。
const WINDOW_KEY_HOOK_GRACE_MS: u32 = 250;

/// フックの生存確認を行う間隔。
///
/// 短くしすぎるとダミーキーを撒きすぎる。長すぎると「効かない時間」が伸びる。
/// 無操作のときしか送らないので、この程度なら実害はない。
const HOOK_HEALTH_INTERVAL: Duration = Duration::from_secs(15);
/// 生存確認のキーがフックへ届くのを待つ猶予。
const HEARTBEAT_GRACE: Duration = Duration::from_millis(500);
/// 生存確認に使う VK。`VK_NONAME` — Microsoft が「ダミーのキーストローク用」と
/// 明記している、どのアプリにも意味を持たないキー。
///
/// 合成キーは前景のウィンドウにも届く。設定画面のキー捕獲は、この VK を
/// 押されたキーとして数えない (`src/main.ts` の `HEARTBEAT_KEY_CODE`)。
/// 変えるときは両方を揃えること。
const HEARTBEAT_VK: u32 = 0xFC;
/// 生存確認のキーに載せる `dwExtraInfo`。値は ASCII の "NOXH"。
///
/// [`crate::inject::SELF_INJECTED_MARKER`] とは別にする。あちらは
/// 「無視するだけ」だが、こちらは**観測したことを数える**必要がある。
const HEARTBEAT_MARKER: usize = 0x4E4F_5848;

/// フックスレッドへ「フックを設置し直せ」と伝えるメッセージ。
///
/// `WM_APP` 以降はアプリが自由に使える範囲。
const WM_REHOOK: u32 = 0x8000 + 1;


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

    // 自分が SendInput した合成入力 (注入の Ctrl+V 等) だけを無視する。
    //
    // 判定に `LLKHF_INJECTED` を使ってはいけない。このフラグは**注入元を区別しない**ため、
    // RDP / PowerToys Keyboard Manager / AutoHotkey / 一部のキーボードユーティリティを
    // 経由した**正当なユーザー入力まで全部無視**され、ホットキーが無反応になる
    // (design.md の既知の制限として記録されていたもの)。
    // 自プロセス由来かどうかは `dwExtraInfo` に載せた印で判別する
    // ([`crate::inject::SELF_INJECTED_MARKER`])。
    if info.dwExtraInfo == crate::inject::SELF_INJECTED_MARKER {
        return;
    }

    // 生存確認のダミーキー ([`spawn_hook_watchdog`])。見えたことだけ数えて捨てる。
    // PTT の解釈には絶対に通さない。
    if info.dwExtraInfo == HEARTBEAT_MARKER {
        HEARTBEAT_SEEN.fetch_add(1, Ordering::SeqCst);
        return;
    }

    // 「フックが呼ばれている」ことの証拠。番犬がこれを見て生死を判断する。
    HOOK_EVENTS_SEEN.fetch_add(1, Ordering::SeqCst);
    // ページから来たキーと重ねないための時刻 ([`feed_window_key`])。
    // GetTickCount は共有メモリを読むだけで、呼び出しの不変条件に触れない。
    // SAFETY: 引数なし。
    LAST_HOOK_KEY_TICK.store(unsafe { GetTickCount() }.max(1), Ordering::SeqCst);

    let is_down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    let is_up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
    if !is_down && !is_up {
        return;
    }
    interpret_key(info.vkCode, is_down, KeySource::Hook);
}

/// キーがどこから来たか ([`interpret_key`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeySource {
    /// 低レベルフック。キーが前景アプリへ届く**前**に観測する。
    Hook,
    /// 設定画面のページ ([`feed_window_key`])。キーは既に自分へ届いた後。
    Window,
}

/// 設定画面 (nox-voice 自身の WebView2) のページが受けたキーを、フックと
/// 同じ状態機械へ渡す。`code` は `KeyboardEvent.code` ([`code_to_vk`])。
///
/// # なぜ要るのか (2026-09-26)
///
/// nox-voice の設定画面 (Chromium 系のウィンドウ) が前景のあいだ、Windows は
/// `WH_KEYBOARD_LL` のフックを呼ばない (`docs/hotkey-e2e.md`)。キーは画面に
/// 届いているのにフックだけが沈黙し、**ホットキーを設定した直後に、その画面の
/// まま押して試すと効かなかった**。そのあいだはページの `keydown` / `keyup` を
/// ここへ渡して PTT を成り立たせる。捕獲 (DOM 方式) と同じ考え方。
///
/// # フックが生きていればフックに任せる
///
/// 他のプロセスの LL フックが入っている機械では、このあいだもフックが呼ばれる
/// (観察者効果、`docs/hotkey-e2e.md`)。そのときは同じ打鍵がフックとページの
/// 両方から届く。状態機械は押下・離しを 1 回へ畳む ([`KEY_IS_DOWN`] の swap) が、
/// ページ側は IPC の分だけ遅れて届くので、短く叩くとフックの「押下・離し」の
/// **後に**ページの押下が着き、2 回目の打鍵に化けうる (トグルが即座に戻る)。
/// そこで、フックが直前 ([`WINDOW_KEY_HOOK_GRACE_MS`] 以内) にキーを観測して
/// いれば、ページのキーは捨てる。フックが沈黙している機械ではページだけが効く。
///
/// 前景ウィンドウを見て片方を止める作りにしないのは、フォーカスの移り変わりと
/// 入力の到着の順序に依存させないため。
pub fn feed_window_key(code: &str, is_down: bool) {
    let Some(vk) = code_to_vk(code) else {
        return; // 写せないキーはホットキーの部品になりえない。
    };
    if hook_saw_keys_recently() {
        return; // フックが同じ打鍵を解釈済み。
    }
    interpret_key(vk, is_down, KeySource::Window);
}

/// フックが [`WINDOW_KEY_HOOK_GRACE_MS`] 以内にキーを観測したか。
fn hook_saw_keys_recently() -> bool {
    let last = LAST_HOOK_KEY_TICK.load(Ordering::SeqCst);
    if last == 0 {
        return false;
    }
    // SAFETY: 引数なし。
    let now = unsafe { GetTickCount() };
    // GetTickCount は 49.7 日で一周する。差はラップを考慮して取る。
    now.wrapping_sub(last) < WINDOW_KEY_HOOK_GRACE_MS
}

/// キーの押下・離しを PTT / キャンセルとして解釈する (フックとページの共通部分)。
///
/// フックコールバックからも呼ばれるので、モジュール冒頭の不変条件
/// (確保・ロック・IO・panic 禁止) はここにもそのまま掛かる。
fn interpret_key(vk: u32, is_down: bool, source: KeySource) {
    let is_up = !is_down;

    // 設定済み修飾キーの押下状態を常に追跡する。捕獲モード中も更新してよい
    // (捕獲を確定した時点で set_mode_hotkey が畳むが、取り消し経路のために
    // 実態に追従させておく方が安全)。用途ごとに別の組み合わせを持つので、
    // 追跡もスロットごとに行う。
    let mut combos = [None; HOTKEY_SLOTS];
    for (slot, cell) in HOTKEY_COMBOS.iter().enumerate() {
        let raw = cell.load(Ordering::SeqCst);
        if raw == 0 {
            continue; // 未設定のスロット。
        }
        let combo = unpack_combo(raw);
        if let Some(bit) = combo.mod_bit(vk) {
            if is_down {
                MODS_DOWN[slot].fetch_or(1 << bit, Ordering::SeqCst);
            } else {
                MODS_DOWN[slot].fetch_and(!(1 << bit), Ordering::SeqCst);
            }
        }
        combos[slot] = Some(combo);
    }

    // 捕獲モード中は、キーを一切解釈せずに捨てる。
    //
    // 捕獲そのものはフロントの DOM イベントが行う ([`CAPTURE_MODE`] の doc)。
    // ここが担うのは**消音**だけ: 捕獲中に押されるのは多くの場合いま設定中の
    // ホットキーそのもの (既定なら 左Ctrl+Space) なので、PTT として解釈すると
    // 「キーを設定しようとしただけで録音が始まる」。キャンセルキー (Esc) も
    // 同じ理由で通さない — 捕獲の取り消しは DOM 側で処理する。
    // キー自体は抑制しない (呼び出し元が必ず `CallNextHookEx` へ流す)。
    if CAPTURE_MODE.load(Ordering::SeqCst) {
        return;
    }

    // 捕獲直後の押しっぱなしを締め出す。離した時点で解除する。
    if is_suppressed(vk) {
        if is_up {
            clear_suppression(vk);
        }
        return;
    }

    // 録音中のキャンセルキー。押下で Cancel を 1 回だけ報告し、離しで状態を戻す。
    // ホットキー経路の前に判定する (録音中は PTT の解釈より破棄が優先)。
    let cancel_vk = CANCEL_VK.load(Ordering::SeqCst);
    if cancel_vk != 0 && vk == cancel_vk && RECORDING_ACTIVE.load(Ordering::SeqCst) {
        if is_down {
            // オートリピートの連打を 1 回の押下に畳む。
            if !CANCEL_IS_DOWN.swap(true, Ordering::SeqCst) {
                send(HotkeyEvent::new(HotkeyEventKind::Cancel));
            }
        } else {
            CANCEL_IS_DOWN.store(false, Ordering::SeqCst);
        }
        // キー自体は抑制しない。Esc は挿入先アプリでも「取り消し」として
        // 働くべきなので、そのまま流す (CallNextHookEx は呼び出し側で必ず通る)。
        return;
    }

    // どのスロットのトリガーでもなければ、これ以上見る必要がない。
    //
    // 判定は**修飾キーの多い組み合わせから**行う。「Ctrl+Space」と「Space」の
    // ように片方がもう片方の部分集合になっていると、素朴に順番で見た場合
    // Ctrl+Space を押しても Space 側が先に食ってしまう。より具体的な
    // (=ユーザーが意図して押した) 方を優先する。
    let mut order: [usize; HOTKEY_SLOTS] = [0; HOTKEY_SLOTS];
    for (i, entry) in order.iter_mut().enumerate() {
        *entry = i;
    }
    // 確保しない比較キーを使う ([`HotkeyCombo::mods_count`] の doc 参照)。
    // 並べ替えも `sort_unstable_*` にする — 安定ソートは要素数次第で
    // 一時バッファを確保しうる (今は 2 要素なので実際には確保されないが、
    // スロットが増えたときに静かに不変条件が破れる)。
    order.sort_unstable_by_key(|slot| {
        std::cmp::Reverse(combos[*slot].map_or(0, |c| c.mods_count()))
    });

    for slot in order {
        let Some(combo) = combos[slot] else { continue };
        if vk != combo.vk {
            continue;
        }
        let kind = if is_down {
            // 修飾キーがすべて揃って初めてホットキーになる。
            // 揃っていないときに KEY_IS_DOWN を立てて返るのは**誤り**:
            // 「Space を先に押してから Ctrl を押した」とき、Space の
            // オートリピートで後から発火できなくなる。
            if !combo.mods_are_down(MODS_DOWN[slot].load(Ordering::SeqCst)) {
                continue;
            }
            // オートリピートの連打を 1 回の押下に畳む。
            if KEY_IS_DOWN[slot].swap(true, Ordering::SeqCst) {
                return;
            }
            HotkeyEventKind::Press {
                mode: HotkeyMode::from_slot(slot),
            }
        } else {
            // トリガーの離しは常に終端として扱う (途中で修飾キーを外しても、
            // 録音はトリガーを離すまで続く)。押していないスロットは飛ばす —
            // 同じトリガーを共有する別用途が、押してもいないのに離しだけ
            // 送ってしまうのを防ぐ。
            if !KEY_IS_DOWN[slot].swap(false, Ordering::SeqCst) {
                continue;
            }
            HotkeyEventKind::Release {
                mode: HotkeyMode::from_slot(slot),
            }
        };

        // トリガーが Alt そのものなら、**離しを観測した瞬間**にダミーキーを
        // 1 打挟む。挟まないと、前景アプリが「Alt を単独で叩いた」と読み、
        // ブラウザは入力欄からフォーカスを外す ([`break_lone_alt`] の doc)。
        //
        // **なぜ押下ではいけないか** (2026-08-29 に一度こう書いて外した):
        // 低レベルフックは、そのキーが前景アプリのキューへ届く**前**に走る。
        // 押下を観測した時点で `SendInput` すると、ダミーは Alt-down を
        // **追い越して**先に並ぶ:
        //     ダミー → Alt押下 → Alt解放   ＝ 依然として「単独 Alt」
        // Alt の押下と離しの間には何も無いままなので、メニューは起動する。
        // 離しの側で撒けば、フックは Alt-up がアプリへ届く前に走るので:
        //     Alt押下 → ダミー → Alt解放   ＝ 単独ではない → 起動しない
        //
        // ここに置く (`send` の前) のは、チャネルが満杯でも撒くため。
        // 「録音を取りこぼしたうえにフォーカスも失う」を作らない。
        //
        // ページから来たキー ([`KeySource::Window`]) では撒かない。ページが受け
        // 取った時点で、その離しはもう前景アプリ (= 自分) に届いた後なので効かない。
        if is_up && source == KeySource::Hook {
            break_lone_alt(combo.vk);
        }

        // 時刻はここで採る。デキュー時刻で判定すると、コントローラの詰まりが
        // そのまま「長押し」に化ける (HotkeyEvent の doc 参照)。
        send(HotkeyEvent::new(kind));
        // 1 回のキー入力で発火するのは 1 用途だけ。
        return;
    }
}

/// 単独 Alt を崩すために撒いたダミーキーの累計 ([`break_lone_alt`])。
///
/// `dropped_events` と同じ「フックの外から観測できる数字」。E2E はこれで
/// 「撒いたこと」を確かめる (フックからはログを出せないため)。
static LONE_ALT_BREAKS: AtomicU64 = AtomicU64::new(0);

/// [`LONE_ALT_BREAKS`] の現在値。
pub fn lone_alt_breaks() -> u64 {
    LONE_ALT_BREAKS.load(Ordering::Relaxed)
}

/// `vk` が Alt キーか (汎用・左・右)。
///
/// 純関数なのでテストできる。ここを間違えると、ダミーキーを撒きすぎるか
/// (実害は無いが余計)、単独 Alt の取りこぼしになる。
fn is_alt_key(vk: u32) -> bool {
    // VK_MENU / VK_LMENU / VK_RMENU。
    matches!(vk, 0x12 | 0xA4 | 0xA5)
}

/// トリガーが Alt 単独のとき、「Alt を単独で叩いた」状態を崩す。
///
/// # なぜ必要か (2026-08-29 の不具合)
///
/// フックはキーを**抑制しない**ので、ホットキーの押下と離しは前景アプリにも
/// そのまま届く。トリガーが Alt そのもの (既定の右 Alt など) だと、前景アプリ
/// から見た挙動は「Alt を押して、何も押さずに離した」— つまり Windows の
/// メニュー起動そのものになる。Chromium はこれを
/// **「ツールバーのメニューボタンへフォーカスを移す」**と解釈する
/// (Chromium の Accessibility: Keyboard Access に明記されている)。
///
/// 結果として、PTT を離した瞬間 = 録音終了の瞬間に、**ページの入力欄から
/// キャレットが外れる**。前景ウィンドウは Chrome のままなので R7 の照合は
/// 通過し、ログ上は貼付成功に見えるのに、実際には貼り先を失っている。
/// ネイティブアプリで報告が無かったのは、メニューを持たない Electron 製の
/// アプリでは単独 Alt が何もしないから。
///
/// # 直し方
///
/// どのアプリにも意味を持たないダミーキー ([`HEARTBEAT_VK`] と同じ
/// `VK_NONAME`) を 1 打挟む。Alt の押下と離しの間にキーが 1 つでも入れば、
/// それはもう「単独の Alt」ではないので、メニューは起動しない。
/// **キーを握り潰さない**という既存の不変条件を守ったまま直せるのが
/// この手の利点 (抑制すると Alt+Tab などが壊れる)。
///
/// # 撒く辺は「離し」(2026-08-29 に押下側へ書いて外した)
///
/// 最初は Alt の**押下**を観測した瞬間に撒いた。効かなかった。低レベルフックは
/// キーが前景アプリのキューへ届く**前**に走るので、押下時の `SendInput` は
/// Alt-down を追い越して先に並ぶ:
///
/// ```text
/// ダミー → Alt押下 → Alt解放     ＝ 押下と離しの間は空 = 単独 Alt
/// ```
///
/// **離し**を観測した瞬間に撒けば、フックは Alt-up がアプリへ届く前に走るので
/// 正しい順序になる:
///
/// ```text
/// Alt押下 → ダミー → Alt解放     ＝ 単独ではない → メニューは起動しない
/// ```
///
/// 「Alt を押し下げた状態で `VK_NONAME` が来る」= アプリから見れば
/// `Alt+VK_NONAME` の打鍵で、どのアプリにも割り当ての無い組み合わせなので
/// 副作用は無い (`VK_NONAME` を選んだ理由でもある)。
///
/// # フックの不変条件との折り合い
///
/// モジュール冒頭のとおり、コールバックでは確保・ロック・IO・panic が禁止。
/// `SendInput` は 1 回の syscall で、確保もロックも取らず、ここでの入力は
/// スタック上の固定長配列なので**この制約に触れない**。加えて呼ぶのは
/// 「Alt 単独トリガーを離した瞬間」だけ = 1 録音につき 1 回で、
/// キー入力ごとの経路には乗らない。
/// (フックの中から `SendInput` を呼ぶこと自体は Windows が想定している。
/// 自分で撒いた打鍵は `dwExtraInfo` のマーカーで捨てるので再帰もしない。)
///
/// `dwExtraInfo` に [`crate::inject::SELF_INJECTED_MARKER`] を載せるので、
/// 自分のフックはこの打鍵を最初の分岐で捨てる (PTT の解釈には通らない)。
///
/// 数える ([`LONE_ALT_BREAKS`]) のは常に行うが、**打鍵はテストでは送らない**。
/// `cargo test` が走っているマシンの前景アプリへキーを撒くわけにいかない。
fn break_lone_alt(trigger_vk: u32) {
    if !is_alt_key(trigger_vk) {
        return;
    }
    // 数えるのは常に行う (atomic なのでフックの制約に触れない)。
    // ログはここから出せない — フックでの IO は禁止なので、
    // 出すのはコントローラ側 ([`lone_alt_breaks`] を読む人) の仕事。
    LONE_ALT_BREAKS.fetch_add(1, Ordering::Relaxed);
    #[cfg(not(test))]
    {
        let make = |up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(HEARTBEAT_VK as u16),
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    time: 0,
                    dwExtraInfo: crate::inject::SELF_INJECTED_MARKER,
                },
            },
        };
        let inputs = [make(false), make(true)];
        // SAFETY: inputs は有効な INPUT 配列で、cbsize は正しい構造体サイズ。
        // 失敗 (UIPI で弾かれる等) しても録音は続けるので戻り値は見ない。
        let _ = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    }
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
    let mut hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) }
    {
        Ok(h) => h,
        Err(e) => {
            let _ = ready_tx.try_send(Err(format!("SetWindowsHookExW に失敗: {e}")));
            return;
        }
    };

    // フックコールバックは**このスレッドで**走る。応答が
    // `LowLevelHooksTimeout` を超えると OS はフックを無言で外すので、
    // 起動直後のようにプロセスが混んでいる時間帯でも確実に走れるよう
    // 優先度を上げておく。やることは atomic とチャネル送信だけなので、
    // 高優先度でも他を締め出さない。
    // SAFETY: 引数なし / 自スレッドのハンドルを渡すだけ。
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }

    // SAFETY: 引数なし。
    HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let _ = ready_tx.try_send(Ok(()));
    log::info!(
        "キーボードフックを設置 (ホットキー: {})",
        mode_hotkey(HotkeyMode::Inject)
            .map(|c| c.label())
            .unwrap_or_else(|| "未設定".to_string())
    );
    spawn_hook_watchdog();

    let mut msg = MSG::default();
    loop {
        // SAFETY: msg はスタック上の有効な MSG。
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            // 0 = WM_QUIT, -1 = エラー。どちらもループを抜ける。
            break;
        }
        if msg.message == WM_REHOOK {
            // 再設置は**このスレッドで**行う。フックは設置したスレッドに
            // 紐づき、そのスレッドがメッセージを回していないとコールバックが
            // 来ない。番犬スレッドから直接設置し直してはいけない。
            // SAFETY: hook は直前の SetWindowsHookExW 由来。
            let _ = unsafe { UnhookWindowsHookEx(hook) };
            // SAFETY: 上と同じ引数。
            match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0) } {
                Ok(h) => {
                    hook = h;
                    HOOK_REINSTALLS.fetch_add(1, Ordering::SeqCst);
                    log::warn!("キーボードフックを再設置しました (OS に外されていた)");
                }
                Err(e) => log::error!("キーボードフックの再設置に失敗: {e}"),
            }
            continue;
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

/// フックが生きているか定期的に確かめ、死んでいたら再設置させる番犬。
///
/// # なぜ要るのか
///
/// `WH_KEYBOARD_LL` は Windows に**無言で外される**。コールバックの応答が
/// `HKCU\Control Panel\Desktop\LowLevelHooksTimeout` (既定 300ms) を
/// 超えると、OS はそのフックをチェーンから落とす。エラーも通知も無く、
/// `UnhookWindowsHookEx` も呼ばれないので、**アプリ側からは何も起きていない
/// ように見えたまま、以降すべてのキーが届かなくなる**。
/// 起動直後のようにプロセスが重い時間帯は特に踏みやすい。
///
/// 本アプリでは「アプリは起動しているのにホットキーがまったく反応しない
/// (再起動すると直る)」という形で表面化していた。実測でも、キーが
/// `GetAsyncKeyState` に現れている (= OS の入力キューには届いている) のに
/// フックのコールバックが 1 度も呼ばれない起動が再現している。
///
/// # 生存確認の方法
///
/// 「キーが来ないこと」は無操作と区別できない。そこで**自分で 1 打だけ送って
/// 自分のフックが見たかを確かめる**。使うのは `VK_NONAME` (0xFC) —
/// Microsoft が「ダミーのキーストローク用」と明記している、意味を持たない VK。
/// `dwExtraInfo` に [`HEARTBEAT_MARKER`] を載せ、フック側は PTT の解釈には
/// 一切通さずに「見えた」ことだけ数える。
///
/// 直近の間隔でフックが何か観測していれば生きているのは自明なので、
/// **無操作の間だけ**送る。人が打っている間にダミーキーは流さない。
fn spawn_hook_watchdog() {
    let _ = thread::Builder::new()
        .name("nox-hotkey-watchdog".to_string())
        .spawn(|| {
            let mut last_seen = HOOK_EVENTS_SEEN.load(Ordering::SeqCst);
            loop {
                thread::sleep(HOOK_HEALTH_INTERVAL);
                let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
                if tid == 0 {
                    return; // フックスレッドが終了した = アプリ終了。
                }
                let seen = HOOK_EVENTS_SEEN.load(Ordering::SeqCst);
                if seen != last_seen {
                    last_seen = seen; // 実キーが流れている = 生きている。
                    continue;
                }
                match probe_hook_alive() {
                    HookProbe::Alive => {}
                    // 送れないときは判定しない (欠測であって故障ではない)。
                    HookProbe::Unknown => {}
                    // 自分の画面が前景のあいだは確かめようがない (フックが呼ばれない)。
                    HookProbe::OwnWindowForeground => {}
                    HookProbe::Silent => request_rehook(tid, "定期の生存確認"),
                }
                last_seen = HOOK_EVENTS_SEEN.load(Ordering::SeqCst);
            }
        });
}

/// 生存確認の結果。
///
/// **`Silent` は「フックが死んでいる」の証明ではない**。生存確認は
/// `SendInput` で自分宛にキーを 1 打送るが、[SendInput の MSDN][si] によれば
/// UIPI でブロックされた場合、**戻り値でも `GetLastError` でも判別できない**
/// (昇格したウィンドウが前景にあるときなど)。つまり「生きているフックを
/// 再設置する」偽陽性が構造的に混ざる。再設置自体は無害だが、
/// ログが騒がしくなると本物の故障が埋もれるので、
/// 再設置ログには**そのとき前景だったプロセス名**を必ず添える
/// ([`request_rehook`])。切り分けの手掛かりはそこにしか無い。
///
/// [si]: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookProbe {
    /// ダミーキーがフックまで届いた = 生きている。
    Alive,
    /// 送ったのに観測できなかった = 死んでいる**か**、送出が握り潰された。
    Silent,
    /// そもそも送れなかった。判定不能 (欠測)。
    Unknown,
    /// nox-voice 自身のウィンドウが前景なので確かめなかった。
    ///
    /// 設定画面 (WebView2 = Chromium 系) が前景のあいだ、Windows は LL フックを
    /// 呼ばない (`docs/hotkey-e2e.md`)。ダミーキーを送っても必ず `Silent` になり、
    /// 生きているフックの再設置と「応答しません」の警告を毎回生むだけだった。
    /// そのあいだのホットキーはページが受ける ([`feed_window_key`])。
    OwnWindowForeground,
}

/// ダミーキーを 1 打送って、自分のフックが観測できるか確かめる。
fn probe_hook_alive() -> HookProbe {
    if own_window_is_foreground() {
        return HookProbe::OwnWindowForeground;
    }
    let before = HEARTBEAT_SEEN.load(Ordering::SeqCst);
    if !send_heartbeat_key() {
        return HookProbe::Unknown;
    }
    let deadline = Instant::now() + HEARTBEAT_GRACE;
    while HEARTBEAT_SEEN.load(Ordering::SeqCst) == before && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if HEARTBEAT_SEEN.load(Ordering::SeqCst) == before {
        HookProbe::Silent
    } else {
        HookProbe::Alive
    }
}

/// 前景のウィンドウが nox-voice 自身のものか ([`HookProbe::OwnWindowForeground`])。
fn own_window_is_foreground() -> bool {
    // SAFETY: 引数なし。戻り値は NULL でありうるので下で見る。
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return false;
    }
    let mut process_id: u32 = 0;
    // SAFETY: hwnd は非 NULL、出力先はスタック上の有効な u32。
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    process_id != 0 && process_id == std::process::id()
}

/// フックスレッドへ再設置を要求する。
///
/// 前景プロセス名を必ず添える ([`HookProbe`] の doc)。昇格したウィンドウが
/// 前景にある間は `SendInput` が UIPI で捨てられ、生きているフックでも
/// `Silent` になる。ログにその名前が並んでいれば、後から
/// 「本当に外れていた」のか「偽陽性だった」のかを読み分けられる。
///
/// 起動からフックが観測したキーイベントの累計も添える。再設置を繰り返しても
/// 0 のままなら、途中で外れたのではなく**最初から 1 度も呼ばれていない**
/// (2026-09-26、CI のランナーで実際にそうなった)。
fn request_rehook(tid: u32, reason: &str) {
    let foreground = crate::foreground::capture_foreground();
    log::warn!(
        "キーボードフックが応答しません ({reason}: 生存確認のキーを観測できない)。再設置します \
         [そのときの前景プロセス: {} / 昇格ウィンドウが前景だと生存確認だけが届かないことがある / \
         起動からフックが観測したキーイベント {} 件・再設置 {} 回]",
        if foreground.process_name.is_empty() {
            "<unknown>"
        } else {
            foreground.process_name.as_str()
        },
        HOOK_EVENTS_SEEN.load(Ordering::SeqCst),
        HOOK_REINSTALLS.load(Ordering::SeqCst),
    );
    // SAFETY: 引数は数値のみ。宛先スレッドが無ければ Err が返るだけ。
    if let Err(e) = unsafe { PostThreadMessageW(tid, WM_REHOOK, WPARAM(0), LPARAM(0)) } {
        log::error!("フックスレッドへの再設置要求に失敗: {e}");
    }
}

/// 「今すぐ」フックの生存を確かめ、応答が無ければ再設置させる。
///
/// # なぜ設定画面から離れた瞬間に呼ぶのか
///
/// 捕獲そのものは DOM イベントへ移したのでフックに依存しない
/// ([`CAPTURE_MODE`] の doc)。しかし**他のアプリの上での PTT は引き続き
/// フックに依存する**。ホットキーを設定した利用者は、他のアプリへ移って
/// 押して試す。そこで「設定はできたのに押しても録音が始まらない」に当たると、
/// 原因がフックの脱落だと気づく手掛かりが何も無い。
///
/// 定期の番犬 ([`spawn_hook_watchdog`]) は 15 秒周期で、しかも
/// 「無操作のとき」しか確認しないので、移った直後には間に合わない。
/// 設定画面から離れた瞬間 (`WindowEvent::Focused(false)`) に能動的に 1 回確かめる。
///
/// 以前は捕獲の開始時に呼んでいた。そのときは設定画面 (WebView2) が前景で、
/// Windows は LL フックを呼ばないので確認は必ず「応答なし」になり、生きている
/// フックの再設置と誤った警告を毎回生んでいた (2026-09-26、`docs/hotkey-e2e.md`)。
/// 設定画面が前景のあいだは確かめずに見送る ([`HookProbe::OwnWindowForeground`])。
/// 設定画面で押して試すホットキーはページが受ける ([`feed_window_key`])。
///
/// 呼び出し側をブロックしないよう、確認は自前のスレッドで行う
/// (最悪 [`HEARTBEAT_GRACE`] + 再確認ぶん待つ)。捕獲の開始表示を
/// 1 秒近く遅らせると、それはそれで「押しても反応しない」に見える。
pub fn ensure_hook_alive_async(reason: &'static str) {
    let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
    if tid == 0 {
        return; // フックスレッドが居ない (未設置 / 終了処理中)。
    }
    let spawned = thread::Builder::new()
        .name("nox-hook-health".to_string())
        .spawn(move || match probe_hook_alive() {
            HookProbe::Alive => log::info!("フックの生存を確認 ({reason})"),
            HookProbe::Unknown => {
                log::info!("フックの生存確認を送れませんでした ({reason})。判定は見送る")
            }
            HookProbe::OwnWindowForeground => log::info!(
                "設定画面が前景なので、フックの生存確認は見送る ({reason})。\
                 このあいだのホットキーは設定画面が受ける"
            ),
            HookProbe::Silent => {
                request_rehook(tid, reason);
                // 再設置が効いたかまで見る。効いていなければ、次に押しても
                // 発火しないことが**この時点で**分かる (ログに残る)。
                thread::sleep(Duration::from_millis(200));
                match probe_hook_alive() {
                    HookProbe::Alive => log::info!("再設置後にフックの生存を確認"),
                    HookProbe::Silent => log::warn!(
                        "再設置してもフックが応答しません。ホットキーが効かない可能性があります"
                    ),
                    HookProbe::Unknown | HookProbe::OwnWindowForeground => {}
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("フックの生存確認スレッドを起動できません: {e}");
    }
}

/// 生存確認用のダミーキーを 1 打送る。送れたら `true`。
fn send_heartbeat_key() -> bool {
    let make = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(HEARTBEAT_VK as u16),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: HEARTBEAT_MARKER,
            },
        },
    };
    let inputs = [make(false), make(true)];
    // SAFETY: inputs は有効な INPUT 配列で、cbsize は正しい構造体サイズ。
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    sent as usize == inputs.len()
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
            if let Err(e) = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) } {
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
            HotkeyEventKind::Press { .. } => self.on_press(event.at),
            HotkeyEventKind::Release { .. } => self.on_release(event.at),
            // キャンセルは解釈器を通らない。コントローラが直接処理し、
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

/// キー捕獲の状態機械 (UI 側の「キーを押して設定」用) の判定部。
///
/// Win32 に触れないのでテストできる。捕獲したキーの集合をそのまま採用せず、
/// ここで**採用してよいか**を決める:
///
/// - Esc は「取り消し」。ホットキーには選べない (設定をやり直せなくなる)
/// - 非修飾キーは 1 つまで (2 押し同時判定はフックの照合と合わない)
/// - 単独指定は従来基準 ([`is_allowed_hotkey`])、組み合わせなら
///   [`is_allowed_combo_key`] を使う
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// この組み合わせを採用する。
    Accept(HotkeyCombo),
    /// 取り消し。
    Cancel,
    /// ホットキーには使えない組み合わせ。理由を人間可読で添える。
    Rejected(String),
}

/// 捕獲したキーの集合をどう扱うか決める。
///
/// `keys` は押された順 ([コントローラ側](crate::lib) が積んだ順)。空は呼び出し側の
/// バグだが、落ちるより拒否を返す。
pub fn decide_capture_combo(keys: &[u32]) -> CaptureOutcome {
    if keys.is_empty() {
        return CaptureOutcome::Rejected("(キーが観測できませんでした)".to_string());
    }
    if keys.contains(&ESCAPE_VK) {
        return CaptureOutcome::Cancel;
    }
    for vk in keys {
        // マウスボタン等もここで範囲外として弾かれる。
        if !is_allowed_combo_key(*vk) {
            return CaptureOutcome::Rejected(key_label(*vk));
        }
    }
    let non_mods: Vec<u32> = keys
        .iter()
        .copied()
        .filter(|vk| !is_modifier(*vk))
        .collect();
    if non_mods.len() > 1 {
        let names = describe_keys(&non_mods);
        return CaptureOutcome::Rejected(format!("{names} (非修飾キーは 1 つまでです)"));
    }

    // トリガーの決め方:
    // - 非修飾キーがあればそれがトリガー (Ctrl + Space の Space)
    // - 修飾キーだけの組み合わせなら、最後に押した修飾キーをトリガーにする
    //   (Ctrl → Shift の順に押したら「Shift をタップ」で発火する形になる)
    let trigger = match non_mods.first() {
        Some(vk) => *vk,
        None => *keys.last().expect("空は上で弾いた"),
    };
    let mods: Vec<u32> = keys.iter().copied().filter(|vk| *vk != trigger).collect();

    match sanitize_combo(&mods, trigger) {
        Some(combo) => CaptureOutcome::Accept(combo),
        None => CaptureOutcome::Rejected(key_label(trigger)),
    }
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
                kind: HotkeyEventKind::Press { mode: HotkeyMode::Inject },
                at: t0,
            },
            HotkeyEvent {
                kind: HotkeyEventKind::Release { mode: HotkeyMode::Inject },
                at: t0 + Duration::from_millis(100),
            },
        ];

        let mut it = interp();
        assert_eq!(it.on_event(events[0]), Some(HotkeyAction::StartRecording));

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
                kind: HotkeyEventKind::Press { mode: HotkeyMode::Inject },
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
            kind: HotkeyEventKind::Press { mode: HotkeyMode::Inject },
            at: t0,
        });
        assert_eq!(
            it.on_event(HotkeyEvent {
                kind: HotkeyEventKind::Release { mode: HotkeyMode::Inject },
                at: t0 + Duration::from_millis(800),
            }),
            Some(HotkeyAction::StopRecording)
        );
    }

    // --- ホットキーの差し替えと捕獲 ---

    /// テスト用の組み合わせ。左 Ctrl + Space (既定と同じ形)。
    fn ctrl_space() -> HotkeyCombo {
        HotkeyCombo::from_parts(&[0xA2], 0x20).expect("左 Ctrl + Space は有効")
    }

    #[test]
    fn the_default_hotkey_is_left_ctrl_plus_space() {
        let combo = HotkeyCombo::default();
        assert_eq!(combo.vk, 0x20, "トリガーは Space");
        assert_eq!(combo.mods_vec(), vec![0xA2], "修飾子は左 Ctrl");
        assert_eq!(combo.label(), "左 Ctrl + Space");
    }

    #[test]
    fn packing_round_trips_through_a_single_u64() {
        for combo in [
            HotkeyCombo::default(),
            HotkeyCombo::from_parts(&[], 0x70).unwrap(),
            HotkeyCombo::from_parts(&[0xA2, 0xA0], 0x14).unwrap(),
            HotkeyCombo::from_parts(&[0x5B, 0xA5, 0xA1], 0x1D).unwrap(),
        ] {
            assert_eq!(unpack_combo(pack_combo(&combo)), combo);
        }
    }

    #[test]
    fn sanitize_normalizes_the_modifier_list() {
        // 重複・非修飾キー・トリガー自身は落とし、表示順に並ぶ。
        let combo = sanitize_combo(&[0x41, 0xA0, 0xA2, 0xA2, 0x20], 0x20)
            .expect("Space をトリガーにできる");
        assert_eq!(combo.mods_vec(), vec![0xA2, 0xA0]);
        assert_eq!(combo.vk, 0x20);

        // 上限を超えた分は切り捨てる。
        let combo = sanitize_combo(&[0xA0, 0xA1, 0xA2, 0xA4, 0xA5], 0x70).expect("F1 は有効");
        assert_eq!(combo.mods_vec().len(), MAX_HOTKEY_MODS);
    }

    #[test]
    fn space_needs_a_modifier_but_combos_relax_the_trigger_rule() {
        // 単独の Space は押しっぱなしで空白が入り続けるので不可 (従来基準)。
        assert!(sanitize_combo(&[], 0x20).is_none());
        // Ctrl + Space なら許す (これが新既定)。
        assert!(sanitize_combo(&[0xA2], 0x20).is_some());
        // 組み合わせでも危険なキーは不可: Ctrl+Enter は送信連発になる。
        assert!(sanitize_combo(&[0xA2], 0x0D).is_none());
        assert!(
            sanitize_combo(&[0xA2], 0x41).is_none(),
            "Ctrl+A も連打になる"
        );
        assert!(
            sanitize_combo(&[0xA2], 0x09).is_none(),
            "Ctrl+Tab も連打になる"
        );
        // 組み合わせで許される追加組: Space / テンキー。
        assert!(sanitize_combo(&[0xA2], 0x60).is_some());
    }

    #[test]
    fn key_labels_cover_the_likely_choices() {
        assert_eq!(key_label(0xA2), "左 Ctrl");
        assert_eq!(key_label(0x14), "CapsLock");
        assert_eq!(key_label(0x70), "F1");
        assert_eq!(key_label(0x7B), "F12");
        // 回帰: F13〜F24 は許可されているのに名前が無く、「VK 0x7C」と出ていた。
        assert_eq!(key_label(0x7C), "F13");
        assert_eq!(key_label(0x87), "F24");
        assert_eq!(key_label(0x41), "A");
        assert_eq!(key_label(0x30), "0");
        assert_eq!(key_label(0x60), "テンキー 0");
        assert_eq!(key_label(0x1D), "無変換");
        // 知らないキーでも読める形にする (空文字にしない)。
        assert_eq!(key_label(0xFE), "VK 0xFE");
    }

    // --- 一覧から選ぶ (2026-08-31) -----------------------------------------

    fn catalog_entry(vk: u32) -> HotkeyKeyOption {
        hotkey_key_catalog()
            .into_iter()
            .find(|k| k.vk == vk)
            .unwrap_or_else(|| panic!("VK 0x{vk:02X} が一覧に無い"))
    }

    #[test]
    fn catalog_is_internally_consistent() {
        // 生成関数と同じ述語を並べても何も確かめられない (それは書き写しで
        // あって検査ではない)。ここで固定するのは、**生成方法によらず
        // 一覧が満たすべき性質**だけにする。
        let catalog = hotkey_key_catalog();
        for key in &catalog {
            // 単独で選べるものは組み合わせでも選べる (規則の包含関係)。
            // 逆転すると、修飾キーを足した瞬間に選択肢が消える UI になる。
            assert!(!key.alone || key.with_mods, "{}", key.label);
            // そのまま選べるなら注記は無し。少しでも制約があるなら
            // (修飾キーが要る / そもそも選べない) 必ず理由が付く。
            assert_eq!(key.note.is_empty(), key.alone, "{}", key.label);
            // 名前が VK の生値のままのキーを一覧に並べない
            // (「VK 0x2C はホットキーに使えません」では何の話か分からない)。
            assert!(!key.label.starts_with("VK 0x"), "{}", key.label);
        }
        // 同じキーが 2 か所に出ない。重複すると、片方で選んで確定した後に
        // もう片方が押されていないように見える。
        let mut vks: Vec<u32> = catalog.iter().map(|k| k.vk).collect();
        vks.sort_unstable();
        let count = vks.len();
        vks.dedup();
        assert_eq!(vks.len(), count, "同じ VK が一覧に 2 回出ている");
    }

    #[test]
    fn catalog_reasons_do_not_lie() {
        // 「押している間ずっと入力先へ流れる」は文字を流すキーの理由。
        // 流さないキー (PrintScreen / NumLock) に付けると嘘になる。
        for key in hotkey_key_catalog() {
            if matches!(key.vk, 0x2C | 0x90) {
                assert!(key.note.contains("対象外"), "{} / {}", key.label, key.note);
                assert!(!key.note.contains("流れ続ける"), "{}", key.label);
            }
        }
    }

    #[test]
    fn catalog_reaches_f13_in_the_function_category() {
        // この経路の存在理由。ペダルの F13 / F14 は物理キーが無く、
        // 捕獲 UI では**押しようがない**。
        let f13 = catalog_entry(0x7C);
        assert_eq!(f13.label, "F13");
        assert_eq!(f13.category, CAT_FUNCTION);
        assert!(f13.alone, "F13 は単独で選べなければ意味が無い");
        assert!(f13.note.is_empty());
        // F1〜F24 が欠けなく並ぶ (途中が飛ぶと「無い」と誤解される)。
        let fkeys: Vec<u32> = hotkey_key_catalog()
            .iter()
            .filter(|k| k.category == CAT_FUNCTION)
            .map(|k| k.vk)
            .collect();
        assert_eq!(fkeys, (0x70..=0x87).collect::<Vec<u32>>());
    }

    #[test]
    fn catalog_explains_why_a_key_cannot_be_chosen() {
        // Space: 単独は不可、修飾キーを足せば可。**解き方**が理由に入っていること。
        let space = catalog_entry(0x20);
        assert!(!space.alone && space.with_mods);
        assert!(space.note.contains("修飾キー"), "{}", space.note);

        // Enter / 文字キー: どうやっても選べない。理由は許可規則の根拠そのもの。
        for vk in [0x0D, 0x41] {
            let key = catalog_entry(vk);
            assert!(!key.alone && !key.with_mods);
            assert!(key.note.contains("入力先"), "{}", key.note);
        }

        // Esc は「入力先へ流れる」が理由ではない。嘘の理由を出さないこと。
        let esc = catalog_entry(ESCAPE_VK);
        assert!(esc.note.contains("取り消し"), "{}", esc.note);
        assert!(!esc.note.contains("流れ続ける"), "{}", esc.note);

        // 半角/全角・かなは「押すたびに VK が変わる」が理由。
        for vk in [0xF3, 0x15] {
            let key = catalog_entry(vk);
            assert!(key.note.contains("キーコード"), "{}", key.note);
        }
    }

    #[test]
    fn catalog_categories_are_declared_and_ordered() {
        let categories = hotkey_key_categories();
        // 修飾キー → F キーの順。F13 へ 3 クリック (開く → F キー → F13) で
        // 届くのは、F キーが先頭付近に居ることが前提。
        assert_eq!(categories[0].id, CAT_MODIFIER);
        assert_eq!(categories[1].id, CAT_FUNCTION);
        let ids: Vec<&str> = categories.iter().map(|c| c.id).collect();
        // 一覧のキーはすべて宣言済みのカテゴリに属する (フロントが
        // 描き落とすカテゴリを作らない)。
        for key in hotkey_key_catalog() {
            assert!(ids.contains(&key.category), "{} / {}", key.label, key.category);
        }
    }

    #[test]
    fn catalog_agrees_with_the_capture_path() {
        // **一覧で選べるもの = 捕獲で押して決まるもの**であること。
        // 入口が 2 つある機能なので、ここが割れると「押せば決まるのに
        // 一覧では灰色」(あるいはその逆) という説明のつかない差が出る。
        // 一覧側の真偽値と、捕獲の判定 (decide_capture_combo) の結論を
        // 突き合わせる — 別々の関数の**結果**を比べるので、書き写しにならない。
        for key in hotkey_key_catalog() {
            if key.vk == ESCAPE_VK {
                // Esc だけは捕獲では「取り消し」に化ける (押して決めることが
                // 原理的にできない)。一覧側では選べないキーとして出す。
                assert!(!key.alone && !key.with_mods, "{}", key.label);
                assert_eq!(decide_capture_combo(&[key.vk]), CaptureOutcome::Cancel);
                continue;
            }

            // 単独。
            let alone = match decide_capture_combo(&[key.vk]) {
                CaptureOutcome::Accept(combo) => {
                    assert_eq!(Some(combo), sanitize_combo(&[], key.vk), "{}", key.label);
                    true
                }
                _ => false,
            };
            assert_eq!(alone, key.alone, "単独 {}", key.label);

            // 修飾キー付き。トリガー自身とは別の修飾キーで試す
            // (同じにすると sanitize_combo が落として単独に化ける)。
            let m = if key.vk == 0xA2 { 0xA0 } else { 0xA2 };
            let with_mods = match decide_capture_combo(&[m, key.vk]) {
                CaptureOutcome::Accept(combo) => {
                    assert_eq!(Some(combo), sanitize_combo(&[m], key.vk), "{}", key.label);
                    true
                }
                _ => false,
            };
            assert_eq!(with_mods, key.with_mods, "組み合わせ {}", key.label);
        }
    }

    #[test]
    fn escape_cancels_the_capture() {
        // Esc を採用できると、設定をやり直す手段が無くなる。
        assert_eq!(decide_capture_combo(&[0x1B]), CaptureOutcome::Cancel);
        // 組み合わせの一部に Esc が混ざっても取り消し扱い。
        assert_eq!(decide_capture_combo(&[0xA2, 0x1B]), CaptureOutcome::Cancel);
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
    fn modifier_and_function_keys_are_accepted_as_singles() {
        // 押しっぱなしでも挿入先に実害が出ないキー (単独指定の従来基準)。
        for vk in [
            0xA3, 0xA5, 0xA0, 0x5B, 0x5D, 0x14, 0x91, 0x13, 0x70, 0x87, 0x1C, 0x1D,
        ] {
            match decide_capture_combo(&[vk]) {
                CaptureOutcome::Accept(combo) => {
                    assert_eq!(combo.vk, vk, "VK 0x{vk:02X}");
                    assert!(combo.mods_vec().is_empty(), "単独なのに修飾子が付いた");
                }
                other => panic!("VK 0x{vk:02X}: {other:?}"),
            }
        }
    }

    #[test]
    fn combos_are_accepted_and_normalized() {
        // 新既定: 左 Ctrl + Space。
        assert_eq!(
            decide_capture_combo(&[0xA2, 0x20]),
            CaptureOutcome::Accept(ctrl_space())
        );
        // 押した順が逆でも同じ組み合わせになる。
        assert_eq!(
            decide_capture_combo(&[0x20, 0xA2]),
            CaptureOutcome::Accept(ctrl_space())
        );
        // 修飾キーだけの組み合わせも可 (最後に押した方をトリガーにする)。
        let pure = decide_capture_combo(&[0xA2, 0xA0]);
        let CaptureOutcome::Accept(combo) = pure else {
            panic!("修飾キーだけの組み合わせが拒否された: {pure:?}")
        };
        assert_eq!(combo.vk, 0xA0, "最後に押したキーがトリガー");
        assert_eq!(combo.mods_vec(), vec![0xA2]);
    }

    #[test]
    fn printable_keys_are_rejected() {
        // フックはキーを抑制しないので、PTT 中ずっと挿入先へ流れる。
        // Enter なら送信連発、A なら文字が入り続ける。
        for vk in [0x41, 0x5A, 0x30, 0x39, 0x0D, 0x20, 0x09, 0x08, 0xBC] {
            assert!(
                matches!(decide_capture_combo(&[vk]), CaptureOutcome::Rejected(_)),
                "VK 0x{vk:02X} を許してしまった"
            );
        }
    }

    #[test]
    fn a_rejection_names_the_key_for_the_user() {
        match decide_capture_combo(&[0x0D]) {
            CaptureOutcome::Rejected(label) => assert_eq!(label, "Enter"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn two_non_modifier_keys_are_rejected() {
        // 照合は「トリガー 1 個 + 修飾子」で行うため、非修飾キーの複数押し
        // は表現できない。Space + F1 のような設定を作らせない。
        let outcome = decide_capture_combo(&[0xA2, 0x20, 0x70]);
        assert!(
            matches!(outcome, CaptureOutcome::Rejected(_)),
            "{outcome:?}"
        );
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
            assert!(!is_allowed_combo_key(vk), "VK 0x{vk:02X}");
        }
    }

    #[test]
    fn confirmed_captures_ignore_every_key_until_it_is_released() {
        // M1 回帰: 捕獲確定時にキーがまだ押されたままなら、
        // オートリピートが通常経路へ流れて設定しただけで録音が始まる。
        hold_keys(&[0xA2, 0x20]);
        suppress_until_release_keys(&[0xA2, 0x20]);
        assert!(is_suppressed(0xA2), "左 Ctrl が締め出されていない");
        assert!(is_suppressed(0x20), "Space が締め出されていない");
        assert!(
            !KEY_IS_DOWN[HotkeyMode::Inject.slot()].load(Ordering::SeqCst),
            "押下状態が残っている"
        );

        clear_suppression(0x20);
        assert!(!is_suppressed(0x20), "離した Space がまだ締め出されている");
        assert!(is_suppressed(0xA2), "まだ押している Ctrl まで解除された");

        // 後片付け (プロセス共有の状態)。
        for slot in SUPPRESS_UNTIL_RELEASE.iter() {
            slot.store(0, Ordering::SeqCst);
        }
        release_all_test_keys();
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
    fn changing_the_hotkey_clears_the_held_state() {
        // 押しっぱなしのまま差し替えると、次の離しだけが届いて状態がねじれる。
        let slot = HotkeyMode::Inject.slot();
        KEY_IS_DOWN[slot].store(true, Ordering::SeqCst);
        MODS_DOWN[slot].store(1, Ordering::SeqCst);
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::from_parts(&[], 0x70).unwrap()));
        assert!(!KEY_IS_DOWN[slot].load(Ordering::SeqCst));
        assert_eq!(MODS_DOWN[slot].load(Ordering::SeqCst), 0);
        assert_eq!(mode_hotkey(HotkeyMode::Inject).expect("設定済み").vk, 0x70);
        // 後片付け (プロセス共有の状態なので戻す)。
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
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
    /// テスト中だけ「押されていることにする」キー ([`super::is_physically_down`])。
    static HELD_FOR_TEST: [AtomicU32; SUPPRESS_SLOTS] = [const { AtomicU32::new(0) }; SUPPRESS_SLOTS];

    /// `keys` を「今押されている」ことにする。実キーを押させられないテストで、
    /// 抑制の登録と解除を実物と同じ経路で動かすため。
    fn hold_keys(keys: &[u32]) {
        for (slot, vk) in HELD_FOR_TEST.iter().zip(keys.iter()) {
            slot.store(*vk, Ordering::SeqCst);
        }
    }

    fn release_all_test_keys() {
        for slot in HELD_FOR_TEST.iter() {
            slot.store(0, Ordering::SeqCst);
        }
    }

    pub(super) fn is_held_for_test(vk: u32) -> bool {
        HELD_FOR_TEST.iter().any(|slot| slot.load(Ordering::SeqCst) == vk)
    }

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
        let info = KBDLLHOOKSTRUCT {
            vkCode: vk_code as u32,
            ..Default::default()
        };
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
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Cancel)
        );
        assert!(rx.try_recv().is_err(), "オートリピートごとに発火した");

        // 離せば再度押せる (次の録音でキャンセルできる)。
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYUP);
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Cancel)
        );

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
        assert!(
            CANCEL_IS_DOWN.load(Ordering::SeqCst),
            "押下状態が立っていない"
        );
        // keyup 自体はイベントにならない (離しは報告する必要がない)。
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYUP);
        assert!(
            !CANCEL_IS_DOWN.load(Ordering::SeqCst),
            "離しても状態が残った"
        );

        disarm_cancel_state();
    }

    #[test]
    fn capture_mode_takes_priority_over_the_cancel_key() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_cancel_vk(DEFAULT_CANCEL_VK);
        set_recording_active(true);
        drain(&rx);

        // 捕獲モード中の Esc は「捕獲の取り消し」であって録音の破棄ではない。
        // 取り消しは DOM 側 (フォーカスのあるウィンドウ) が処理するので、
        // フックはここで何も出してはいけない。
        let previous = CAPTURE_MODE.swap(true, Ordering::SeqCst);
        feed_key(DEFAULT_CANCEL_VK as i32, WM_KEYDOWN);
        assert!(
            rx.try_recv().is_err(),
            "捕獲モード中に Cancel 側へ吸われた"
        );
        assert!(
            !CANCEL_IS_DOWN.load(Ordering::SeqCst),
            "捕獲中の Esc で押下状態が汚れた"
        );

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

    // --- 組み合わせホットキーの照合 ------------------------------------------
    //
    // handle_key_event を直接叩いて、修飾キーとの組み合わせでだけ
    // Press / Release が報告されることを確かめる。

    #[test]
    fn the_chord_fires_only_while_the_modifier_is_held() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);

        // Space 単独では何も起きない (通常のタイピング)。
        feed_key(0x20, WM_KEYDOWN);
        feed_key(0x20, WM_KEYUP);
        assert!(rx.try_recv().is_err(), "修飾子なしの Space で発火した");

        // Ctrl を押してから Space → Press。
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );
        // Space を離す → Release (Ctrl はまだ押していられる)。
        feed_key(0x20, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release { mode: HotkeyMode::Inject })
        );

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    // --- 設定画面が前景のあいだ、ページから来たキー (2026-09-26) ---------------
    //
    // 設定画面 (WebView2) が前景のあいだ Windows は LL フックを呼ばない
    // (docs/hotkey-e2e.md) ので、ページの keydown / keyup を feed_window_key で
    // 同じ状態機械へ渡す。フックが生きている機械では両方から届くので、二重に
    // 数えないことも押さえる。

    /// フックが最後にキーを見た時刻を消す (ページのキーが使われる状態)。
    fn forget_hook_activity() {
        LAST_HOOK_KEY_TICK.store(0, Ordering::SeqCst);
    }

    #[test]
    fn window_keys_drive_the_same_chord_as_the_hook() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);
        forget_hook_activity();

        // Space 単独では何も起きない (設定画面での普通の入力)。
        feed_window_key("Space", true);
        feed_window_key("Space", false);
        assert!(rx.try_recv().is_err(), "修飾子なしの Space で発火した");

        feed_window_key("ControlLeft", true);
        feed_window_key("Space", true);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject }),
            "設定画面でホットキーを押しても発火しない"
        );
        feed_window_key("Space", false);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release { mode: HotkeyMode::Inject })
        );
        feed_window_key("ControlLeft", false);
        assert!(rx.try_recv().is_err());

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn window_keys_are_dropped_while_the_hook_is_hearing_the_same_keys() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);

        // 他のフックが入っている機械では、設定画面が前景でもフックが呼ばれる。
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );
        feed_key(0x20, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release { mode: HotkeyMode::Inject })
        );
        // ページの分は IPC の分だけ遅れて、フックの離しの**後に**着きうる。
        // 使うと 2 回目の打鍵に化ける (短押しのトグルが即座に戻る)。
        feed_window_key("ControlLeft", true);
        feed_window_key("Space", true);
        feed_window_key("Space", false);
        feed_window_key("ControlLeft", false);
        assert!(
            rx.try_recv().is_err(),
            "フックが解釈済みの打鍵を、ページからもう一度数えた"
        );

        feed_key(0xA2, WM_KEYUP);
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn window_keys_never_scatter_the_lone_alt_dummy() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, HotkeyCombo::from_parts(&[], 0xA5));
        drain(&rx);
        forget_hook_activity();

        let before = lone_alt_breaks();
        feed_window_key("AltRight", true);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );
        feed_window_key("AltRight", false);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release { mode: HotkeyMode::Inject })
        );
        // ページが受け取った時点で Alt の離しはもう自分に届いた後。撒いても効かず、
        // 設定画面へ余計な打鍵が入るだけ ([`break_lone_alt`] の doc)。
        assert_eq!(lone_alt_breaks(), before, "ページのキーでダミーキーを撒いた");

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn window_keys_are_silenced_while_capturing() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);
        forget_hook_activity();

        // 捕獲中に押されるのは、いま設定しようとしているキーそのもの。
        let generation = begin_capture();
        feed_window_key("ControlLeft", true);
        feed_window_key("Space", true);
        feed_window_key("Space", false);
        feed_window_key("ControlLeft", false);
        assert!(rx.try_recv().is_err(), "捕獲中のキーで録音が始まった");
        end_capture(Some(generation));

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn unmappable_window_codes_are_ignored() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        drain(&rx);
        forget_hook_activity();

        // code が空のキー (生存確認のダミーキーなど) と、写せない code。
        for code in ["", "Unidentified"] {
            feed_window_key(code, true);
            feed_window_key(code, false);
        }
        assert!(rx.try_recv().is_err());
    }

    // --- 単独 Alt のメニュー起動をつぶす (2026-08-29) -------------------------
    //
    // 既定の右 Alt のように**トリガー自体が Alt**だと、離した瞬間に前景アプリが
    // 「Alt を単独で叩いた」と読む。Chromium はそれをツールバーへのフォーカス
    // 移動として扱うので、ブラウザの入力欄からキャレットが外れる
    // ([`break_lone_alt`] の doc)。前景ウィンドウは変わらないため R7 では
    // 検出できない = ここで固定しておかないと静かに再発する。

    #[test]
    fn only_alt_keys_need_the_lone_tap_broken() {
        // 汎用・左・右の 3 つ。VK_CONTROL や VK_SHIFT を単独で叩いても
        // メニューは開かないので、余計な打鍵を撒かない。
        assert!(is_alt_key(0x12), "VK_MENU");
        assert!(is_alt_key(0xA4), "VK_LMENU");
        assert!(is_alt_key(0xA5), "VK_RMENU");
        assert!(!is_alt_key(0xA2), "左 Ctrl まで対象にしている");
        assert!(!is_alt_key(0xA0), "左 Shift まで対象にしている");
        assert!(!is_alt_key(0x20), "Space まで対象にしている");
        assert!(!is_alt_key(0x7C), "F13 まで対象にしている");
    }

    #[test]
    fn an_alt_trigger_gets_a_dummy_keystroke_on_release_not_on_press() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        // 既定の設定と同じ「右 Alt 単独」。
        set_mode_hotkey(HotkeyMode::Inject, HotkeyCombo::from_parts(&[], 0xA5));
        drain(&rx);

        let before = lone_alt_breaks();
        feed_key(0xA5, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );
        // **押下では撒かない**。ここが 2026-08-29 に一度間違えた辺:
        // フックは前景アプリより先に走るので、押下時に撒いたダミーは
        // Alt-down を追い越して並び、アプリから見た押下〜離しの間は
        // 空のまま = 単独 Alt が崩れない ([`break_lone_alt`] の doc)。
        assert_eq!(
            lone_alt_breaks(),
            before,
            "押下で撒いている (ダミーが Alt-down を追い越すので効かない)"
        );

        // 離しを観測した瞬間に 1 打。フックは Alt-up がアプリへ届く前に
        // 走るので、アプリから見た順序は Alt押下 → ダミー → Alt解放 になる。
        feed_key(0xA5, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release { mode: HotkeyMode::Inject })
        );
        assert_eq!(
            lone_alt_breaks(),
            before + 1,
            "右 Alt の離しでダミーキーが挟まれていない (Chrome のメニューがフォーカスを取る)"
        );

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn an_alt_trigger_breaks_once_per_press_not_per_autorepeat() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, HotkeyCombo::from_parts(&[], 0xA5));
        drain(&rx);

        let before = lone_alt_breaks();
        // 長押しはオートリピートで押下が何度も来る。撒くのは離しの 1 回だけ。
        feed_key(0xA5, WM_KEYDOWN);
        feed_key(0xA5, WM_KEYDOWN);
        feed_key(0xA5, WM_KEYDOWN);
        assert_eq!(lone_alt_breaks(), before, "オートリピートのたびに撒いている");
        feed_key(0xA5, WM_KEYUP);
        assert_eq!(lone_alt_breaks(), before + 1, "離しで撒いた回数が 1 でない");

        // 押していないのに離しだけ来ても撒かない (KEY_IS_DOWN が偽なら
        // そもそもこのスロットの離しではない)。
        feed_key(0xA5, WM_KEYUP);
        assert_eq!(lone_alt_breaks(), before + 1, "押していない離しでも撒いている");

        drain(&rx);
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn a_non_alt_trigger_is_left_alone() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        // Ctrl+Space。Space の押下が Alt の間に入るわけでもなく、そもそも
        // メニューを開かないので、ダミーキーは要らない。
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);

        let before = lone_alt_breaks();
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );
        assert_eq!(lone_alt_breaks(), before, "要らない打鍵を撒いている");

        feed_key(0x20, WM_KEYUP);
        feed_key(0xA2, WM_KEYUP);
        // 撒く辺を離しへ移したので、離しの側でも念のため見る。
        assert_eq!(lone_alt_breaks(), before, "離しで要らない打鍵を撒いている");
        drain(&rx);
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn the_dummy_key_is_the_same_no_op_the_watchdog_uses() {
        // どのアプリにも意味を持たない VK_NONAME。ここを普通のキー
        // (Ctrl など) に変えると、Alt と組んで AltGr になり文字が入る。
        assert_eq!(HEARTBEAT_VK, 0xFC, "VK_NONAME でなくなっている");
    }

    // --- 用途ごとのホットキー --------------------------------------------

    #[test]
    fn each_mode_fires_with_its_own_hotkey() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        // F13 は単独で選べる (通常のタイピングに現れないキー)。
        set_mode_hotkey(
            HotkeyMode::ClipboardOnly,
            HotkeyCombo::from_parts(&[], 0x7C),
        );
        drain(&rx);

        feed_key(0x7C, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press {
                mode: HotkeyMode::ClipboardOnly
            })
        );
        feed_key(0x7C, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release {
                mode: HotkeyMode::ClipboardOnly
            })
        );

        // 貼り付け用は独立して動く。
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press {
                mode: HotkeyMode::Inject
            })
        );
        feed_key(0x20, WM_KEYUP);
        feed_key(0xA2, WM_KEYUP);
        drain(&rx);

        set_mode_hotkey(HotkeyMode::ClipboardOnly, None);
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    /// 未設定のスロットは**何があっても**発火してはいけない。
    /// 既定値へ倒すと、設定していない用途が同じキーに相乗りする。
    #[test]
    fn an_unset_mode_never_fires() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        set_mode_hotkey(HotkeyMode::ClipboardOnly, None);
        drain(&rx);

        feed_key(0x7C, WM_KEYDOWN);
        feed_key(0x7C, WM_KEYUP);
        assert!(rx.try_recv().is_err(), "未設定の用途が発火した");

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    /// 用途を 3 つに増やしても、スロットの対応がずれていないこと。
    ///
    /// `slot()` と `from_slot()` が食い違うと、押したキーと違う用途が
    /// 動く (= 普通の音声入力のつもりで押したキーが画面を送る)。
    #[test]
    fn every_mode_round_trips_through_its_slot() {
        for mode in [
            HotkeyMode::Inject,
            HotkeyMode::ClipboardOnly,
            HotkeyMode::ScreenAsk,
        ] {
            assert!(mode.slot() < HOTKEY_SLOTS, "{mode:?}");
            assert_eq!(HotkeyMode::from_slot(mode.slot()), mode, "{mode:?}");
            assert!(!mode.label().is_empty(), "{mode:?}");
        }
        // 範囲外は貼り付けへ倒す (録音できなくなるより安全側)。
        assert_eq!(HotkeyMode::from_slot(HOTKEY_SLOTS), HotkeyMode::Inject);
    }

    /// 画面質問モードのキーは、自分のスロットだけを動かす。
    #[test]
    fn the_screen_ask_key_fires_only_its_own_mode() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        set_mode_hotkey(HotkeyMode::ClipboardOnly, HotkeyCombo::from_parts(&[], 0x7C));
        set_mode_hotkey(HotkeyMode::ScreenAsk, HotkeyCombo::from_parts(&[], 0x7D));
        drain(&rx);

        feed_key(0x7D, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press {
                mode: HotkeyMode::ScreenAsk
            })
        );
        feed_key(0x7D, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release {
                mode: HotkeyMode::ScreenAsk
            })
        );

        // 他の用途のキーは、それぞれ自分の用途で出る。
        feed_key(0x7C, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press {
                mode: HotkeyMode::ClipboardOnly
            })
        );
        feed_key(0x7C, WM_KEYUP);
        drain(&rx);

        set_mode_hotkey(HotkeyMode::ScreenAsk, None);
        set_mode_hotkey(HotkeyMode::ClipboardOnly, None);
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    /// 未設定の画面質問スロットは、他の用途のキーに相乗りしない。
    ///
    /// このスロットが既定値へ倒れると、**普通の音声入力のたびに
    /// 画面がクラウドへ送られる**。他の用途より事故の代償が大きい。
    #[test]
    fn an_unset_screen_ask_slot_never_rides_along() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        set_mode_hotkey(HotkeyMode::ScreenAsk, None);
        drain(&rx);

        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        let kinds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok().map(|e| e.kind)).collect();
        assert_eq!(
            kinds,
            vec![HotkeyEventKind::Press {
                mode: HotkeyMode::Inject
            }],
            "1 回の押下で 2 用途が発火した"
        );
        feed_key(0x20, WM_KEYUP);
        feed_key(0xA2, WM_KEYUP);
        drain(&rx);

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    /// 片方がもう片方の部分集合 (F13 と Ctrl+F13) のとき、
    /// 修飾キーまで揃っている方を採る。順番で先に見た方が食うと、
    /// Ctrl+F13 を押しているのに F13 側が動いてしまう。
    #[test]
    fn the_more_specific_chord_wins_over_the_bare_trigger() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(
            HotkeyMode::Inject,
            HotkeyCombo::from_parts(&[0xA2], 0x7C),
        );
        set_mode_hotkey(
            HotkeyMode::ClipboardOnly,
            HotkeyCombo::from_parts(&[], 0x7C),
        );
        drain(&rx);

        // Ctrl + F13 → 貼り付け用。
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x7C, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press {
                mode: HotkeyMode::Inject
            })
        );
        feed_key(0x7C, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release {
                mode: HotkeyMode::Inject
            }),
            "押した用途と違う用途の離しが出た"
        );
        feed_key(0xA2, WM_KEYUP);
        drain(&rx);

        // F13 単独 → クリップボードのみ。
        feed_key(0x7C, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press {
                mode: HotkeyMode::ClipboardOnly
            })
        );
        feed_key(0x7C, WM_KEYUP);
        drain(&rx);

        set_mode_hotkey(HotkeyMode::ClipboardOnly, None);
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn releasing_the_modifier_mid_hold_does_not_end_the_recording_early() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);

        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );

        // 先に Ctrl を離しても、Space のオートリピートでは再発火しない。
        feed_key(0xA2, WM_KEYUP);
        for _ in 0..3 {
            feed_key(0x20, WM_KEYDOWN);
        }
        assert!(rx.try_recv().is_err(), "途中経過でイベントが出た");

        // Space を離した時点で初めて Release。
        feed_key(0x20, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release { mode: HotkeyMode::Inject }),
            "トリガーの離しが報告されない"
        );
        assert!(rx.try_recv().is_err(), "二重に Release が出た");

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn autorepeat_under_a_held_chord_collapses_into_one_press() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);

        feed_key(0xA2, WM_KEYDOWN);
        for _ in 0..5 {
            feed_key(0x20, WM_KEYDOWN);
        }
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );
        assert!(rx.try_recv().is_err(), "オートリピートごとに発火した");

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn a_late_modifier_press_is_picked_up_by_autorepeat() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        drain(&rx);

        // Space を先に押しても、この時点では発火しない。
        feed_key(0x20, WM_KEYDOWN);
        assert!(rx.try_recv().is_err());
        // 押しっぱなしの Space が (KEY_IS_DOWN を立てずに) 見送られているので、
        // 後から Ctrl が来ても次の keydown で発火できる。
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN); // オートリピート相当
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject }),
            "修飾子を後から押した形で発火しない"
        );

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    /// 捕獲は「全部離した瞬間」に確定する。そのとき渡されるキーはもう
    /// 押されていないので、抑制に**登録してはいけない**。
    ///
    /// 登録してしまうと解除する keyup が二度と来ず、設定したばかりの
    /// ホットキーが再起動まで無反応になる (E2E T4 で実際に再現した回帰)。
    /// トリガーは F13/F14 — 物理キーボードに無いので、テスト実行中に
    /// 人間が押していることがありえず、`GetAsyncKeyState` の答えが安定する。
    #[test]
    fn keys_already_released_are_not_suppressed() {
        let _guard = hook_state_lock();
        let combo = [0x7C_u32, 0x7D_u32];

        suppress_until_release_keys(&combo);

        for vk in combo {
            assert!(
                !is_suppressed(vk),
                "離されているキー 0x{vk:02X} を抑制した — 解除の keyup が来ないので恒久的に無視される"
            );
        }

        // 新しいホットキーとして実際に発火できることまで見る
        // (抑制は is_suppressed で通常経路を止めるので、ここが本当の回帰点)。
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::from_parts(&[], 0x7C).expect("F13 は単独で選べる")));
        drain(&rx);
        feed_key(0x7C, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject }),
            "捕獲直後に設定したホットキーが発火しない"
        );
        feed_key(0x7C, WM_KEYUP);
        drain(&rx);

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn capture_mode_mutes_the_ptt_path_entirely() {
        // 捕獲中に現行ホットキー (左Ctrl+Space) をそのまま押しても、
        // 録音が始まってはいけない。捕獲そのものは DOM 側が拾うので、
        // フックがここで出すべきイベントは 1 件も無い。
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        let generation = begin_capture();
        drain(&rx);

        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        feed_key(0xA2, WM_KEYUP);
        feed_key(0x20, WM_KEYUP);
        assert!(
            rx.try_recv().is_err(),
            "捕獲中にフックがイベントを出した (設定しようとしただけで録音が始まる)"
        );

        // 捕獲を抜ければ、同じキーが普通にホットキーとして働く。
        end_capture(Some(generation));
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press {
                mode: HotkeyMode::Inject
            }),
            "捕獲を抜けたのにホットキーが復帰しない"
        );
        feed_key(0x20, WM_KEYUP);
        feed_key(0xA2, WM_KEYUP);
        drain(&rx);

        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
    }

    #[test]
    fn dom_codes_map_to_the_virtual_keys_the_rest_of_the_module_expects() {
        // 写像は 1 か所しか無い ([`code_to_vk`] の doc)。ここがずれると
        // 「押したキーと違うホットキーが保存される」形で表面化する。
        assert_eq!(code_to_vk("ControlLeft"), Some(0xA2));
        assert_eq!(code_to_vk("ControlRight"), Some(0xA3));
        assert_eq!(code_to_vk("ShiftLeft"), Some(0xA0));
        assert_eq!(code_to_vk("AltRight"), Some(0xA5));
        assert_eq!(code_to_vk("MetaLeft"), Some(0x5B));
        assert_eq!(code_to_vk("CapsLock"), Some(0x14));
        assert_eq!(code_to_vk("Space"), Some(0x20));
        assert_eq!(code_to_vk("Escape"), Some(ESCAPE_VK));
        assert_eq!(code_to_vk("Convert"), Some(0x1C));
        assert_eq!(code_to_vk("NonConvert"), Some(0x1D));
        // 連番の端。F13〜F24 は物理キーが無くても DOM に届く。
        assert_eq!(code_to_vk("F1"), Some(0x70));
        assert_eq!(code_to_vk("F12"), Some(0x7B));
        assert_eq!(code_to_vk("F13"), Some(0x7C));
        assert_eq!(code_to_vk("F24"), Some(0x87));
        assert_eq!(code_to_vk("KeyA"), Some(0x41));
        assert_eq!(code_to_vk("KeyZ"), Some(0x5A));
        assert_eq!(code_to_vk("Digit0"), Some(0x30));
        assert_eq!(code_to_vk("Digit9"), Some(0x39));
        assert_eq!(code_to_vk("Numpad0"), Some(0x60));

        // 範囲外・でっち上げは写さない。
        assert_eq!(code_to_vk("F0"), None);
        assert_eq!(code_to_vk("F25"), None);
        assert_eq!(code_to_vk("F01"), None);
        assert_eq!(code_to_vk("KeyAB"), None);
        assert_eq!(code_to_vk("Digit10"), None);
        assert_eq!(code_to_vk(""), None);
        assert_eq!(code_to_vk("Fn"), None);
        // ブラウザによっては `Unidentified` が来る。設定に使ってはいけない。
        assert_eq!(code_to_vk("Unidentified"), None);
    }

    #[test]
    fn mapped_codes_round_trip_through_the_existing_labels_and_rules() {
        // DOM 方式にしても、許可判定と表示名は既存の Rust 側をそのまま通る。
        let keys = codes_to_vks(&["ControlLeft".into(), "F13".into()]).expect("写せるはず");
        assert_eq!(keys, vec![0xA2, 0x7C]);
        assert_eq!(describe_keys(&keys), "左 Ctrl + F13");
        match decide_capture_combo(&keys) {
            CaptureOutcome::Accept(combo) => assert_eq!(combo.label(), "左 Ctrl + F13"),
            other => panic!("採用されなかった: {other:?}"),
        }

        // 押しっぱなしで実害が出るキーは、DOM 経由でも同じ理由で拒否される。
        let enter = codes_to_vks(&["Enter".into()]).expect("Enter は写せる");
        assert!(matches!(
            decide_capture_combo(&enter),
            CaptureOutcome::Rejected(_)
        ));
    }

    #[test]
    fn an_unmappable_code_is_reported_rather_than_silently_dropped() {
        // 黙って落とすと「Win + F13」のつもりが「F13」単独で保存される。
        let err = codes_to_vks(&["MediaPlayPause".into(), "F13".into()])
            .expect_err("写せない code があれば失敗するはず");
        assert_eq!(err, "MediaPlayPause");
    }

    #[test]
    fn suppression_lifts_key_by_key_as_each_confirmed_key_is_released() {
        let _guard = hook_state_lock();
        let rx = hook_events();
        set_mode_hotkey(HotkeyMode::Inject, Some(ctrl_space()));
        hold_keys(&[0xA2, 0x20]);
        suppress_until_release_keys(&[0xA2, 0x20]);
        drain(&rx);

        // 確定直後の押しっぱなし (オートリピート) は通常経路へ流れない。
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert!(rx.try_recv().is_err(), "締め出し中に Press が出た");

        // Space を離すと Space の締め出しだけ解ける。
        feed_key(0x20, WM_KEYUP);
        assert!(!is_suppressed(0x20));
        assert!(is_suppressed(0xA2), "まだ押している Ctrl まで解けた");

        // Ctrl は物理的にも押されているので、Space の再押下は
        // 「新しいコードの始まり」として発火してよい。
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );
        feed_key(0x20, WM_KEYUP);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Release { mode: HotkeyMode::Inject })
        );

        // Ctrl を離せば締め出しは全部解けて、普通に使える。
        feed_key(0xA2, WM_KEYUP);
        assert!(!is_suppressed(0xA2));
        feed_key(0xA2, WM_KEYDOWN);
        feed_key(0x20, WM_KEYDOWN);
        assert_eq!(
            rx.try_recv().map(|e| e.kind).ok(),
            Some(HotkeyEventKind::Press { mode: HotkeyMode::Inject })
        );

        release_all_test_keys();
        set_mode_hotkey(HotkeyMode::Inject, Some(HotkeyCombo::default()));
        for slot in SUPPRESS_UNTIL_RELEASE.iter() {
            slot.store(0, Ordering::SeqCst);
        }
    }
}
