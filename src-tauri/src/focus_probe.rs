//! キーボードフォーカスとキャレットの観測 (`GetGUIThreadInfo`)。
//!
//! # なぜ `GetForegroundWindow` では足りないのか
//!
//! R7 の照合は前景ウィンドウ (= どのトップレベルが活性か) しか見ていない。
//! ところが Windows の入力先は 2 段階ある:
//!
//! - **前景 (activation)**: どのトップレベルウィンドウが活性か
//! - **フォーカス (focus)**: その前景スレッドのキュー内で、どの HWND が
//!   キー入力を受け取るか
//!
//! 前者が動かなくても後者だけが動くことがある。ブラウザはこの「フォーカスが
//! 一瞬でも外れた」を DOM の `blur` として観測し、**戻ってきてもキャレットを
//! 復元しない** (contenteditable / textarea の選択位置は自動では戻らない)。
//! つまり「ログ上は R7 を通過して貼り付けたのに、入力欄のカーソルが外れる」
//! という症状は、前景だけを見ていると原理的に検出できない。
//!
//! `GetGUIThreadInfo(0, ..)` は前景スレッドの `hwndFocus` / `hwndCaret` を
//! 返すので、この 1 段深い層を観測できる。
//!
//! # 読むときの注意: `caret=0` は異常ではない
//!
//! `hwndCaret` は **システムキャレット**を持つスレッドの HWND。Chromium 系は
//! 支援技術が有効なときしかシステムキャレットを作らないので、Chrome では
//! 通常 0 のままになる。したがって Chrome の診断では `focus` の変化を見る。
//! `caret` が意味を持つのはメモ帳・Office など Win32 のエディットを使うアプリ。
//!
//! # 既定では 1 行も出さない
//!
//! 常駐アプリなので、録音のたびに数行出すとログがすぐ埋まる。通常の観測は
//! `debug!` (既定の `Info` では出ない)。**不変条件が破れたときだけ `warn!`**
//! で出す — 「フォーカスを奪わない」は設計上の約束 (design.md R7 / Q2) なので、
//! 破れたことは既定の運用でも気づけなければ意味がない。
//!
//! 詳細を見たいときは環境変数で上げる:
//!
//! ```text
//! set NOX_VOICE_LOG=debug
//! ```

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
    GUI_CARETBLINKING,
};

/// ある瞬間の「入力がどこへ行くか」。
///
/// HWND を `isize` で持つのは、[`crate::session::TargetWindow`] と揃えるため
/// (ログの 16 進表記をそのまま突き合わせられる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FocusSnapshot {
    /// 前景ウィンドウ。R7 が照合しているのはこれ。
    pub foreground: isize,
    /// 前景スレッドのキーボードフォーカス。ページ内の入力先はこの下にある。
    pub focus: isize,
    /// システムキャレットを持つ HWND (Chrome では通常 0 / 上のモジュール注記参照)。
    pub caret: isize,
    /// キャレットが点滅中か (`GUI_CARETBLINKING`)。
    pub caret_blinking: bool,
    /// 前景ウィンドウの所有プロセス ID。HWND 値の使い回しを見分けるため。
    pub process_id: u32,
}

/// 2 つのスナップショットの間で「何が動いたか」。
///
/// 前景 → フォーカス → キャレットの順に粗いものを優先して報告する。
/// 前景が動いていればフォーカスが動くのは当たり前なので、両方言っても
/// 読み手の役に立たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusChange {
    /// 何も動いていない。期待する状態。
    Unchanged,
    /// 前景そのものが変わった。R7 が貼付を中止する状況。
    ForegroundMoved,
    /// **前景は同じなのにキーボードフォーカスが移った。**
    /// ブラウザの入力欄からキャレットが外れる症状の直接の原因になる。
    FocusMoved,
    /// 前景もフォーカスも同じだが、システムキャレットが消えた。
    CaretLost,
}

impl FocusChange {
    /// 「フォーカスを奪わない」という約束が破れているか。
    ///
    /// 前景移動は R7 が検出して中止できる (= 別の防御が効く) が、
    /// フォーカスとキャレットは誰も見ていないので、ここで鳴らす。
    pub fn is_violation(self) -> bool {
        matches!(self, FocusChange::FocusMoved | FocusChange::CaretLost)
    }

    fn label(self) -> &'static str {
        match self {
            FocusChange::Unchanged => "変化なし",
            FocusChange::ForegroundMoved => "前景が移動",
            FocusChange::FocusMoved => "キーボードフォーカスが移動",
            FocusChange::CaretLost => "キャレットが消滅",
        }
    }
}

