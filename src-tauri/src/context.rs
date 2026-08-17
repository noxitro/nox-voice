//! deep context — 画面の文脈を UI Automation で読む。
//!
//! 録音開始の瞬間に、フォーカスされている要素の周辺テキストを取得して
//! STT と整形へ渡す。「さっきまで書いていた文章」が分かると、固有名詞の
//! 表記や文体が揃いやすくなる。
//!
//! # 絶対に録音を止めない
//!
//! UIA は相手アプリのプロセスを跨いで問い合わせるため、**相手次第で
//! いくらでも遅くなる** (Electron 系はツリーを遅延生成するので初回が重い)。
//! そこで取得は使い捨てスレッドへ投げ、[`CAPTURE_TIMEOUT`] で見切る。
//! 間に合わなければ結果を捨てて空のコンテキストで続行する —
//! 文脈は「あれば嬉しい」ものであって、録音を遅らせてよい理由にはならない。
//!
//! 見切ったスレッドは裏で走り続けるが、書き込む先は自分のチャネルだけなので
//! 放置してよい (受信側が落ちれば送信は失敗し、スレッドは静かに終わる)。
//!
//! # プライバシー (design.md R1)
//!
//! 画面テキストはクラウド (Groq / Gemini) へ送られる。したがって:
//!
//! - **既定は無効**。設定で明示的に有効化したときだけ取得する。
//! - `IsPassword` の要素は読まない。
//! - 取得量は [`MAX_CONTEXT_CHARS`] で頭打ちにする。
//! - **履歴 DB には保存しない**。その場で使って捨てる。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    IUIAutomationValuePattern, UIA_TextPatternId, UIA_ValuePatternId,
};

/// 取得を見切る時間。これを超えたら空のコンテキストで続行する。
pub const CAPTURE_TIMEOUT: Duration = Duration::from_millis(300);

/// 画面テキストの上限。長すぎるとプロンプトを圧迫し、送る情報も増える。
pub const MAX_CONTEXT_CHARS: usize = 2_000;

/// 取得した画面コンテキスト。
///
/// `Clone` は付けるが、**履歴には渡さないこと** (モジュール doc 参照)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenContext {
    /// フォーカス要素の周辺テキスト。
    pub text: String,
    /// どの経路で取れたか (ログと診断用)。
    pub source: ContextSource,
}

/// 取得経路。取れなかった理由も含む。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextSource {
    /// 設定で無効。
    #[default]
    Disabled,
    /// `TextPattern` から取れた (エディタ・ブラウザの入力欄など)。
    TextPattern,
    /// `ValuePattern` から取れた (単純なテキストボックス)。
    ValuePattern,
    /// 要素名しか取れなかった。
    ElementName,
    /// フォーカス要素がパスワード欄だったので読まなかった。
    PasswordSkipped,
    /// 制限時間内に返ってこなかった。
    TimedOut,
    /// UIA が使えない・要素が無い等。
    Unavailable,
}

impl ContextSource {
    pub fn label(self) -> &'static str {
        match self {
            ContextSource::Disabled => "無効",
            ContextSource::TextPattern => "TextPattern",
            ContextSource::ValuePattern => "ValuePattern",
            ContextSource::ElementName => "要素名",
            ContextSource::PasswordSkipped => "パスワード欄のためスキップ",
            ContextSource::TimedOut => "タイムアウト",
            ContextSource::Unavailable => "取得不可",
        }
    }
}

