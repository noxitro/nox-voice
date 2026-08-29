//! **技術検証スパイク (テスト専用モジュール)**。製品経路からは一切呼ばれない。
//!
//! 調べたい一点: **Chrome / Electron のテキスト入力欄の内容を UIA で
//! 読み返せるか**。読めるなら Typeless 相当の自動辞書学習 (貼り付け後に
//! ユーザーが直した内容を検知して辞書へ入れる) が成り立つ。
//!
//! 既存の走査 ([`crate::screen::win32`]) は `ElementFromHandle` +
//! `EnumChildWindows` (深さ 1) しか見ておらず、**Chromium の本文には
//! 構造的に届かない** — Chromium は 1 つの HWND
//! (`Chrome_RenderWidgetHostHWND`) の内側に、子 HWND を持たない UIA 要素
//! ツリーとして本文を出すため。ここでは
//!
//! 1. `IUIAutomationTreeWalker` (Control / Raw) による**木の下降**
//! 2. `FindAll(TreeScope_Subtree)` による一括取得
//! 3. **遅延有効化の待ち** (Chromium は要求されて初めて非同期にツリーを作る)
//! 4. `TextPattern` / `ValuePattern` / `LegacyIAccessible` / `Name` の**併記**
//! 5. `AddAutomationEventHandler` (TextChanged) と FocusChanged の**購読**
//!
//! を実測する。
//!
//! # プライバシー
//!
//! **本文は一切出力しない。** 出すのは経路・文字数・所要時間・要素数だけ。
//! 他人の画面の内容だから (design.md R1 / `screen.rs` と同じ方針)。
//! パスワード欄は [`crate::context::must_not_read`] で除外する。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, BSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationFocusChangedEventHandler,
    IUIAutomationFocusChangedEventHandler_Impl, IUIAutomationLegacyIAccessiblePattern,
    IUIAutomationTextPattern, IUIAutomationTreeWalker, IUIAutomationValuePattern,
    TreeScope_Subtree, UIA_CONTROLTYPE_ID, UIA_DocumentControlTypeId,
    UIA_EVENT_ID, UIA_EditControlTypeId, UIA_LegacyIAccessiblePatternId, UIA_TextControlTypeId,
    UIA_TextPatternId, UIA_Text_TextChangedEventId, UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

/// 1 要素から読める文字数を上限で頭打ちにする。相手に大量の文字列を
/// 作らせないため (既存の `MAX_CONTEXT_CHARS` と同じ発想)。
const READ_CAP: i32 = 4_000;

/// 木の下降で見る要素数の上限。**打ち切ったら報告に出す**。
const MAX_NODES: usize = 30_000;

/// 木の下降の深さ上限。**打ち切ったら報告に出す**。
const MAX_DEPTH: usize = 40;

// ---------------------------------------------------------------------------
// COM / UIA の準備
// ---------------------------------------------------------------------------

/// COM を MTA で初期化し、drop で解放する。
struct ComGuard;

impl ComGuard {
    fn new() -> Option<Self> {
        // SAFETY: このスレッドで初期化し、Drop で対の CoUninitialize を呼ぶ。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.is_ok().then_some(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: new() の CoInitializeEx と対。
        unsafe { CoUninitialize() };
    }
}

fn new_automation() -> Option<IUIAutomation> {
    // SAFETY: COM は初期化済み。CLSID は UIA のもの。
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.ok()
}

// ---------------------------------------------------------------------------
// 対象ウィンドウの列挙
// ---------------------------------------------------------------------------

/// 調べる対象のウィンドウ。
#[derive(Debug, Clone)]
struct Target {
    hwnd: isize,
    /// 実行ファイル名 (小文字)。**ウィンドウタイトルは持たない** —
    /// タイトルには開いている文書名やページ名が出るため。
    exe: String,
}

/// 可視のトップレベルウィンドウのうち、対象の実行ファイルのものを集める。
fn targets(exes: &[&str]) -> Vec<Target> {
    let mut hwnds: Vec<isize> = Vec::new();

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        // SAFETY: lparam には下で載せた &mut Vec<isize> が入る。
        // EnumWindows は同期的なので参照先は列挙中ずっと有効。
        let found = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
        // SAFETY: hwnd は列挙で得た有効な値。
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut buf = [0u16; 8];
            // タイトルの無いウィンドウ (不可視のメッセージ窓など) は外す。
            // SAFETY: hwnd は有効。長さ 0 を判定するだけで中身は使わない。
            if unsafe { GetWindowTextW(hwnd, &mut buf) } > 0 {
                found.push(hwnd.0 as isize);
            }
        }
        true.into()
    }

    let ptr = &raw mut hwnds;
    // SAFETY: コールバックは上の定義。列挙中 hwnds は生きている。
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(ptr as isize));
    }

    hwnds
        .into_iter()
        .filter_map(|hwnd| {
            let exe = exe_name(hwnd)?;
            exes.iter()
                .any(|want| exe == want.to_ascii_lowercase())
                .then_some(Target { hwnd, exe })
        })
        .collect()
}