impl FocusSnapshot {
    /// 前景スレッドの入力状態を採る。取れない項目は 0 のまま。
    ///
    /// 失敗しても呼び出し側は何も変えない。これは観測であって制御ではないので、
    /// 「測れなかった」を理由に録音や貼付を止めるのは本末転倒。
    pub fn capture() -> Self {
        // SAFETY: 引数なし。NULL を返しうるので下で 0 として扱う。
        let foreground = unsafe { GetForegroundWindow() };

        let mut process_id: u32 = 0;
        if !foreground.0.is_null() {
            // SAFETY: hwnd は非 NULL、出力はスタック上の有効な u32。
            unsafe { GetWindowThreadProcessId(foreground, Some(&mut process_id)) };
        }

        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // idThread = 0 は「前景スレッド」。前景が別プロセスでも読める
        // (昇格プロセスや別デスクトップでは失敗するので Err を潰す)。
        //
        // SAFETY: info は cbSize を埋めた有効な構造体。
        let ok = unsafe { GetGUIThreadInfo(0, &mut info) }.is_ok();

        Self {
            foreground: foreground.0 as isize,
            focus: if ok { hwnd_to_isize(info.hwndFocus) } else { 0 },
            caret: if ok { hwnd_to_isize(info.hwndCaret) } else { 0 },
            caret_blinking: ok && info.flags.contains(GUI_CARETBLINKING),
            process_id,
        }
    }

    /// ログ 1 行分。16 進なのは他のログ (R7 の照合) と突き合わせるため。
    pub fn describe(&self) -> String {
        format!(
            "前景=0x{:X}(pid={}) フォーカス=0x{:X} キャレット=0x{:X}{}",
            self.foreground,
            self.process_id,
            self.focus,
            self.caret,
            if self.caret_blinking { " (点滅中)" } else { "" }
        )
    }
}

/// 2 点間の変化を判定する (純関数なのでテストできる)。
pub fn classify(before: &FocusSnapshot, after: &FocusSnapshot) -> FocusChange {
    if before.foreground != after.foreground {
        return FocusChange::ForegroundMoved;
    }
    if before.focus != after.focus {
        return FocusChange::FocusMoved;
    }
    // 「元々無かった」を消滅と言わない。Chrome のようにそもそも
    // システムキャレットを作らないアプリで毎回 warn が出てしまう。
    if before.caret != 0 && after.caret == 0 {
        return FocusChange::CaretLost;
    }
    FocusChange::Unchanged
}

/// その時点の入力状態を `debug!` に残す。
///
/// 区間ではなく「点」を見たいところ (録音開始の直前など) で使う。
pub fn log_point(point: &str) -> FocusSnapshot {
    let snapshot = FocusSnapshot::capture();
    log::debug!("[focus] {point}: {}", snapshot.describe());
    snapshot
}

/// 「この区間でフォーカスが動いてはいけない」を測る番人。
///
/// `begin` で採り、`end` で採り直して差分を判定する。**戻り値を捨てないこと**
/// (`let _ = begin(..)` にすると即座に end が走って何も測れない) — なので
/// `Drop` ではなく明示的な `end` にしてある。
#[must_use = "end() を呼ばないと区間が測られない"]
pub struct FocusGuard {
    point: &'static str,
    before: FocusSnapshot,
}

impl FocusGuard {
    pub fn begin(point: &'static str) -> Self {
        let before = FocusSnapshot::capture();
        log::debug!("[focus] {point} 直前: {}", before.describe());
        Self { point, before }
    }

    /// 区間を閉じて結果を返す。
    pub fn end(self) -> FocusChange {
        let after = FocusSnapshot::capture();
        let change = classify(&self.before, &after);
        log::debug!(
            "[focus] {} 直後: {} [{}]",
            self.point,
            after.describe(),
            change.label()
        );
        if change.is_violation() {
            // 既定のログレベルでも出す。これが出たら、前景照合 (R7) を
            // 通過したまま入力欄のキャレットが失われる経路が生きている。
            log::warn!(
                "[focus] {} でフォーカスが動きました ({}): 直前 {} → 直後 {}",
                self.point,
                change.label(),
                self.before.describe(),
                after.describe()
            );
        }
        change
    }
}

/// 環境変数からログレベルを決める。
///
/// 診断は `debug!` なので既定 (`Info`) では 1 行も出ない。実機で追うときだけ
/// `NOX_VOICE_LOG=debug` で上げる。**未知の値では黙って下げない** — 打ち間違い
/// で「静かになった」のを「問題が直った」と読み違えるのが一番まずい。
pub fn log_level_from_env(raw: Option<&str>) -> log::LevelFilter {
    match raw.map(str::trim).unwrap_or("") {
        v if v.eq_ignore_ascii_case("trace") => log::LevelFilter::Trace,
        v if v.eq_ignore_ascii_case("debug") => log::LevelFilter::Debug,
        v if v.eq_ignore_ascii_case("warn") => log::LevelFilter::Warn,
        v if v.eq_ignore_ascii_case("error") => log::LevelFilter::Error,
        // 空・未知はどちらも既定へ。
        _ => log::LevelFilter::Info,
    }
}