impl ScreenContext {
    /// 取得できなかったときの空コンテキスト。
    pub fn empty(source: ContextSource) -> Self {
        Self {
            text: String::new(),
            source,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// プロンプトへ渡せる文字列 (空なら `None`)。
    pub fn as_prompt_text(&self) -> Option<&str> {
        let trimmed = self.text.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

/// 前回の取得がまだ走っているか。
///
/// 打ち切った取得は裏で走り続ける。連続で録音されると、返ってこない相手に
/// 対してスレッドが積み上がっていく。**1 本だけ**に制限する。
static CAPTURE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 画面コンテキストを取得する。**呼び出し側をブロックするのは最大
/// [`CAPTURE_TIMEOUT`] まで。**
///
/// `enabled` が false なら何もせず空を返す (UIA にも触れない)。
pub fn capture(enabled: bool) -> ScreenContext {
    if !enabled {
        return ScreenContext::empty(ContextSource::Disabled);
    }

    // 前回が終わっていないなら諦める。文脈が 1 回無いだけで、
    // 遅い相手にスレッドを積み上げるよりずっとよい。
    if CAPTURE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        log::info!("前回の画面コンテキスト取得が未完了のため今回はスキップします");
        return ScreenContext::empty(ContextSource::Unavailable);
    }

    let (tx, rx) = crossbeam_channel::bounded::<ScreenContext>(1);
    let spawned = std::thread::Builder::new()
        .name("nox-uia-context".to_string())
        .spawn(move || {
            // 受信側が既に見切っていれば送信は失敗する。それでよい。
            let result = capture_blocking();
            // 打ち切られていても、ここまで来たら次の取得を許可する。
            CAPTURE_IN_FLIGHT.store(false, Ordering::SeqCst);
            let _ = tx.send(result);
        });

    if let Err(e) = spawned {
        CAPTURE_IN_FLIGHT.store(false, Ordering::SeqCst);
        log::warn!("画面コンテキスト取得スレッドを起動できません: {e}");
        return ScreenContext::empty(ContextSource::Unavailable);
    }

    match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(context) => {
            log::debug!(
                "画面コンテキスト: {} ({} 文字)",
                context.source.label(),
                context.text.chars().count()
            );
            context
        }
        Err(_) => {
            // 遅い相手 (Electron 等) では珍しくない。録音は続ける。
            log::info!(
                "画面コンテキストの取得が {} ms を超えたので打ち切りました",
                CAPTURE_TIMEOUT.as_millis()
            );
            ScreenContext::empty(ContextSource::TimedOut)
        }
    }
}

/// UIA を実際に叩く。専用スレッドで呼ばれる前提。
fn capture_blocking() -> ScreenContext {
    // COM を MTA で初期化する。UIA オブジェクトはこのスレッドでしか使わない。
    // SAFETY: このスレッドで最初の初期化。対で CoUninitialize する。
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        log::warn!("COM を初期化できません: {com:?}");
        return ScreenContext::empty(ContextSource::Unavailable);
    }

    let context = capture_with_uia();

    // SAFETY: 上の CoInitializeEx と対。
    unsafe { CoUninitialize() };
    context
}

fn capture_with_uia() -> ScreenContext {
    // SAFETY: COM は初期化済み。CLSID は UIA のもの。
    let automation: IUIAutomation =
        match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
            Ok(a) => a,
            Err(e) => {
                log::warn!("UI Automation を生成できません: {e}");
                return ScreenContext::empty(ContextSource::Unavailable);
            }
        };

    // SAFETY: automation は有効。フォーカスが無ければ Err。
    let element: IUIAutomationElement = match unsafe { automation.GetFocusedElement() } {
        Ok(e) => e,
        Err(e) => {
            log::debug!("フォーカス要素を取得できません: {e}");
            return ScreenContext::empty(ContextSource::Unavailable);
        }
    };

    // パスワード欄は絶対に読まない。
    // SAFETY: element は有効。取得できなければ判定不能なので読まない側に倒す。
    match unsafe { element.CurrentIsPassword() } {
        Ok(is_password) if is_password.as_bool() => {
            log::info!("フォーカスがパスワード欄なので画面コンテキストは取得しません");
            return ScreenContext::empty(ContextSource::PasswordSkipped);
        }
        Ok(_) => {}
        Err(e) => {
            // 判定できないものを読むのは危ない。
            log::debug!("IsPassword を判定できないため取得を見送ります: {e}");
            return ScreenContext::empty(ContextSource::PasswordSkipped);
        }
    }