/// ウィンドウを持つプロセスの実行ファイル名 (小文字)。
fn exe_name(hwnd: isize) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: hwnd は有効。pid は書き込み先。
    unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };
    exe_name_of_pid(pid)
}

/// プロセス ID から実行ファイル名 (小文字)。
///
/// Chromium の中の UIA 要素は HWND を持たないので、そちらはこちらで引く。
fn exe_name_of_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: 権限が無ければ Err。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    // SAFETY: handle は有効。buf/len は対応している。
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    // SAFETY: OpenProcess と対。
    unsafe { let _ = CloseHandle(handle); };
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or(&path)
            .to_ascii_lowercase(),
    )
}

// ---------------------------------------------------------------------------
// 1 要素からの読み取り (経路ごとに別々に測る)
// ---------------------------------------------------------------------------

/// 1 要素を各経路で読んだ結果の**文字数だけ**。本文は保持しない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Routes {
    text_pattern: usize,
    value_pattern: usize,
    legacy: usize,
    name: usize,
    /// パスワード欄等で読み取りを見送ったか。
    skipped: bool,
}

impl Routes {
    fn best(self) -> usize {
        self.text_pattern
            .max(self.value_pattern)
            .max(self.legacy)
            .max(self.name)
    }

    fn merge_max(&mut self, other: Routes) {
        self.text_pattern = self.text_pattern.max(other.text_pattern);
        self.value_pattern = self.value_pattern.max(other.value_pattern);
        self.legacy = self.legacy.max(other.legacy);
        self.name = self.name.max(other.name);
        self.skipped |= other.skipped;
    }
}

fn chars_of(bstr: BSTR) -> usize {
    // BSTR → String。中身は数えるだけで、決して表に出さない。
    bstr.to_string().trim().chars().count()
}

/// 1 要素を 4 経路すべてで読み、**文字数だけ**返す。
fn probe_element(element: &IUIAutomationElement) -> Routes {
    if crate::context::must_not_read(element) {
        return Routes {
            skipped: true,
            ..Default::default()
        };
    }

    let mut routes = Routes::default();

    // TextPattern (エディタ・ブラウザの文書)。
    // SAFETY: element は有効。非対応なら Err か NULL。
    if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_TextPatternId) } {
        if let Ok(pattern) = pattern.cast::<IUIAutomationTextPattern>() {
            // SAFETY: pattern は有効。
            if let Ok(range) = unsafe { pattern.DocumentRange() } {
                // SAFETY: range は有効。上限つきで取る。
                if let Ok(text) = unsafe { range.GetText(READ_CAP) } {
                    routes.text_pattern = chars_of(text);
                }
            }
        }
    }

    // ValuePattern (単純なテキストボックス)。
    // SAFETY: element は有効。
    if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) } {
        if let Ok(pattern) = pattern.cast::<IUIAutomationValuePattern>() {
            // SAFETY: pattern は有効。
            if let Ok(value) = unsafe { pattern.CurrentValue() } {
                routes.value_pattern = chars_of(value);
            }
        }
    }

    // LegacyIAccessible (MSAA 経由。UIA ネイティブが無い相手の保険)。
    // SAFETY: element は有効。
    if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_LegacyIAccessiblePatternId) } {
        if let Ok(pattern) = pattern.cast::<IUIAutomationLegacyIAccessiblePattern>() {
            // SAFETY: pattern は有効。
            if let Ok(value) = unsafe { pattern.CurrentValue() } {
                routes.legacy = chars_of(value);
            }
        }
    }

    // 要素名 (最後の手段)。
    // SAFETY: element は有効。
    if let Ok(name) = unsafe { element.CurrentName() } {
        routes.name = chars_of(name);
    }

    routes
}

// ---------------------------------------------------------------------------
// 木の下降
// ---------------------------------------------------------------------------

/// 木を降りた結果の要約。**本文は含まない**。
#[derive(Debug, Clone, Default)]
struct Descent {
    nodes: usize,
    max_depth_seen: usize,
    node_limit_hit: bool,
    depth_limit_hit: bool,
    /// 入力欄になりうる型 (Document / Edit) の数。
    editable_nodes: usize,
    /// Text 型の数 (読み取り専用の本文)。
    text_nodes: usize,
    /// 全要素を通じた経路別の最大文字数。
    best: Routes,
    /// Document / Edit 型に限った経路別の最大文字数。
    best_editable: Routes,
    /// `HasKeyboardFocus` が立っていた要素の経路別文字数。
    focused: Option<Routes>,
    elapsed: Duration,
}