fn hwnd_to_isize(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(foreground: isize, focus: isize, caret: isize) -> FocusSnapshot {
        FocusSnapshot {
            foreground,
            focus,
            caret,
            caret_blinking: caret != 0,
            process_id: 1234,
        }
    }

    #[test]
    fn nothing_moving_is_the_expected_state() {
        let s = snapshot(0x10, 0x20, 0x30);
        assert_eq!(classify(&s, &s), FocusChange::Unchanged);
        assert!(!FocusChange::Unchanged.is_violation());
    }

    #[test]
    fn the_foreground_moving_outranks_the_rest() {
        // 前景が動けばフォーカスも動く。粗い方だけを言う。
        let before = snapshot(0x10, 0x20, 0x30);
        let after = snapshot(0x11, 0x21, 0x31);
        assert_eq!(classify(&before, &after), FocusChange::ForegroundMoved);
        // R7 が別途検出して中止するので、ここでは warn を鳴らさない。
        assert!(!FocusChange::ForegroundMoved.is_violation());
    }

    #[test]
    fn focus_moving_under_a_stable_foreground_is_the_bug_we_are_hunting() {
        // これがブラウザで「カーソルが外れる」ときの署名。
        // 前景は同じなので R7 は通過してしまう = 誰も気づけない。
        let before = snapshot(0x10, 0x20, 0);
        let after = snapshot(0x10, 0x99, 0);
        let change = classify(&before, &after);
        assert_eq!(change, FocusChange::FocusMoved);
        assert!(change.is_violation(), "既定のログに出ないと発見できない");
    }

    #[test]
    fn a_caret_that_never_existed_is_not_a_loss() {
        // Chrome はシステムキャレットを作らないので caret は常に 0。
        // これを消滅と呼ぶと、録音のたびに嘘の warn が出る。
        let before = snapshot(0x10, 0x20, 0);
        let after = snapshot(0x10, 0x20, 0);
        assert_eq!(classify(&before, &after), FocusChange::Unchanged);
    }

    #[test]
    fn losing_an_existing_caret_is_reported() {
        // メモ帳など Win32 エディットを使うアプリではここが効く。
        let before = snapshot(0x10, 0x20, 0x30);
        let after = snapshot(0x10, 0x20, 0);
        let change = classify(&before, &after);
        assert_eq!(change, FocusChange::CaretLost);
        assert!(change.is_violation());
    }

    #[test]
    fn a_caret_appearing_is_not_a_violation() {
        // 入力欄をクリックした直後などに起きる。歓迎すべき変化。
        let before = snapshot(0x10, 0x20, 0);
        let after = snapshot(0x10, 0x20, 0x30);
        assert_eq!(classify(&before, &after), FocusChange::Unchanged);
    }

    #[test]
    fn the_description_uses_the_same_hex_as_the_r7_log() {
        // 貼付中止ログ ("録音時 0x19072A → 現在 ...") と目で突き合わせられること。
        let text = snapshot(0x19072A, 0x120FEC, 0).describe();
        assert!(text.contains("前景=0x19072A"), "{text}");
        assert!(text.contains("フォーカス=0x120FEC"), "{text}");
    }

    #[test]
    fn the_default_log_level_stays_quiet() {
        assert_eq!(log_level_from_env(None), log::LevelFilter::Info);
        assert_eq!(log_level_from_env(Some("")), log::LevelFilter::Info);
    }

    #[test]
    fn the_env_var_can_turn_the_probe_on() {
        assert_eq!(log_level_from_env(Some("debug")), log::LevelFilter::Debug);
        assert_eq!(log_level_from_env(Some("DEBUG")), log::LevelFilter::Debug);
        assert_eq!(log_level_from_env(Some(" trace ")), log::LevelFilter::Trace);
    }

    #[test]
    fn a_typo_falls_back_to_the_default_not_to_silence() {
        // "dbeug" で黙られると、直ったのか出ていないだけなのか分からなくなる。
        assert_eq!(log_level_from_env(Some("dbeug")), log::LevelFilter::Info);
    }

    #[test]
    fn capture_never_panics_without_a_foreground_window() {
        // CI やテストランナーには前景ウィンドウが無いことがある。
        // 観測が落ちてビルドを止めるのは本末転倒なので、0 で埋まるだけにする。
        let snapshot = FocusSnapshot::capture();
        // 値そのものは環境依存なので、形式が壊れていないことだけ見る。
        assert!(!snapshot.describe().is_empty());
    }
}