    read_element(&element)
}

/// 要素からテキストを読む。対応パターンを上から順に試す。
///
/// 診断テストからも同じ経路を通せるよう切り出してある。
fn read_element(element: &IUIAutomationElement) -> ScreenContext {
    if let Some(text) = text_from_text_pattern(element) {
        return ScreenContext {
            text: trim_context(&text),
            source: ContextSource::TextPattern,
        };
    }
    if let Some(text) = text_from_value_pattern(element) {
        return ScreenContext {
            text: trim_context(&text),
            source: ContextSource::ValuePattern,
        };
    }
    if let Some(text) = text_from_name(element) {
        return ScreenContext {
            text: trim_context(&text),
            source: ContextSource::ElementName,
        };
    }
    ScreenContext::empty(ContextSource::Unavailable)
}

/// `TextPattern` から文書テキストを取る。エディタやブラウザの入力欄向け。
fn text_from_text_pattern(element: &IUIAutomationElement) -> Option<String> {
    // SAFETY: element は有効。パターン非対応なら Err か NULL。
    let pattern = unsafe { element.GetCurrentPattern(UIA_TextPatternId) }.ok()?;
    let pattern: IUIAutomationTextPattern = pattern.cast().ok()?;

    // SAFETY: pattern は有効。
    let range = unsafe { pattern.DocumentRange() }.ok()?;
    // GetText は上限を渡せる。相手に大量のテキストを作らせない。
    // SAFETY: range は有効。
    let text = unsafe { range.GetText(MAX_CONTEXT_CHARS as i32) }.ok()?;

    let text = text.to_string();
    (!text.trim().is_empty()).then_some(text)
}

/// `ValuePattern` から値を取る。単純なテキストボックス向け。
fn text_from_value_pattern(element: &IUIAutomationElement) -> Option<String> {
    // SAFETY: element は有効。
    let pattern = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) }.ok()?;
    let pattern: IUIAutomationValuePattern = pattern.cast().ok()?;
    // SAFETY: pattern は有効。
    let value = unsafe { pattern.CurrentValue() }.ok()?;
    let value = value.to_string();
    (!value.trim().is_empty()).then_some(value)
}

/// 最後の手段として要素名を使う。
fn text_from_name(element: &IUIAutomationElement) -> Option<String> {
    // SAFETY: element は有効。
    let name = unsafe { element.CurrentName() }.ok()?;
    let name = name.to_string();
    (!name.trim().is_empty()).then_some(name)
}