/// `walker` で `root` から木を降り、各要素を 4 経路で読む。
fn descend(walker: &IUIAutomationTreeWalker, root: &IUIAutomationElement) -> Descent {
    let started = Instant::now();
    let mut out = Descent::default();
    // 明示スタック。深い木で Rust のスタックを溢れさせない。
    let mut stack: Vec<(IUIAutomationElement, usize)> = vec![(root.clone(), 0)];

    while let Some((element, depth)) = stack.pop() {
        if out.nodes >= MAX_NODES {
            out.node_limit_hit = true;
            break;
        }
        out.nodes += 1;
        out.max_depth_seen = out.max_depth_seen.max(depth);

        let routes = probe_element(&element);
        out.best.merge_max(routes);

        // SAFETY: element は有効。取れなければ既定値として扱う。
        let control_type = unsafe { element.CurrentControlType() }
            .unwrap_or(UIA_CONTROLTYPE_ID(0));
        if control_type == UIA_DocumentControlTypeId || control_type == UIA_EditControlTypeId {
            out.editable_nodes += 1;
            out.best_editable.merge_max(routes);
        } else if control_type == UIA_TextControlTypeId {
            out.text_nodes += 1;
        }

        // SAFETY: element は有効。
        if unsafe { element.CurrentHasKeyboardFocus() }
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            let mut merged = out.focused.unwrap_or_default();
            merged.merge_max(routes);
            out.focused = Some(merged);
        }

        if depth >= MAX_DEPTH {
            out.depth_limit_hit = true;
            continue;
        }

        // 子を全部積む。
        // SAFETY: walker / element は有効。子が無ければ Err。
        let Ok(mut child) = (unsafe { walker.GetFirstChildElement(&element) }) else {
            continue;
        };
        loop {
            stack.push((child.clone(), depth + 1));
            // SAFETY: child は有効。次が無ければ Err。
            match unsafe { walker.GetNextSiblingElement(&child) } {
                Ok(next) => child = next,
                Err(_) => break,
            }
            if stack.len() > MAX_NODES {
                out.node_limit_hit = true;
                break;
            }
        }
    }

    out.elapsed = started.elapsed();
    out
}

/// `FindAll(TreeScope_Subtree)` で一括取得し、各要素を読む。
fn find_all_probe(automation: &IUIAutomation, root: &IUIAutomationElement) -> Descent {
    let started = Instant::now();
    let mut out = Descent::default();
    // SAFETY: automation は有効。
    let Ok(condition) = (unsafe { automation.CreateTrueCondition() }) else {
        out.elapsed = started.elapsed();
        return out;
    };
    // SAFETY: root / condition は有効。相手が応答しなければ Err。
    let Ok(array) = (unsafe { root.FindAll(TreeScope_Subtree, &condition) }) else {
        out.elapsed = started.elapsed();
        return out;
    };
    // SAFETY: array は有効。
    let len = unsafe { array.Length() }.unwrap_or(0);
    for i in 0..len {
        if out.nodes >= MAX_NODES {
            out.node_limit_hit = true;
            break;
        }
        // SAFETY: i は範囲内。
        let Ok(element) = (unsafe { array.GetElement(i) }) else {
            continue;
        };
        out.nodes += 1;
        let routes = probe_element(&element);
        out.best.merge_max(routes);
        // SAFETY: element は有効。
        let control_type = unsafe { element.CurrentControlType() }
            .unwrap_or(UIA_CONTROLTYPE_ID(0));
        if control_type == UIA_DocumentControlTypeId || control_type == UIA_EditControlTypeId {
            out.editable_nodes += 1;
            out.best_editable.merge_max(routes);
        } else if control_type == UIA_TextControlTypeId {
            out.text_nodes += 1;
        }
    }
    out.elapsed = started.elapsed();
    out
}

// ---------------------------------------------------------------------------
// イベントハンドラ
// ---------------------------------------------------------------------------

/// 受け取ったイベント数と、購読開始からの最初の 1 通までの ms。
static EVENTS: AtomicUsize = AtomicUsize::new(0);
static FIRST_EVENT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
static FOCUS_EVENTS: AtomicUsize = AtomicUsize::new(0);
static SUBSCRIBED_AT_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// TextChanged を数えるだけのハンドラ。**送られてきた要素の中身は読まない**。
#[implement(IUIAutomationEventHandler)]
struct CountingHandler;

impl IUIAutomationEventHandler_Impl for CountingHandler_Impl {
    fn HandleAutomationEvent(
        &self,
        _sender: windows::core::Ref<IUIAutomationElement>,
        _eventid: UIA_EVENT_ID,
    ) -> windows::core::Result<()> {
        EVENTS.fetch_add(1, Ordering::SeqCst);
        let elapsed = now_ms().saturating_sub(SUBSCRIBED_AT_MS.load(Ordering::SeqCst));
        let _ = FIRST_EVENT_MS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            (current == u64::MAX).then_some(elapsed)
        });
        Ok(())
    }
}

/// FocusChanged を数えるだけのハンドラ。
#[implement(IUIAutomationFocusChangedEventHandler)]
struct FocusHandler;

impl IUIAutomationFocusChangedEventHandler_Impl for FocusHandler_Impl {
    fn HandleFocusChangedEvent(
        &self,
        _sender: windows::core::Ref<IUIAutomationElement>,
    ) -> windows::core::Result<()> {
        FOCUS_EVENTS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 出力ヘルパ (本文は決して出さない)
// ---------------------------------------------------------------------------

fn routes_line(routes: Routes) -> String {
    format!(
        "Text={:<6} Value={:<6} Legacy={:<6} Name={:<6}{}",
        routes.text_pattern,
        routes.value_pattern,
        routes.legacy,
        routes.name,
        if routes.skipped { " (一部スキップ)" } else { "" }
    )
}

fn print_descent(label: &str, d: &Descent) {
    println!(
        "    {label:<18} 要素={:<5} 深さ={:<3} 編集可={:<4} Text型={:<5} {:>7}ms{}{}",
        d.nodes,
        d.max_depth_seen,
        d.editable_nodes,
        d.text_nodes,
        d.elapsed.as_millis(),
        if d.node_limit_hit { " [要素上限で打ち切り]" } else { "" },
        if d.depth_limit_hit { " [深さ上限で打ち切り]" } else { "" },
    );
    println!("      全体   : {}", routes_line(d.best));
    println!("      編集可 : {}", routes_line(d.best_editable));
    match d.focused {
        Some(routes) => println!("      focus  : {}", routes_line(routes)),
        None => println!("      focus  : (キーボードフォーカスを持つ要素は木の中に無し)"),
    }
}

// ---------------------------------------------------------------------------
// 診断テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 調べる相手。起動していないものは黙って飛ばす。
    const EXES: &[&str] = &[
        "chrome.exe",
        "claude.exe",
        "opencode.exe",
        "code.exe",
        "typeless.exe",
        "msedge.exe",
    ];

    /// 純ロジックの自己点検 (実機不要)。`Routes` の合成が壊れていないこと。
    #[test]
    fn routes_merge_takes_the_max_of_each_route() {
        let mut a = Routes {
            text_pattern: 10,
            value_pattern: 0,
            legacy: 3,
            name: 5,
            skipped: false,
        };
        a.merge_max(Routes {
            text_pattern: 2,
            value_pattern: 40,
            legacy: 1,
            name: 5,
            skipped: true,
        });
        assert_eq!(a.text_pattern, 10);
        assert_eq!(a.value_pattern, 40);
        assert_eq!(a.legacy, 3);
        assert_eq!(a.name, 5);
        assert!(a.skipped);
        assert_eq!(a.best(), 40);
    }

    #[test]
    fn an_empty_routes_reads_nothing() {
        assert_eq!(Routes::default().best(), 0);
    }

    /// **本題**: HWND 起点で UIA の木を降りて、Chrome / Electron の
    /// 入力欄の内容が読めるかを見る。
    ///
    /// 既存の `screen/win32.rs::read_window` が `EnumChildWindows`
    /// (深さ 1) しか見ていないのに対し、ここは `IUIAutomationTreeWalker`
    /// で**UIA の木そのもの**を降りる。Control / Raw の両ビューと、
    /// `FindAll(TreeScope_Subtree)` の 3 経路を比べる。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture spike_tree_descend`
    #[test]
    #[ignore = "実機の他アプリに依存する診断"]
    fn spike_tree_descend() {
        let Some(_com) = ComGuard::new() else {
            panic!("COM を初期化できない");
        };
        let automation = new_automation().expect("UIA を生成できる");

        let targets = targets(EXES);
        println!("対象ウィンドウ: {} 件", targets.len());
        assert!(!targets.is_empty(), "対象アプリが 1 つも起動していない");

        for target in &targets {
            // SAFETY: automation / hwnd は有効。
            let Ok(root) =
                (unsafe { automation.ElementFromHandle(HWND(target.hwnd as *mut _)) })
            else {
                println!("[{}] hwnd={:#x}: 要素を取得できない", target.exe, target.hwnd);
                continue;
            };
            println!("\n[{}] hwnd={:#x}", target.exe, target.hwnd);

            // SAFETY: automation は有効。
            if let Ok(walker) = unsafe { automation.ControlViewWalker() } {
                print_descent("ControlView", &descend(&walker, &root));
            }
            // SAFETY: automation は有効。
            if let Ok(walker) = unsafe { automation.RawViewWalker() } {
                print_descent("RawView", &descend(&walker, &root));
            }
            print_descent("FindAll(Subtree)", &find_all_probe(&automation, &root));
        }
    }

    /// **遅延有効化の実測**。Chromium は a11y を要求されて初めて非同期に
    /// ツリーを作る。一度読んで 0 でも、待てば読めるようになるのか。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture spike_lazy_activation`
    #[test]
    #[ignore = "実機の他アプリに依存する診断。10 秒ほどかかる"]
    fn spike_lazy_activation() {
        let Some(_com) = ComGuard::new() else {
            panic!("COM を初期化できない");
        };
        let automation = new_automation().expect("UIA を生成できる");
        let targets = targets(EXES);
        assert!(!targets.is_empty(), "対象アプリが 1 つも起動していない");

        // 待ち時間の刻み。0 は「最初の 1 回」。
        let waits_ms = [0u64, 200, 300, 500, 1_000, 2_000, 4_000];

        for target in &targets {
            // SAFETY: automation / hwnd は有効。
            let Ok(root) =
                (unsafe { automation.ElementFromHandle(HWND(target.hwnd as *mut _)) })
            else {
                continue;
            };
            // SAFETY: automation は有効。
            let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
                continue;
            };
            println!("\n[{}] hwnd={:#x}", target.exe, target.hwnd);
            let mut cumulative = 0u64;
            for wait in waits_ms {
                if wait > 0 {
                    std::thread::sleep(Duration::from_millis(wait));
                    cumulative += wait;
                }
                let d = descend(&walker, &root);
                println!(
                    "    +{cumulative:>5}ms 後: 要素={:<5} 編集可={:<4} 最大文字数={:<6} 走査={:>6}ms",
                    d.nodes,
                    d.editable_nodes,
                    d.best.best(),
                    d.elapsed.as_millis()
                );
            }
        }
    }