/// 上限まで切り詰める。**末尾を残す** — キャレット付近は文書の後ろ側に
/// あることが多く、今書いている話題に近い。
pub fn trim_context(text: &str) -> String {
    let normalized = text.replace('\r', "");
    let count = normalized.chars().count();
    if count <= MAX_CONTEXT_CHARS {
        return normalized.trim().to_string();
    }
    normalized
        .chars()
        .skip(count - MAX_CONTEXT_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_capture_touches_nothing() {
        let context = capture(false);
        assert_eq!(context.source, ContextSource::Disabled);
        assert!(context.is_empty());
        assert_eq!(context.as_prompt_text(), None);
    }

    #[test]
    fn a_second_capture_is_skipped_while_one_is_in_flight() {
        // 遅い相手でスレッドが積み上がらないこと。
        CAPTURE_IN_FLIGHT.store(true, Ordering::SeqCst);
        let context = capture(true);
        assert_eq!(context.source, ContextSource::Unavailable);
        assert!(context.is_empty());
        CAPTURE_IN_FLIGHT.store(false, Ordering::SeqCst);
    }

    #[test]
    fn trimming_keeps_the_tail() {
        // キャレット付近 = 末尾。頭を捨てて末尾を残す。
        let text: String = (0..MAX_CONTEXT_CHARS + 500).map(|_| 'あ').collect();
        let trimmed = trim_context(&text);
        assert_eq!(trimmed.chars().count(), MAX_CONTEXT_CHARS);

        let tail = format!("{}末尾の目印", "あ".repeat(MAX_CONTEXT_CHARS));
        assert!(trim_context(&tail).ends_with("末尾の目印"));
    }

    #[test]
    fn short_text_is_returned_as_is() {
        assert_eq!(trim_context("  短い文章  "), "短い文章");
    }

    #[test]
    fn carriage_returns_are_normalized() {
        assert_eq!(trim_context("一行目\r\n二行目"), "一行目\n二行目");
    }

    #[test]
    fn whitespace_only_context_counts_as_empty() {
        let context = ScreenContext {
            text: "  \n\t ".to_string(),
            source: ContextSource::TextPattern,
        };
        assert!(context.is_empty());
        assert_eq!(context.as_prompt_text(), None);
    }

    #[test]
    fn prompt_text_is_trimmed() {
        let context = ScreenContext {
            text: "  本文  ".to_string(),
            source: ContextSource::TextPattern,
        };
        assert_eq!(context.as_prompt_text(), Some("本文"));
    }

    #[test]
    fn every_source_has_a_label() {
        for source in [
            ContextSource::Disabled,
            ContextSource::TextPattern,
            ContextSource::ValuePattern,
            ContextSource::ElementName,
            ContextSource::PasswordSkipped,
            ContextSource::TimedOut,
            ContextSource::Unavailable,
        ] {
            assert!(!source.label().is_empty(), "{source:?}");
        }
    }

    /// アプリごとに何が読めるかを実機で調べる診断。
    ///
    /// `GetFocusedElement` は前景に依存するため、前景を奪えない環境では
    /// 1 アプリ分しか見られない。ここでは `ElementFromHandle` で
    /// 開いている各ウィンドウを直接見て、**どのアプリでどの経路が使えるか**
    /// を一覧にする (テキストの中身は出さない。他人の画面の内容なので)。
    ///
    /// 実行: `cargo test -- --ignored --nocapture live_uia_probe_windows`
    #[test]
    #[ignore = "実際の UIA を叩く。開いているウィンドウに依存する"]
    fn live_uia_probe_windows() {
        use windows::Win32::Foundation::{HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        };

        // 可視のトップレベルウィンドウを集める。
        static mut WINDOWS: Vec<isize> = Vec::new();
        unsafe extern "system" fn collect(hwnd: HWND, _: LPARAM) -> windows::core::BOOL {
            // SAFETY: EnumWindows は単一スレッドから同期的に呼ぶ。
            unsafe {
                if IsWindowVisible(hwnd).as_bool() {
                    let ptr = &raw mut WINDOWS;
                    (*ptr).push(hwnd.0 as isize);
                }
            }
            true.into()
        }

        // SAFETY: コールバックは上で定義したもの。
        unsafe {
            let ptr = &raw mut WINDOWS;
            (*ptr).clear();
            let _ = EnumWindows(Some(collect), LPARAM(0));
        }
        // SAFETY: EnumWindows は同期的に完了している。
        let windows: Vec<isize> = unsafe {
            let ptr = &raw const WINDOWS;
            (*ptr).clone()
        };

        // SAFETY: このスレッドで COM を初期化し、最後に解放する。
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        assert!(com.is_ok(), "COM を初期化できない");
        // SAFETY: COM は初期化済み。
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .expect("UIA を生成できる");

        println!("{:<28} {:<16} 文字数", "ウィンドウ", "経路");
        println!("{}", "-".repeat(60));
        let mut probed = 0;
        for hwnd in windows {
            let mut buf = [0u16; 128];
            // SAFETY: hwnd は列挙で得た有効な値。
            let len = unsafe { GetWindowTextW(HWND(hwnd as *mut _), &mut buf) };
            if len <= 0 {
                continue; // タイトルの無いウィンドウは対象外。
            }
            let title: String = String::from_utf16_lossy(&buf[..len as usize])
                .chars()
                .take(26)
                .collect();
            let mut pid = 0u32;
            // SAFETY: hwnd は有効。
            unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };

            // SAFETY: automation と hwnd は有効。
            let element = match unsafe { automation.ElementFromHandle(HWND(hwnd as *mut _)) } {
                Ok(e) => e,
                Err(_) => {
                    println!("{title:<28} {:<16} -", "要素取得不可");
                    continue;
                }
            };
            // トップレベル要素の Name はウィンドウタイトルなので、それだけでは
            // 「テキストが読めるアプリか」が分からない。子コントロールまで
            // 降りて、実際に本文が読める経路があるかを見る。
            let mut best = read_element(&element);
            for child in child_windows(hwnd) {
                // SAFETY: child は列挙で得た有効な HWND。
                let Ok(child_element) =
                    (unsafe { automation.ElementFromHandle(HWND(child as *mut _)) })
                else {
                    continue;
                };
                let found = read_element(&child_element);
                if rank(found.source) > rank(best.source) {
                    best = found;
                }
                if best.source == ContextSource::TextPattern {
                    break; // これ以上良い経路は無い。
                }
            }

            println!(
                "{title:<28} {:<16} {}",
                best.source.label(),
                best.text.chars().count()
            );
            probed += 1;
        }

        // SAFETY: 上の CoInitializeEx と対。
        unsafe { CoUninitialize() };
        assert!(probed > 0, "調べられるウィンドウが 1 つも無かった");
    }

    /// 経路の望ましさ。大きいほど本文に近い。
    #[cfg(test)]
    fn rank(source: ContextSource) -> u8 {
        match source {
            ContextSource::TextPattern => 3,
            ContextSource::ValuePattern => 2,
            ContextSource::ElementName => 1,
            _ => 0,
        }
    }

    /// 子ウィンドウを列挙する (深さ 1 段、上限つき)。
    #[cfg(test)]
    fn child_windows(parent: isize) -> Vec<isize> {
        use windows::Win32::Foundation::{HWND, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;

        static mut CHILDREN: Vec<isize> = Vec::new();
        unsafe extern "system" fn collect(hwnd: HWND, _: LPARAM) -> windows::core::BOOL {
            // SAFETY: EnumChildWindows は同期的に単一スレッドから呼ばれる。
            unsafe {
                let ptr = &raw mut CHILDREN;
                if (*ptr).len() < 40 {
                    (*ptr).push(hwnd.0 as isize);
                }
            }
            true.into()
        }

        // SAFETY: コールバックは上の定義。parent は有効な HWND。
        unsafe {
            let ptr = &raw mut CHILDREN;
            (*ptr).clear();
            let _ = EnumChildWindows(Some(HWND(parent as *mut _)), Some(collect), LPARAM(0));
            let ptr = &raw const CHILDREN;
            (*ptr).clone()
        }
    }

    /// 実機で UIA が動くことの確認。フォーカス次第で結果が変わるので
    /// 内容は問わず、**時間内に返ること**と panic しないことを見る。
    ///
    /// 実行: `cargo test -- --ignored --nocapture live_uia_capture`
    #[test]
    #[ignore = "実際の UIA を叩く。前景アプリに依存する"]
    fn live_uia_capture() {
        let started = std::time::Instant::now();
        let context = capture(true);
        let elapsed = started.elapsed();

        println!("経路   : {}", context.source.label());
        println!("文字数 : {}", context.text.chars().count());
        if let Some(text) = context.as_prompt_text() {
            let preview: String = text.chars().take(120).collect();
            println!("先頭   : {preview:?}");
        }
        println!("所要   : {elapsed:?}");

        assert!(
            elapsed < CAPTURE_TIMEOUT + Duration::from_millis(200),
            "打ち切りが効いていない: {elapsed:?}"
        );
        assert!(context.text.chars().count() <= MAX_CONTEXT_CHARS);
    }
}