    /// **イベント購読**。`UIA_Text_TextChangedEventId` と FocusChanged を
    /// 購読し、(a) 購読自体が Chromium の a11y 有効化を誘発するか、
    /// (b) イベントが実際に飛んでくるか、を見る。
    ///
    /// 購読前後で同じ木を降り、要素数と読める文字数の変化を比べる。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture spike_event_subscription`
    #[test]
    #[ignore = "実機の他アプリに依存する診断。20 秒ほどかかる"]
    fn spike_event_subscription() {
        let Some(_com) = ComGuard::new() else {
            panic!("COM を初期化できない");
        };
        let automation = new_automation().expect("UIA を生成できる");
        let targets = targets(EXES);
        assert!(!targets.is_empty(), "対象アプリが 1 つも起動していない");

        // 購読前のスナップショット。
        println!("== 購読前 ==");
        let mut before: Vec<(String, usize, usize)> = Vec::new();
        for target in &targets {
            // SAFETY: automation / hwnd は有効。
            let Ok(root) =
                (unsafe { automation.ElementFromHandle(HWND(target.hwnd as *mut _)) })
            else {
                continue;
            };
            // SAFETY: automation は有効。
            let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
                continue;
            };
            let d = descend(&walker, &root);
            println!(
                "    {:<14} hwnd={:#x} 要素={:<5} 最大文字数={:<6}",
                target.exe,
                target.hwnd,
                d.nodes,
                d.best.best()
            );
            before.push((target.exe.clone(), d.nodes, d.best.best()));
        }

        EVENTS.store(0, Ordering::SeqCst);
        FOCUS_EVENTS.store(0, Ordering::SeqCst);
        FIRST_EVENT_MS.store(u64::MAX, Ordering::SeqCst);
        SUBSCRIBED_AT_MS.store(now_ms(), Ordering::SeqCst);

        // 各ウィンドウの部分木に TextChanged を張る。
        let handler: IUIAutomationEventHandler = CountingHandler.into();
        let mut subscribed = 0;
        for target in &targets {
            // SAFETY: automation / hwnd は有効。
            let Ok(root) =
                (unsafe { automation.ElementFromHandle(HWND(target.hwnd as *mut _)) })
            else {
                continue;
            };
            // SAFETY: 引数はすべて有効。cacherequest は None。
            let result = unsafe {
                automation.AddAutomationEventHandler(
                    UIA_Text_TextChangedEventId,
                    &root,
                    TreeScope_Subtree,
                    None,
                    &handler,
                )
            };
            match result {
                Ok(()) => subscribed += 1,
                Err(e) => println!("    購読失敗 {}: {e:?}", target.exe),
            }
        }

        // FocusChanged はデスクトップ全体に 1 本。
        let focus_handler: IUIAutomationFocusChangedEventHandler = FocusHandler.into();
        // SAFETY: automation / focus_handler は有効。
        let focus_ok = unsafe { automation.AddFocusChangedEventHandler(None, &focus_handler) };
        println!(
            "\nTextChanged 購読: {subscribed}/{} 件、FocusChanged 購読: {}",
            targets.len(),
            if focus_ok.is_ok() { "成功" } else { "失敗" }
        );
        println!("15 秒待ちます。この間に対象アプリの入力欄で文字を打つとイベントが出ます。");

        // 5 秒おきに集計を出す。
        for step in 1..=3 {
            std::thread::sleep(Duration::from_secs(5));
            println!(
                "    +{}s TextChanged={} FocusChanged={}",
                step * 5,
                EVENTS.load(Ordering::SeqCst),
                FOCUS_EVENTS.load(Ordering::SeqCst)
            );
        }

        let first = FIRST_EVENT_MS.load(Ordering::SeqCst);
        println!(
            "\n最初の TextChanged までの時間: {}",
            if first == u64::MAX {
                "(1 通も来なかった)".to_string()
            } else {
                format!("{first} ms")
            }
        );

        // 購読後のスナップショット。誘発されていれば要素数か文字数が増える。
        println!("\n== 購読後 ==");
        for (i, target) in targets.iter().enumerate() {
            // SAFETY: automation / hwnd は有効。
            let Ok(root) =
                (unsafe { automation.ElementFromHandle(HWND(target.hwnd as *mut _)) })
            else {
                continue;
            };
            // SAFETY: automation は有効。
            let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
                continue;
            };
            let d = descend(&walker, &root);
            let (_, was_nodes, was_chars) = before.get(i).cloned().unwrap_or_default();
            println!(
                "    {:<14} hwnd={:#x} 要素={:<5} (前 {:<5}) 最大文字数={:<6} (前 {:<6})",
                target.exe,
                target.hwnd,
                d.nodes,
                was_nodes,
                d.best.best(),
                was_chars
            );
        }

        // SAFETY: automation は有効。張ったハンドラを全部外す。
        unsafe {
            let _ = automation.RemoveAllEventHandlers();
        }
    }

    /// **入力欄 1 つ 1 つの読み返し**。`spike_tree_descend` は経路ごとの
    /// 最大値しか出さないので、「本文が読めた」のか「入力欄の中身が
    /// 読めた」のかが区別できない。ここでは Document / Edit 型の要素を
    /// **1 つずつ**並べ、それぞれの文字数と `HasKeyboardFocus` を出す。
    ///
    /// 既知の長さの文字列を入力欄に入れてから走らせると、その数字が
    /// そのまま出るかで**読み返しの正確さ**が確かめられる。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture spike_editable_readback`
    #[test]
    #[ignore = "実機の他アプリに依存する診断"]
    fn spike_editable_readback() {
        let Some(_com) = ComGuard::new() else {
            panic!("COM を初期化できない");
        };
        let automation = new_automation().expect("UIA を生成できる");
        let targets = targets(EXES);
        assert!(!targets.is_empty(), "対象アプリが 1 つも起動していない");

        for target in &targets {
            // SAFETY: automation / hwnd は有効。
            let Ok(root) =
                (unsafe { automation.ElementFromHandle(HWND(target.hwnd as *mut _)) })
            else {
                continue;
            };
            // SAFETY: automation は有効。
            let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
                continue;
            };

            let mut rows: Vec<String> = Vec::new();
            let mut stack: Vec<(IUIAutomationElement, usize)> = vec![(root.clone(), 0)];
            let mut nodes = 0usize;
            let mut truncated = false;
            let started = Instant::now();
            while let Some((element, depth)) = stack.pop() {
                if nodes >= MAX_NODES {
                    truncated = true;
                    break;
                }
                nodes += 1;
                // SAFETY: element は有効。
                let control_type = unsafe { element.CurrentControlType() }
                    .unwrap_or(UIA_CONTROLTYPE_ID(0));
                if control_type == UIA_DocumentControlTypeId
                    || control_type == UIA_EditControlTypeId
                {
                    let routes = probe_element(&element);
                    // SAFETY: element は有効。
                    let focused = unsafe { element.CurrentHasKeyboardFocus() }
                        .map(|b| b.as_bool())
                        .unwrap_or(false);
                    // SAFETY: element は有効。クラス名は実装名であって
                    // 利用者の入力内容ではないので出してよい。
                    let class = unsafe { element.CurrentClassName() }
                        .map(|c| c.to_string())
                        .unwrap_or_default();
                    let class: String = class.chars().take(24).collect();
                    rows.push(format!(
                        "      {:<9} 深さ={:<3} {:<26} {} {}",
                        if control_type == UIA_DocumentControlTypeId {
                            "Document"
                        } else {
                            "Edit"
                        },
                        depth,
                        class,
                        routes_line(routes),
                        if focused { "<- focus" } else { "" }
                    ));
                }
                if depth >= MAX_DEPTH {
                    continue;
                }
                // SAFETY: walker / element は有効。
                let Ok(mut child) = (unsafe { walker.GetFirstChildElement(&element) }) else {
                    continue;
                };
                loop {
                    stack.push((child.clone(), depth + 1));
                    // SAFETY: child は有効。
                    match unsafe { walker.GetNextSiblingElement(&child) } {
                        Ok(next) => child = next,
                        Err(_) => break,
                    }
                }
            }

            println!(
                "
[{}] hwnd={:#x} 要素={nodes} {}ms{}",
                target.exe,
                target.hwnd,
                started.elapsed().as_millis(),
                if truncated { " [要素上限で打ち切り]" } else { "" }
            );
            if rows.is_empty() {
                println!("      (Document / Edit 型の要素なし)");
            }
            for row in rows {
                println!("{row}");
            }
        }
    }

    /// **遅延有効化を、まっさらな Chromium で測る**。
    ///
    /// 既に走っている Chrome は誰か (スクリーンリーダ、この診断自身) の
    /// せいで a11y が有効になっている可能性があり、「初回 0 文字」を
    /// 再現できない。ここでは専用の `--user-data-dir` で**新しいブラウザ
    /// プロセスを起動**し、窓が出た瞬間から 50ms 刻みで木を降りて、
    /// 「何 ms 後に何文字読めるようになったか」を測る。
    ///
    /// 環境変数:
    /// - `NOX_SPIKE_PAGE` … 開く HTML のパス (必須)。既知の長さの入力欄を
    ///   持つページを用意しておくと、読み返しの正確さも同時に測れる。
    /// - `NOX_SPIKE_CHROME` … chrome.exe のパス (既定は標準の場所)。
    ///
    /// 起動した Chrome は**このテストが最後に終了させる**。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture spike_fresh_chrome_activation`
    #[test]
    #[ignore = "Chrome を専用プロファイルで起動する診断"]
    fn spike_fresh_chrome_activation() {
        let Ok(page) = std::env::var("NOX_SPIKE_PAGE") else {
            println!("NOX_SPIKE_PAGE が無いので飛ばします");
            return;
        };
        let chrome = std::env::var("NOX_SPIKE_CHROME").unwrap_or_else(|_| {
            r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_string()
        });
        let profile = std::env::temp_dir().join(format!("nox-spike-{}", std::process::id()));

        let Some(_com) = ComGuard::new() else {
            panic!("COM を初期化できない");
        };
        let automation = new_automation().expect("UIA を生成できる");

        let known: Vec<isize> = targets(&["chrome.exe"]).iter().map(|t| t.hwnd).collect();

        let mut child = std::process::Command::new(&chrome)
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--new-window")
            .arg(format!("file:///{}", page.replace('\\', "/")))
            .spawn()
            .expect("chrome を起動できる");
        let launched = Instant::now();

        // 新しい窓が出るのを待つ。
        let mut hwnd = 0isize;
        while launched.elapsed() < Duration::from_secs(30) {
            if let Some(found) = targets(&["chrome.exe"])
                .into_iter()
                .find(|t| !known.contains(&t.hwnd))
            {
                hwnd = found.hwnd;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(hwnd != 0, "新しい Chrome の窓が見つからない");
        let window_at = launched.elapsed();
        println!("窓が出るまで: {} ms (hwnd={hwnd:#x})", window_at.as_millis());
        println!(
            "{:<10} {:<8} {:<8} {:<8} 各 Edit の文字数",
            "経過ms", "要素", "編集可", "最大文字"
        );

        // SAFETY: automation は有効。
        let walker = unsafe { automation.ControlViewWalker() }.expect("walker を取れる");
        let mut first_readable: Option<u128> = None;
        loop {
            let since_window = launched.elapsed();
            if since_window > Duration::from_secs(20) {
                break;
            }
            // SAFETY: automation / hwnd は有効。窓が閉じていれば Err。
            let Ok(root) = (unsafe { automation.ElementFromHandle(HWND(hwnd as *mut _)) }) else {
                println!("{:<10} 要素を取得できない", since_window.as_millis());
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            let d = descend(&walker, &root);

            // Edit 型ごとの文字数 (中身は出さない)。
            let mut edits: Vec<usize> = Vec::new();
            let mut stack = vec![(root.clone(), 0usize)];
            let mut seen = 0usize;
            while let Some((element, depth)) = stack.pop() {
                if seen >= MAX_NODES {
                    break;
                }
                seen += 1;
                // SAFETY: element は有効。
                let ct = unsafe { element.CurrentControlType() }.unwrap_or(UIA_CONTROLTYPE_ID(0));
                if ct == UIA_EditControlTypeId {
                    edits.push(probe_element(&element).best());
                }
                if depth >= MAX_DEPTH {
                    continue;
                }
                // SAFETY: walker / element は有効。
                let Ok(mut c) = (unsafe { walker.GetFirstChildElement(&element) }) else {
                    continue;
                };
                loop {
                    stack.push((c.clone(), depth + 1));
                    // SAFETY: c は有効。
                    match unsafe { walker.GetNextSiblingElement(&c) } {
                        Ok(n) => c = n,
                        Err(_) => break,
                    }
                }
            }
            edits.sort_unstable();
            println!(
                "{:<10} {:<8} {:<8} {:<8} {:?}",
                since_window.as_millis(),
                d.nodes,
                d.editable_nodes,
                d.best.best(),
                edits
            );
            if first_readable.is_none() && d.editable_nodes > 0 && d.best.best() > 0 {
                first_readable = Some(since_window.as_millis());
            }
            // 読めるようになったら数回だけ余分に見て終わる。
            if let Some(at) = first_readable {
                if since_window.as_millis() > at + 600 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        match first_readable {
            Some(ms) => println!("\n初めて入力欄が読めた: 起動から {ms} ms"),
            None => println!("\n20 秒待っても入力欄は読めなかった"),
        }

        // 自分で起動した Chrome だけを落とす。
        let _ = child.kill();
        let _ = child.wait();
    }

    /// **イベントが本当に飛んでくるか**を、自分で起動した Chrome で測る。
    ///
    /// `spike_event_subscription` は他人のアプリ頼みで「打たなければ
    /// 何も起きない」。ここでは 500ms ごとに textarea が 1 文字伸びる
    /// ページ (`NOX_SPIKE_TICKER`) を専用プロファイルの Chrome で開き、
    ///
    /// - `UIA_Text_TextChangedEventId` が飛んでくるか / 何 ms 遅れか
    /// - 購読と実際の読み値が食い違わないか (読み値も並行して見る)
    ///
    /// を測る。起動した Chrome はこのテストが落とす。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture spike_events_fresh_chrome`
    #[test]
    #[ignore = "Chrome を専用プロファイルで起動する診断。15 秒ほどかかる"]
    fn spike_events_fresh_chrome() {
        let Ok(page) = std::env::var("NOX_SPIKE_TICKER") else {
            println!("NOX_SPIKE_TICKER が無いので飛ばします");
            return;
        };
        let chrome = std::env::var("NOX_SPIKE_CHROME").unwrap_or_else(|_| {
            r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_string()
        });
        let profile = std::env::temp_dir().join(format!("nox-spike-ev-{}", std::process::id()));

        let Some(_com) = ComGuard::new() else {
            panic!("COM を初期化できない");
        };
        let automation = new_automation().expect("UIA を生成できる");
        let known: Vec<isize> = targets(&["chrome.exe"]).iter().map(|t| t.hwnd).collect();

        let mut child = std::process::Command::new(&chrome)
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--new-window")
            .arg(format!("file:///{}", page.replace('\\', "/")))
            .spawn()
            .expect("chrome を起動できる");

        let launched = Instant::now();
        let mut hwnd = 0isize;
        while launched.elapsed() < Duration::from_secs(30) {
            if let Some(found) = targets(&["chrome.exe"])
                .into_iter()
                .find(|t| !known.contains(&t.hwnd))
            {
                hwnd = found.hwnd;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(hwnd != 0, "新しい Chrome の窓が見つからない");

        // SAFETY: automation / hwnd は有効。
        let root = unsafe { automation.ElementFromHandle(HWND(hwnd as *mut _)) }
            .expect("要素を取れる");

        EVENTS.store(0, Ordering::SeqCst);
        FOCUS_EVENTS.store(0, Ordering::SeqCst);
        FIRST_EVENT_MS.store(u64::MAX, Ordering::SeqCst);
        SUBSCRIBED_AT_MS.store(now_ms(), Ordering::SeqCst);

        let handler: IUIAutomationEventHandler = CountingHandler.into();
        // SAFETY: 引数はすべて有効。
        let subscribed = unsafe {
            automation.AddAutomationEventHandler(
                UIA_Text_TextChangedEventId,
                &root,
                TreeScope_Subtree,
                None,
                &handler,
            )
        };
        println!(
            "TextChanged 購読: {}",
            if subscribed.is_ok() { "成功" } else { "失敗" }
        );

        // SAFETY: automation は有効。
        let walker = unsafe { automation.ControlViewWalker() }.expect("walker を取れる");
        for step in 1..=10 {
            std::thread::sleep(Duration::from_secs(1));
            // SAFETY: automation / hwnd は有効。
            let chars = match unsafe { automation.ElementFromHandle(HWND(hwnd as *mut _)) } {
                Ok(root) => descend(&walker, &root).best_editable.best(),
                Err(_) => 0,
            };
            println!(
                "    +{step}s TextChanged={} FocusChanged={} 入力欄の文字数={chars}",
                EVENTS.load(Ordering::SeqCst),
                FOCUS_EVENTS.load(Ordering::SeqCst),
            );
        }

        let first = FIRST_EVENT_MS.load(Ordering::SeqCst);
        println!(
            "最初の TextChanged: {}",
            if first == u64::MAX {
                "(1 通も来なかった)".to_string()
            } else {
                format!("購読から {first} ms")
            }
        );

        // SAFETY: automation は有効。
        unsafe {
            let _ = automation.RemoveAllEventHandlers();
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// `GetFocusedElement` 経路 (deep context が使っている経路) を
    /// 一定間隔でサンプリングし、**どのプロセスの何が読めるか**を見る。
    ///
    /// テストを走らせている端末自身にフォーカスがあるのが普通なので、
    /// 待っている間に Chrome / Electron の入力欄をクリックすると
    /// そのアプリの数字が出る。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture spike_focused_element`
    #[test]
    #[ignore = "実機のフォーカスに依存する診断。15 秒ほどかかる"]
    fn spike_focused_element() {
        let Some(_com) = ComGuard::new() else {
            panic!("COM を初期化できない");
        };
        let automation = new_automation().expect("UIA を生成できる");

        println!("15 回サンプリングします (1 秒おき)。読みたい入力欄をクリックしてください。");
        println!("{:<6} {:<16} 経路別文字数", "秒", "プロセス");
        for second in 1..=15 {
            std::thread::sleep(Duration::from_secs(1));
            let started = Instant::now();
            // SAFETY: automation は有効。フォーカスが無ければ Err。
            let Ok(element) = (unsafe { automation.GetFocusedElement() }) else {
                println!("{second:<6} {:<16} (フォーカス要素なし)", "-");
                continue;
            };
            // Chromium の中の要素は HWND を持たない (`CurrentNativeWindowHandle`
            // が 0) ので、プロセス ID から実行ファイル名を引く。
            // SAFETY: element は有効。取れなければ 0。
            let pid = unsafe { element.CurrentProcessId() }.unwrap_or(0) as u32;
            let exe = exe_name_of_pid(pid).unwrap_or_else(|| "(不明)".to_string());
            let routes = probe_element(&element);
            println!(
                "{second:<6} {exe:<16} {}  {:>5}ms",
                routes_line(routes),
                started.elapsed().as_millis()
            );
        }
    }
}
