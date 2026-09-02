//! 画面質問モード — モニタ 1 枚分の画面を「資料」として集める。
//!
//! deep context ([`crate::context`]) が「今書いている文章の続き」を掴むための
//! **前景 1 要素**を読むのに対し、こちらは「画面に何が写っているか」を
//! **モニタ単位**で読む。発話は書き取ってほしい文章ではなく画面への質問
//! ({「左画面に出ているセッション一覧を出して」}) なので、資料が 1 ウィンドウ
//! 分では答えられない — どのウィンドウを指しているかを選ぶのは LLM 側であり、
//! そのためには候補が全部載っている必要がある。
//!
//! # 二段構え: UIA 優先、駄目ならスクリーンショット
//!
//! UIA で読めるならそちらが圧倒的に良い。文字が文字のまま取れるので、
//! 一覧を一覧として返せるし、画像より桁違いに軽い。しかし実測
//! (design.md「Q1: deep context」の表) のとおり、**Chrome・Electron 系・
//! エクスプローラーはタイトル相当しか返さない**。これらを諦めると、
//! この機能が一番使われるであろう相手 (エディタ・ターミナル・チャット) で
//! 「画面が読めません」しか返せなくなる。
//!
//! そこで、1 つでも読めなかったウィンドウがあれば**モニタ 1 枚のスクリーン
//! ショットを 1 枚だけ**添える ([`needs_screenshot`])。ウィンドウごとに
//! `PrintWindow` する案は採らなかった:
//!
//! - GPU 合成のアプリでは `PrintWindow` が黒い矩形を返すことがあり、
//!   「撮れたのに何も写っていない」という一番たちの悪い失敗をする
//! - 撮れたとしても、重なり順・位置関係が失われる。「左の画面の」「奥の窓の」
//!   といった指示語は、**ユーザーが見ているとおりの 1 枚**でないと解けない
//! - 枚数が増えれば増えるほど送信量と待ち時間が増える
//!
//! # プライバシー (design.md R1 の一段先)
//!
//! deep context は「フォーカスしている要素」だけだったが、こちらは
//! **モニタに写っているものを全部**クラウドへ送る。したがって:
//!
//! - **既定は無効**、かつ**専用ホットキーを割り当てるまで動かない**
//!   (設定 1 つの ON/OFF だと、既存のキーに相乗りして誤発火する)
//! - 自分自身のウィンドウは読まない。とくに**オーバーレイは録音中まさに
//!   画面に出ている**ので、除外しないと自分の小窓を読んで返すことになる
//! - **画面から読んだ資料と、それを元にした回答は履歴に一切残さない**
//!   (質問文も画面の語を含みがちなので同じ扱い)。唯一の例外は
//!   「聞き取れなかったときのユーザー自身の音声」で、これは R4 と M4 の
//!   所有権原則を守るために他の用途と同じく退避する
//!   ([`crate::answer_screen_question`] の doc に理由がある)
//! - **ログにも中身を出さない**。出すのは件数・文字数・経路だけ。
//!   ウィンドウタイトルも出さない — 資料としては LLM に渡すが、
//!   ファイル名や相手の名前が入りうるものをログファイルに残す理由はない

use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::context::ContextSource;

/// 走査全体の時間予算。
///
/// deep context の 300ms より桁で長い。理由は非対称で、あちらは
/// **録音開始をブロックする**のに対し、こちらは録音と**並行して**走り、
/// 待つのは STT が終わったあと ([`ScanHandle::wait`]) だからである。
/// つまりこの時間はユーザーの体感待ち時間には (STT より短い限り) 乗らない。
pub const SCAN_BUDGET: Duration = Duration::from_secs(4);

/// 資料に載せるウィンドウ数の上限。
///
/// Z オーダー順 (手前から) に採るので、溢れるのは奥にある窓になる。
/// 手前に見えているものほど質問の対象である可能性が高い。
pub const MAX_WINDOWS: usize = 12;

/// 1 ウィンドウあたりのテキスト上限 (文字)。
pub const MAX_TEXT_PER_WINDOW: usize = 4_000;

/// 資料テキスト全体の上限 (文字)。
pub const MAX_TOTAL_TEXT: usize = 24_000;

/// 「UIA で読めた」と認めるテキストの最小文字数。
///
/// # ここの緩さ / 厳しさが機能の成否を決める
///
/// 緩くする (1 文字でも取れたら成功扱い) と、ウィンドウタイトルだけが
/// 返ってきた Chrome を「読めた」と数えてスクリーンショットへ落ちず、
/// **質問に答えられない**。厳しくする (数百文字を要求) と、短い一覧や
/// ダイアログしか出ていない画面でも毎回画像を送ることになる。
///
/// 実測 (design.md Q1) では、読めないアプリの戻り値は
/// `ElementName` 経由のウィンドウタイトル — 日本語で数文字〜数十文字。
/// タイトルは超えるが本文としては話にならない、という位置に線を引く。
/// あわせて**経路も見る** ([`is_usable_text`]): `ElementName` しか
/// 取れていないなら、何文字あろうと「読めた」とは認めない。
pub const MIN_USABLE_CHARS: usize = 40;

/// 資料に載せる最小のウィンドウサイズ (論理ピクセル)。
///
/// これ未満は通知トーストや細い常駐バーで、質問の対象になりえない。
pub const MIN_WINDOW_WIDTH: i32 = 200;
pub const MIN_WINDOW_HEIGHT: i32 = 120;

/// スクリーンショットの長辺上限。
///
/// 4K をそのまま送ると PNG で数 MB になり、送信だけで数秒かかる。
/// Gemini 側もタイル分割して読むので、原寸で送る利点は小さい。
pub const MAX_IMAGE_EDGE: u32 = 1_536;

/// 走査 1 回分の成果。
///
/// **`Clone` は付けない。** 画像を含みうるので、うっかり複製されると
/// 数 MB がそのまま増える。持ち回すときは所有権を渡すこと。
#[derive(Debug, Default)]
pub struct ScreenScan {
    /// 読めたウィンドウ (Z オーダー順、手前が先)。
    pub windows: Vec<ScannedWindow>,
    /// モニタ 1 枚のスクリーンショット (UIA で全部読めたなら `None`)。
    pub screenshot: Option<Screenshot>,
    /// どのモニタをどういう理由で選んだか (ログ用)。
    pub monitor: MonitorPick,
    /// 資料が 1 つも作れなかった理由。**空を黙って返さないための欄**
    /// (design.md「0 件と欠測を混同しない」)。
    pub failure: Option<String>,
}

impl ScreenScan {
    /// 質問に答えられるだけの資料があるか。
    pub fn has_material(&self) -> bool {
        self.screenshot.is_some() || self.windows.iter().any(|w| !w.text.trim().is_empty())
    }

    /// 取得できなかったときの空の成果。
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            failure: Some(reason.into()),
            ..Self::default()
        }
    }
}

/// 資料として載せる 1 ウィンドウ分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedWindow {
    /// ウィンドウタイトル。**LLM には渡すがログには出さない。**
    pub title: String,
    /// 実行ファイル名 (`chrome.exe` など)。
    pub process: String,
    /// モニタ内でのおおよその位置 (「左半分の上寄り」など)。
    ///
    /// 「左の画面の」「右下の窓の」という指示語を解くのに要る。
    pub position: String,
    /// 読めた本文。読めなかったウィンドウは空。
    pub text: String,
    /// どの経路で取れたか (診断用)。
    pub route: ContextSource,
}

/// スクリーンショット 1 枚。
#[derive(Debug)]
pub struct Screenshot {
    /// PNG バイト列。
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// どのモニタを、なぜ選んだか。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MonitorPick {
    /// 前景ウィンドウのあるモニタ。**通常はこれ**。
    Foreground,
    /// 前景が無いのでマウスカーソルのあるモニタにした。
    Cursor,
    /// 前景もカーソルも取れないので主モニタにした。
    Primary,
    /// まだ決まっていない (失敗した走査)。
    #[default]
    None,
}

impl MonitorPick {
    pub fn label(self) -> &'static str {
        match self {
            MonitorPick::Foreground => "前景ウィンドウのモニタ",
            MonitorPick::Cursor => "カーソルのあるモニタ",
            MonitorPick::Primary => "主モニタ",
            MonitorPick::None => "未選択",
        }
    }
}

// ---------------------------------------------------------------------------
// 判定ロジック (純関数)。Win32 に触らないのでここだけは単体テストできる。
// ---------------------------------------------------------------------------

/// 走査候補の 1 ウィンドウ。Win32 から集めた生の属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowCandidate {
    pub hwnd: isize,
    pub title: String,
    pub process_id: u32,
    /// `IsWindowVisible`。
    pub visible: bool,
    /// `IsIconic` (最小化)。
    pub minimized: bool,
    /// DWM の `DWMWA_CLOAKED`。仮想デスクトップの別ページや、
    /// 起動済みだが表示されていない UWP がここに引っかかる。
    pub cloaked: bool,
    /// `WS_EX_TOOLWINDOW`。Alt+Tab に出ない補助窓。
    pub tool_window: bool,
    pub width: i32,
    pub height: i32,
    /// 対象モニタ上にあるか (`MonitorFromWindow` の一致)。
    pub on_target_monitor: bool,
}

impl Default for WindowCandidate {
    fn default() -> Self {
        Self {
            hwnd: 1,
            title: "窓".to_string(),
            process_id: 1234,
            visible: true,
            minimized: false,
            cloaked: false,
            tool_window: false,
            width: 800,
            height: 600,
            on_target_monitor: true,
        }
    }
}

/// 候補を読むか、読まないならなぜか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDecision {
    Scan,
    Skip(SkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 自分自身のウィンドウ (メインウィンドウとオーバーレイ)。
    OwnWindow,
    OtherMonitor,
    NotVisible,
    Minimized,
    /// DWM から見て隠されている (別の仮想デスクトップ等)。
    Cloaked,
    ToolWindow,
    /// タイトルが無い。ほぼ全てが不可視のメッセージ専用ウィンドウ。
    Untitled,
    TooSmall,
    /// 手前のウィンドウに**完全に覆われて**いて、画面に 1 px も見えていない。
    ///
    /// この窓はユーザーの目にも入らず、スクリーンショットにも写らない。
    /// **資料に載せる理由が無い**うえ、載せると「読めなかった窓」として
    /// [`needs_screenshot`] を発火させ、**他の全ウィンドウが正しく読めて
    /// いてもモニタ全体の画像をクラウドへ送らせてしまう** (実機で再現:
    /// 常駐アプリの窓 1 つのために画像が送られた)。
    Covered,
}

impl SkipReason {
    pub fn label(self) -> &'static str {
        match self {
            SkipReason::OwnWindow => "自分自身",
            SkipReason::OtherMonitor => "別モニタ",
            SkipReason::NotVisible => "非表示",
            SkipReason::Minimized => "最小化",
            SkipReason::Cloaked => "DWM が隠している",
            SkipReason::ToolWindow => "ツールウィンドウ",
            SkipReason::Untitled => "タイトル無し",
            SkipReason::TooSmall => "小さすぎる",
            SkipReason::Covered => "手前の窓に完全に隠れている",
        }
    }
}

/// このウィンドウを資料に載せてよいか。
///
/// # 自分自身を必ず外す
///
/// 判定を**プロセス ID**で行うのが要点。ウィンドウラベルや HWND の
/// 一覧で除外すると、ウィンドウが 1 つ増えるたびに漏れる。とくに
/// **オーバーレイは録音中まさに画面に出ている**ので、漏れると
/// 「録音中…」と書かれた自分の小窓を読んで返すことになる。
/// プロセス単位で切れば、今後どんなウィンドウを足しても自動的に外れる。
pub fn scan_decision(candidate: &WindowCandidate, own_process_id: u32) -> ScanDecision {
    // 自分自身が最優先。以降の条件をすり抜ける可能性を一切残さない。
    if candidate.process_id == own_process_id {
        return ScanDecision::Skip(SkipReason::OwnWindow);
    }
    if !candidate.on_target_monitor {
        return ScanDecision::Skip(SkipReason::OtherMonitor);
    }
    if !candidate.visible {
        return ScanDecision::Skip(SkipReason::NotVisible);
    }
    if candidate.minimized {
        return ScanDecision::Skip(SkipReason::Minimized);
    }
    if candidate.cloaked {
        return ScanDecision::Skip(SkipReason::Cloaked);
    }
    if candidate.tool_window {
        return ScanDecision::Skip(SkipReason::ToolWindow);
    }
    if candidate.title.trim().is_empty() {
        return ScanDecision::Skip(SkipReason::Untitled);
    }
    if candidate.width < MIN_WINDOW_WIDTH || candidate.height < MIN_WINDOW_HEIGHT {
        return ScanDecision::Skip(SkipReason::TooSmall);
    }
    ScanDecision::Scan
}

/// UIA の戻り値を「本文として読めた」と認めるか。
///
/// 文字数だけでなく**経路も見る**。`ElementName` はウィンドウタイトル相当が
/// 返ってくる経路で、長いタイトル (ブラウザのページ名 + サイト名など) は
/// 平気で 40 文字を超える。経路で切らないと、タイトルの長い Chrome だけが
/// 「読めた」ことになり、スクリーンショットが添付されない。
pub fn is_usable_text(text: &str, route: ContextSource) -> bool {
    if !matches!(
        route,
        ContextSource::TextPattern | ContextSource::ValuePattern | ContextSource::Legacy
    ) {
        return false;
    }
    text.trim().chars().count() >= MIN_USABLE_CHARS
}

/// もうこれ以上 UIA の木を降りる必要が無い、と言い切れる読み取りか。
///
/// # なぜ [`is_usable_text`] で止めてはいけないか
///
/// 木の下降は**浅いところから**進む ([`win32`] の幅優先)。Chromium の
/// ウィンドウで最初に当たる本文っぽい要素は**アドレスバー**で、URL は
/// `ValuePattern` から平気で 40 文字以上返る。`is_usable_text` で打ち切ると、
/// その窓は「URL が読めたので読めた窓」に分類され、**記事本文にも
/// スクリーンショットにも辿り着かないまま資料が確定する** — 今より悪い。
///
/// そこで打ち切りの線は別に引く: **`TextPattern` から本文相当の量が
/// 取れたときだけ**。ページ本文・エディタ・ターミナルはここに当たり、
/// アドレスバーや検索欄は当たらない。当たらなければ予算まで木を降り続け、
/// その間に見つけた最良の読み取りが残る (壊れるのは速度だけで、正しさではない)。
pub fn is_conclusive_text(text: &str, route: ContextSource) -> bool {
    matches!(route, ContextSource::TextPattern)
        && text.trim().chars().count() >= CONCLUSIVE_CHARS
}

/// [`is_conclusive_text`] の閾値。
///
/// [`MIN_USABLE_CHARS`] (40) より一桁大きい。アドレスバーの URL・タブ名・
/// パンくずは 40 は超えても 400 は超えない。逆に「読み終わってよい」と
/// 言うにはページ 1 画面ぶんは欲しい。
pub const CONCLUSIVE_CHARS: usize = 400;

/// ローカル OCR の結果を「読めた」と認めるか。
///
/// [`is_usable_text`] とは**別の関数**にしてある。OCR は UIA の経路では
/// ないので、あちらの `route` の並びに混ぜると「UIA で読めた」の意味が
/// 濁る。閾値は同じ [`MIN_USABLE_CHARS`] を使う — 数文字しか起こせなかった
/// ウィンドウ (ほぼ画像・グラフの窓) は、無理に文字にするより
/// スクリーンショットに任せた方が答えられる。ここを緩めると、
/// **「OCR で埋まったから画像は要らない」と判断して手がかりを失う**。
pub fn is_usable_ocr_text(text: &str) -> bool {
    text.trim().chars().count() >= MIN_USABLE_CHARS
}

/// OCR にかける最小の一辺 (px)。これ未満は起こせる文字がほぼ無い。
pub const OCR_MIN_EDGE: i32 = 64;

/// 2 つの矩形 `(x, y, 幅, 高さ)` が 1 px でも重なるか (純関数)。
pub fn rects_overlap(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    if aw <= 0 || ah <= 0 || bw <= 0 || bh <= 0 {
        return false;
    }
    ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
}

/// `outer` が `inner` を**完全に**含むか (純関数)。
pub fn rect_contains(outer: (i32, i32, i32, i32), inner: (i32, i32, i32, i32)) -> bool {
    let (ox, oy, ow, oh) = outer;
    let (ix, iy, iw, ih) = inner;
    if ow <= 0 || oh <= 0 || iw <= 0 || ih <= 0 {
        return false;
    }
    ox <= ix && oy <= iy && ox + ow >= ix + iw && oy + oh >= iy + ih
}

/// 手前のウィンドウにどれだけ覆われているか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occlusion {
    /// どの手前の窓とも重なっていない。**OCR してよい唯一の状態**。
    Clear,
    /// 一部が覆われている。
    Partial,
    /// 手前の窓 1 つに完全に覆われ、画面に 1 px も見えていない。
    Covered,
}

/// ある窓が、**それより手前にある窓たち**にどう覆われているかを決める (純関数)。
///
/// `fronts` は Z オーダーで手前にある窓の矩形 (`EnumWindows` の順に積んだもの)。
///
/// # 3 状態を分ける理由
///
/// - [`Occlusion::Covered`]: **候補から外す。** ユーザーの目にも入らず、
///   スクリーンショットにも写らないので、資料に載せる理由が無い。しかも
///   載せると「読めなかった窓」として [`needs_screenshot`] を発火させ、
///   **他の全ウィンドウが正しく読めていてもモニタ全体の画像を送らせる**。
///   実機で再現した挙動そのもので、この機能の目的 (送るものを減らす) を
///   常駐アプリの窓 1 つが丸ごと無効化していた
/// - [`Occlusion::Partial`]: **資料には載せるが OCR はしない。** OCR は
///   画面から見えているものを撮るので、覆われた部分には手前の窓の中身が
///   写る。それを奥の窓のテキストとして載せると**中身が別のウィンドウの
///   ものにすり替わる** — 「読めなかった」よりたちが悪い (LLM は嘘を
///   資料として扱う)。この窓は従来どおりモニタ全体の画像に任せる
/// - [`Occlusion::Clear`]: OCR してよい
///
/// # 完全被覆は「手前の窓 1 つ」で見る
///
/// 複数の窓が寄り集まって覆っている場合 (和集合による被覆) は見ない。
/// 判定に矩形の集合演算が要るうえ、**間違えたときに「見えている窓を
/// 資料から落とす」方向へ倒れる**。1 つの窓に完全に含まれる、という
/// 保守的な条件だけを見る — 最大化ウィンドウが常駐窓を覆う実運用の
/// ケースはこれで拾える。
pub fn occlusion(rect: (i32, i32, i32, i32), fronts: &[(i32, i32, i32, i32)]) -> Occlusion {
    let mut partial = false;
    for front in fronts {
        if rect_contains(*front, rect) {
            return Occlusion::Covered;
        }
        if rects_overlap(rect, *front) {
            partial = true;
        }
    }
    if partial {
        Occlusion::Partial
    } else {
        Occlusion::Clear
    }
}

/// 読み取り候補 `found` が、いま最良の `best` を置き換えるべきか (純関数)。
///
/// **順序は「本文として読めたか → 経路のランク → 長さ」。**
///
/// # なぜランクを先に見てはいけないか
///
/// ランクだけを先に見ると、**20 文字の `TextPattern` が 105 文字の
/// `ValuePattern` に勝つ**。本文が `ValuePattern` でしか出ないアプリ
/// (スパイクの実測: Typeless.exe は Text=0 / Value=105) では、木のどこかに
/// ある無関係な小さい入力欄が本文を追い出し、**読めているのに
/// 「読めない窓」として画像送りになる**。
///
/// 「読めた」を先に見れば、読めているものが読めていないものに負けない。
/// 読めたもの同士では従来どおりランク (本文に近い経路) を優先し、
/// 同ランクなら長い方を採る。
pub fn is_better_read(found: (&str, ContextSource), best: (&str, ContextSource)) -> bool {
    let key = |(text, route): (&str, ContextSource)| {
        (
            is_usable_text(text, route),
            route_rank(route),
            text.chars().count(),
        )
    };
    key(found) > key(best)
}

/// 経路の望ましさ。大きいほど本文に近い。
pub fn route_rank(source: ContextSource) -> u8 {
    match source {
        ContextSource::TextPattern => 4,
        ContextSource::ValuePattern => 3,
        // MSAA 経由。UIA ネイティブの 2 経路が両方 0 を返す相手向けの保険で、
        // 返るのは本文なので `ElementName` (ラベル) より上に置く。
        ContextSource::Legacy => 2,
        ContextSource::ElementName => 1,
        _ => 0,
    }
}

/// ウィンドウ矩形をモニタ矩形で切り取る (純関数)。
///
/// 画面外へはみ出した部分を `BitBlt` で撮ると、ドライバによっては
/// 黒や前回の内容が返る。**撮る前に画面の中へ収める。**
/// 収めた結果が [`OCR_MIN_EDGE`] 未満なら `None` (OCR しない)。
pub fn clip_to_monitor(
    win: (i32, i32, i32, i32),
    monitor: (i32, i32, i32, i32),
) -> Option<(i32, i32, i32, i32)> {
    let (wx, wy, ww, wh) = win;
    let (mx, my, mw, mh) = monitor;
    let left = wx.max(mx);
    let top = wy.max(my);
    let right = (wx + ww).min(mx + mw);
    let bottom = (wy + wh).min(my + mh);
    let (w, h) = (right - left, bottom - top);
    (w >= OCR_MIN_EDGE && h >= OCR_MIN_EDGE).then_some((left, top, w, h))
}

/// `GetDIBits` の BGRA (下から上) を、上から下の BGRA へ並べ替える。
///
/// あわせて**アルファを 255 で埋める**。32bpp `BI_RGB` のアルファは
/// 未定義で、実際には 0 が入る。`SoftwareBitmap` は Bgra8 を
/// **乗算済みアルファ**として解釈するので、0 のまま渡すと画像全体が
/// 透明 = 真っ黒になり、**OCR が必ず 0 文字を返す** (「動いているのに
/// 何も読めない」という切り分けの難しい失敗になる)。
pub fn bgra_bottom_up_to_top_down(src: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || src.len() < w * h * 4 {
        return None;
    }
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let src_row = (h - 1 - y) * w * 4;
        let dst_row = y * w * 4;
        out[dst_row..dst_row + w * 4].copy_from_slice(&src[src_row..src_row + w * 4]);
        for x in 0..w {
            out[dst_row + x * 4 + 3] = 255;
        }
    }
    Some(out)
}

/// OCR の単語をつなぐ区切り (純関数)。
///
/// `Windows.Media.Ocr` は認識結果を**単語単位**で返す。英語のように
/// 空白で区切る言語はそのまま空白でつなげばよいが、**日本語・中国語を
/// 空白でつなぐと「本 文 が こ の よ う に」なる** — 資料としては読めるが、
/// 固有名詞が割れてクラウド側の理解を確実に落とす。言語タグで分ける。
pub fn ocr_word_separator(language_tag: &str) -> &'static str {
    let tag = language_tag.to_ascii_lowercase();
    // 前方一致で見るのは、実際に返るのが "ja-JP" / "zh-Hans-CN" のような
    // 地域つきのタグだから。"ja" 完全一致で書くと日本語で空白が入る。
    // 韓国語は分かち書きするので**入れない**。「CJK だから」で 3 言語を
    // まとめると、韓国語だけ単語が全部くっつく。
    if tag.starts_with("ja") || tag.starts_with("zh") {
        ""
    } else {
        " "
    }
}

/// OCR の単語列を 1 行につなぐ (純関数)。
///
/// 区切りが空 (日本語・中国語) のときも、**両隣が ASCII 英数字なら空白を
/// 入れる**。入れないと日本語の行に混ざった英単語が
/// 「Windows Update を実行」→「WindowsUpdateを実行」のように潰れる。
/// 固有名詞・コマンド名・エラーコードは画面質問でまさに訊かれる対象なので、
/// ここが潰れると答えの精度に直接効く。
pub fn join_ocr_words(words: &[String], separator: &str) -> String {
    let mut out = String::new();
    for word in words {
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            let left_ascii = out.chars().next_back().is_some_and(is_ascii_wordish);
            let right_ascii = word.chars().next().is_some_and(is_ascii_wordish);
            if separator.is_empty() && left_ascii && right_ascii {
                out.push(' ');
            } else {
                out.push_str(separator);
            }
        }
        out.push_str(word);
    }
    out
}

/// 空白を挟むべき「語の一部」に見える ASCII か。
///
/// 記号は入れない。`(Windows)` のような囲みや `-` で切れた語の間に
/// 空白を差し込むと、今度はそちらが壊れる。
fn is_ascii_wordish(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// スクリーンショットを撮るべきか。
///
/// **1 つでも資料に残らなかったウィンドウがあれば撮る。** 質問が来る前に
/// 走査は終わっているので、「どのウィンドウについて訊かれるか」はこの時点
/// では分からない。読めなかった窓が 1 つでも残っていれば、それが質問の対象で
/// ある可能性がある以上、画像で保険をかけるほかない。
///
/// 対象ウィンドウが 0 個のときも撮る。デスクトップだけが見えている状況で
/// 「今なにが出てる?」と訊かれたら、答えは画像にしか無い。
///
/// # `unscanned` を別引数で受ける理由
///
/// `readable` は**読もうとした窓**の結果しか持たない。走査は
/// [`MAX_WINDOWS`] 件と時間予算のどちらでも打ち切られるので、
/// 「読んだ 12 件は全部読めたが、13 件目以降は手つかず」が普通に起きる。
/// 読んだ分だけを見て判断すると、そこで画像が付かず、**打ち切られた窓に
/// ついて訊かれるとテキストにも画像にも資料が無い**状態になる。
/// 「1 つでも読めなかったら画像で保険」という原則は、
/// **読まなかった窓にも等しく適用する**。
pub fn needs_screenshot(readable: &[bool], unscanned: usize) -> bool {
    readable.is_empty() || unscanned > 0 || readable.iter().any(|ok| !ok)
}

/// モニタ内での位置をことばにする。
///
/// 「左画面の」「右下の」といった指示語を LLM が解けるようにするための欄。
/// 画像を添えるときは画像から分かるが、**UIA だけで完結したときは
/// これが唯一の位置情報**になる。
pub fn describe_position(
    win: (i32, i32, i32, i32),
    monitor: (i32, i32, i32, i32),
) -> String {
    let (mx, my, mw, mh) = monitor;
    if mw <= 0 || mh <= 0 {
        return "位置不明".to_string();
    }
    let (wx, wy, ww, wh) = win;
    // 窓の中心がモニタのどの区画にあるか。左右 3 分割 × 上下 3 分割。
    // 端の座標ではなく中心で見るのは、画面いっぱいの窓が「左」に
    // 分類されるのを避けるため。
    let cx = wx + ww / 2 - mx;
    let cy = wy + wh / 2 - my;
    let horizontal = match cx * 3 / mw.max(1) {
        i if i <= 0 => "左",
        1 => "中央",
        _ => "右",
    };
    let vertical = match cy * 3 / mh.max(1) {
        i if i <= 0 => "上",
        1 => "中段",
        _ => "下",
    };
    // ほぼ画面いっぱいなら区画より「全体」の方が正確。
    if ww * 100 >= mw * 80 && wh * 100 >= mh * 80 {
        return "画面ほぼ全体".to_string();
    }
    format!("{horizontal}{vertical}")
}

/// 長辺が `max_edge` に収まる縮小後サイズ。拡大はしない。
pub fn plan_scale(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest == 0 || max_edge == 0 || longest <= max_edge {
        return (width.max(1), height.max(1));
    }
    // 整数演算のみ。f32 を経由すると 4K などで 1px ずれることがある。
    let w = (width as u64 * max_edge as u64 / longest as u64).max(1) as u32;
    let h = (height as u64 * max_edge as u64 / longest as u64).max(1) as u32;
    (w, h)
}

/// BGRA (下から上、`GetDIBits` の既定) を RGB (上から下) へ、
/// 同時に `dw x dh` へ縮小する。
///
/// 縮小は**元画素の平均**で行う。間引き (nearest) にすると細い字の
/// 縦棒がまるごと消え、画像を送る意味そのものが無くなる。
pub fn downscale_bgra_bottom_up(
    src: &[u8],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
) -> Option<Vec<u8>> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return None;
    }
    // 32bpp なので行のパディングは無い (幅 × 4 が常に 4 の倍数)。
    if src.len() < (sw as usize) * (sh as usize) * 4 {
        return None;
    }
    let mut out = vec![0u8; (dw as usize) * (dh as usize) * 3];
    for dy in 0..dh {
        // 出力行が対応する入力行の範囲。
        let y0 = (dy as u64 * sh as u64 / dh as u64) as u32;
        let y1 = (((dy + 1) as u64 * sh as u64 / dh as u64) as u32).max(y0 + 1);
        for dx in 0..dw {
            let x0 = (dx as u64 * sw as u64 / dw as u64) as u32;
            let x1 = (((dx + 1) as u64 * sw as u64 / dw as u64) as u32).max(x0 + 1);
            let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
            for y in y0..y1.min(sh) {
                // GetDIBits は既定でボトムアップ。出力の 0 行目は入力の最終行。
                let src_row = (sh - 1 - y) as usize;
                for x in x0..x1.min(sw) {
                    let i = (src_row * sw as usize + x as usize) * 4;
                    b += src[i] as u64;
                    g += src[i + 1] as u64;
                    r += src[i + 2] as u64;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let o = ((dy as usize) * dw as usize + dx as usize) * 3;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
        }
    }
    Some(out)
}

/// RGB8 を PNG にする。
///
/// JPEG ではなく PNG なのは、資料の主たる内容が**文字**だから。
/// JPEG のブロックノイズは細い字を最初に壊す。
pub fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    if rgb.len() != (width as usize) * (height as usize) * 3 {
        return Err("画素数とバッファ長が合いません".to_string());
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG ヘッダを書けません: {e}"))?;
        writer
            .write_image_data(rgb)
            .map_err(|e| format!("PNG を書けません: {e}"))?;
    }
    Ok(out)
}

/// 資料テキスト全体が上限を超えないよう、後ろのウィンドウから削る。
///
/// 手前 (Z オーダーが上) のウィンドウを優先して残す。奥の窓は
/// ユーザーの視界から隠れている可能性が高く、質問の対象になりにくい。
pub fn cap_total_text(windows: &mut [ScannedWindow], max_total: usize) {
    let mut used = 0usize;
    let mut keep = 0usize;
    for window in windows.iter_mut() {
        let len = window.text.chars().count();
        if used + len <= max_total {
            used += len;
            keep += 1;
            continue;
        }
        // 途中まで入るなら頭から入る分だけ残す (画面は上から読むため、
        // deep context の「末尾を残す」とは逆向きにする)。
        let room = max_total.saturating_sub(used);
        if room >= MIN_USABLE_CHARS {
            window.text = window.text.chars().take(room).collect();
            keep += 1;
        }
        break;
    }
    // 残りは「タイトルだけ」にして落とす。行ごと消すと、そこに窓が
    // あったこと自体が伝わらなくなる (「一覧を出して」の答えから漏れる)。
    for window in windows.iter_mut().skip(keep) {
        window.text.clear();
    }
}

// ---------------------------------------------------------------------------
// 走査の起動と待ち合わせ
// ---------------------------------------------------------------------------

/// 走っている走査の**開始時刻** (プロセス起動からの ms、0 = 空き)。
///
/// [`crate::context::capture`] の `CAPTURE_IN_FLIGHT` と同じ理由で 1 本に
/// 制限する: 打ち切った走査は裏で走り続けるので、連続で録音されると
/// 返ってこない相手に対してスレッドが積み上がる。
///
/// # bool ではなく時刻を持つ理由
///
/// 走査スレッドの予算チェックは**ウィンドウとウィンドウの間**にしかない。
/// `ElementFromHandle` のような UIA 呼び出し 1 回そのものには打ち切りが
/// 無く、相手がハングしていれば返ってこない。単なる bool だと、その 1 本に
/// 掴まった時点で**フラグが立ちっぱなしになり、以後の画面質問が再起動まで
/// 全部失敗する**。時刻を持たせて [`STALE_AFTER`] を過ぎた占有を横取り
/// できるようにすれば、被害は「その間の 1 回が失敗する」で止まる。
///
/// # UIA 呼び出しごとに打ち切りを付けない理由
///
/// 「1 回の呼び出しを別スレッドへ投げて時間で見切る」を各呼び出しへ広げると、
/// **ウィンドウ 1 枚ごとにスレッドと COM の初期化が要る**。走査は十数枚を
/// 相手にするので通常時のコストが跳ね上がるうえ、見切ったスレッドはどのみち
/// 裏に残る (相手が返さない限り止める手段は無い — スレッドの強制終了は
/// COM の状態を壊す)。つまり**スレッドが漏れること自体は防げない**。
///
/// 防げるのは「漏れたスレッドが機能全体を人質に取ること」だけで、それには
/// この横取りで足りる。ハングしたアプリを閉じるまでの間、画面質問は
/// [`STALE_AFTER`] に 1 回まで失敗しうるが、失敗は必ずトーストで見える。
/// **「無言で壊れる」から「見える形で遅くなる」へ落とせている**ので、
/// ここで止めてよいと判断した。
static SCAN_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

/// 占有をこれだけ過ぎたら、返ってこないものとみなして横取りする。
///
/// [`SCAN_BUDGET`] より十分長くする。予算どおりに終わる走査を
/// 追い越してしまうと、1 本制限そのものが意味を失う。
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// 走査の占有を試みる (純関数)。
///
/// `current` が [`SCAN_IN_FLIGHT`] の現在値、`now_ms` が今の時刻。
/// 取れたら**書き込むべき新しい値**を返す。取れなければ `None`。
///
/// `now_ms` が 0 になりうる (プロセス起動直後) と「空き」と区別が付かなく
/// なるので、印は必ず 1 以上にする。
pub fn claim_scan(current: u64, now_ms: u64, stale_after_ms: u64) -> Option<u64> {
    let claim = now_ms.max(1);
    if current == 0 {
        return Some(claim);
    }
    // 返ってこない走査に占有されたまま、というのが唯一許さない状態。
    (now_ms.saturating_sub(current) >= stale_after_ms).then_some(claim)
}

/// プロセス起動からの経過 ms。[`claim_scan`] の時刻源。
///
/// 壁時計 (`SystemTime`) は使わない。時刻同期やサマータイムで巻き戻ると、
/// 「60 秒過ぎたか」の判定が狂って占有が永久に外れなくなる。
pub fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 占有を手放してよいか (純関数)。
///
/// 自分の印がまだ載っているときだけ 0 に戻す。**横取りされたあとに
/// 0 を書くと、走り出したばかりの新しい走査の占有を消してしまう**。
pub fn may_release_scan(current: u64, claim: u64) -> bool {
    current == claim
}

/// 走った走査の待ち受け口。
///
/// `Receiver` を握るだけ。**録音開始をブロックしない**のがこの型の存在理由で、
/// 走査は録音と並行して進み、[`ScanHandle::wait`] は STT が終わったあとに
/// 呼ばれる。ユーザーが最も嫌う「話し始めるまで待たされる」が起きない。
///
/// **`Clone` は付けない。** 複製できると 2 か所が同じ走査を待てることに
/// なるが、結果は 1 回しか流れないので片方は必ず「タイムアウト」を
/// 受け取る。[`ScreenScan`] に `Clone` を付けていないのと同じ理由で、
/// **持ち回すときは所有権を渡す**。
#[derive(Debug)]
pub struct ScanHandle {
    rx: Receiver<ScreenScan>,
    started: Instant,
}

impl ScanHandle {
    /// Win32 側の走査スレッドから作る。
    fn new(rx: Receiver<ScreenScan>, started: Instant) -> Self {
        Self { rx, started }
    }

    /// 結果を待つ。予算を使い切ったら理由つきの空を返す。
    pub fn wait(self) -> ScreenScan {
        let elapsed = self.started.elapsed();
        let remaining = SCAN_BUDGET.saturating_sub(elapsed);
        match self.rx.recv_timeout(remaining) {
            Ok(scan) => scan,
            Err(_) => {
                log::info!(
                    "画面の読み取りが {} ms を超えたので打ち切りました",
                    SCAN_BUDGET.as_millis()
                );
                ScreenScan::failed(format!(
                    "画面の読み取りが {} 秒以内に終わりませんでした",
                    SCAN_BUDGET.as_secs()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn window(text: &str) -> ScannedWindow {
        ScannedWindow {
            title: "タイトル".to_string(),
            process: "app.exe".to_string(),
            position: "左上".to_string(),
            text: text.to_string(),
            route: ContextSource::TextPattern,
        }
    }

    // --- 走査対象の選別 ---

    #[test]
    fn a_normal_window_is_scanned() {
        assert_eq!(
            scan_decision(&WindowCandidate::default(), 9999),
            ScanDecision::Scan
        );
    }

    #[test]
    fn our_own_windows_are_never_scanned() {
        // オーバーレイは録音中まさに画面に出ている。除外が漏れると
        // 「録音中…」と書かれた自分の小窓を読んで返すことになる。
        let overlay = WindowCandidate {
            process_id: 42,
            title: "nox-voice overlay".to_string(),
            width: 320,
            height: 72,
            ..Default::default()
        };
        assert_eq!(
            scan_decision(&overlay, 42),
            ScanDecision::Skip(SkipReason::OwnWindow)
        );
        // メインウィンドウ (大きい・タイトルあり) も同じ理由で外れる。
        let main = WindowCandidate {
            process_id: 42,
            title: "nox-voice".to_string(),
            ..Default::default()
        };
        assert_eq!(
            scan_decision(&main, 42),
            ScanDecision::Skip(SkipReason::OwnWindow)
        );
    }

    #[test]
    fn own_windows_lose_even_when_every_other_rule_would_reject_first() {
        // 「自分自身」を最優先に判定していることの固定。順番が入れ替わると、
        // 除外はされても理由がログに嘘を書く。
        let weird = WindowCandidate {
            process_id: 42,
            visible: false,
            minimized: true,
            cloaked: true,
            tool_window: true,
            title: String::new(),
            width: 1,
            height: 1,
            on_target_monitor: false,
            ..Default::default()
        };
        assert_eq!(
            scan_decision(&weird, 42),
            ScanDecision::Skip(SkipReason::OwnWindow)
        );
    }

    #[test]
    fn windows_on_other_monitors_are_skipped() {
        let other = WindowCandidate {
            on_target_monitor: false,
            ..Default::default()
        };
        assert_eq!(
            scan_decision(&other, 1),
            ScanDecision::Skip(SkipReason::OtherMonitor)
        );
    }

    #[test]
    fn hidden_minimized_and_cloaked_windows_are_skipped() {
        for (candidate, expected) in [
            (
                WindowCandidate {
                    visible: false,
                    ..Default::default()
                },
                SkipReason::NotVisible,
            ),
            (
                WindowCandidate {
                    minimized: true,
                    ..Default::default()
                },
                SkipReason::Minimized,
            ),
            (
                WindowCandidate {
                    cloaked: true,
                    ..Default::default()
                },
                SkipReason::Cloaked,
            ),
            (
                WindowCandidate {
                    tool_window: true,
                    ..Default::default()
                },
                SkipReason::ToolWindow,
            ),
        ] {
            assert_eq!(
                scan_decision(&candidate, 1),
                ScanDecision::Skip(expected),
                "{expected:?}"
            );
        }
    }

    #[test]
    fn untitled_and_tiny_windows_are_skipped() {
        let untitled = WindowCandidate {
            title: "   ".to_string(),
            ..Default::default()
        };
        assert_eq!(
            scan_decision(&untitled, 1),
            ScanDecision::Skip(SkipReason::Untitled)
        );
        let tiny = WindowCandidate {
            width: MIN_WINDOW_WIDTH - 1,
            ..Default::default()
        };
        assert_eq!(
            scan_decision(&tiny, 1),
            ScanDecision::Skip(SkipReason::TooSmall)
        );
        let short = WindowCandidate {
            height: MIN_WINDOW_HEIGHT - 1,
            ..Default::default()
        };
        assert_eq!(
            scan_decision(&short, 1),
            ScanDecision::Skip(SkipReason::TooSmall)
        );
    }

    #[test]
    fn every_skip_reason_has_a_label() {
        for reason in [
            SkipReason::OwnWindow,
            SkipReason::OtherMonitor,
            SkipReason::NotVisible,
            SkipReason::Minimized,
            SkipReason::Cloaked,
            SkipReason::ToolWindow,
            SkipReason::Untitled,
            SkipReason::TooSmall,
            SkipReason::Covered,
        ] {
            assert!(!reason.label().is_empty(), "{reason:?}");
        }
    }

    #[test]
    fn every_monitor_pick_has_a_label() {
        for pick in [
            MonitorPick::Foreground,
            MonitorPick::Cursor,
            MonitorPick::Primary,
            MonitorPick::None,
        ] {
            assert!(!pick.label().is_empty(), "{pick:?}");
        }
    }

    // --- 「UIA で読めた」の判定 ---

    #[test]
    fn a_long_window_title_does_not_count_as_readable() {
        // ここが要点。ブラウザのタイトルは平気で 40 文字を超えるので、
        // 文字数だけで判定すると Chrome が「読めた」ことになり、
        // スクリーンショットが添付されなくなる。
        let title = "設計ドキュメントの読み方について — 社内 Wiki — Google Chrome の長いタイトル";
        assert!(title.chars().count() > MIN_USABLE_CHARS);
        assert!(!is_usable_text(title, ContextSource::ElementName));
    }

    #[test]
    fn a_real_body_from_a_text_pattern_counts_as_readable() {
        let body = "あ".repeat(MIN_USABLE_CHARS);
        assert!(is_usable_text(&body, ContextSource::TextPattern));
        assert!(is_usable_text(&body, ContextSource::ValuePattern));
        // MSAA 経由も本文。ここを外すと、UIA ネイティブが無いアプリだけが
        // 「読めなかった」ことになってスクリーンショット送りになる。
        assert!(is_usable_text(&body, ContextSource::Legacy));
    }

    #[test]
    fn a_too_short_body_does_not_count_as_readable() {
        let body = "あ".repeat(MIN_USABLE_CHARS - 1);
        assert!(!is_usable_text(&body, ContextSource::TextPattern));
        assert!(!is_usable_text("   ", ContextSource::TextPattern));
    }

    #[test]
    fn unreadable_routes_never_count() {
        for route in [
            ContextSource::ElementName,
            // OCR は UIA の経路ではない。ここに混ぜて「UIA で読めた」に
            // すると、UIA の成否を測る指標が意味を失う。
            ContextSource::Ocr,
            ContextSource::Unavailable,
            ContextSource::PasswordSkipped,
            ContextSource::TimedOut,
            ContextSource::Disabled,
        ] {
            assert!(
                !is_usable_text(&"あ".repeat(500), route),
                "{route:?} を読めた扱いにしている"
            );
        }
    }

    // --- 木の下降をどこで打ち切るか ---

    #[test]
    fn an_address_bar_does_not_end_the_descent() {
        // ここが要点。Chromium の木を浅い方から降りると、本文より先に
        // アドレスバーに当たる。URL は 40 文字を平気で超えるので
        // `is_usable_text` で打ち切ると、記事本文にもスクリーンショットにも
        // 辿り着かないまま「読めた窓」として資料が確定する。
        let url = "https://example.com/articles/2026/09/very-long-slug-for-a-page?ref=nav";
        assert!(url.chars().count() > MIN_USABLE_CHARS);
        assert!(is_usable_text(url, ContextSource::ValuePattern));
        assert!(!is_conclusive_text(url, ContextSource::ValuePattern));
        assert!(!is_conclusive_text(url, ContextSource::TextPattern));
    }

    #[test]
    fn a_page_body_ends_the_descent() {
        let body = "あ".repeat(CONCLUSIVE_CHARS);
        assert!(is_conclusive_text(&body, ContextSource::TextPattern));
        // TextPattern 以外は「本文をまるごと持っている」保証が無いので
        // 打ち切らない (量が同じでも降り続ける)。
        assert!(!is_conclusive_text(&body, ContextSource::ValuePattern));
        assert!(!is_conclusive_text(&body, ContextSource::Legacy));
        assert!(!is_conclusive_text(&body, ContextSource::ElementName));
    }

    #[test]
    fn the_descent_threshold_is_well_above_the_usable_one() {
        // 逆転すると「読めた」より先に打ち切りが来て、上の防御が消える。
        const { assert!(CONCLUSIVE_CHARS > MIN_USABLE_CHARS) };
    }

    // --- ローカル OCR ---

    #[test]
    fn ocr_text_is_judged_by_its_own_rule() {
        // OCR は UIA の経路ではない。`is_usable_text` に混ぜると
        // 「UIA で読めた」の意味が濁る。
        let body = "あ".repeat(MIN_USABLE_CHARS);
        assert!(is_usable_ocr_text(&body));
        assert!(!is_usable_text(&body, ContextSource::Ocr));
    }

    #[test]
    fn a_handful_of_characters_is_not_worth_calling_readable() {
        // ほぼ画像の窓から数文字だけ起こして「読めた」にすると、
        // 画像が付かないまま手がかりを失う。
        assert!(!is_usable_ocr_text(&"あ".repeat(MIN_USABLE_CHARS - 1)));
        assert!(!is_usable_ocr_text("   "));
    }

    // --- 手前の窓に覆われた窓の扱い ---

    #[test]
    fn a_fully_covered_window_is_dropped_from_the_material() {
        // これがレビューで実機再現した不具合。常駐アプリの窓 1 つが
        // 最大化ウィンドウの裏に完全に隠れているだけで「読めなかった窓」に
        // 数えられ、他の全ウィンドウが正しく読めていてもモニタ全体の画像が
        // クラウドへ送られていた。**見えない窓は資料に載せる理由が無い。**
        let maximized = (0, 0, 1920, 1080);
        let hidden = (400, 300, 300, 200);
        assert_eq!(occlusion(hidden, &[maximized]), Occlusion::Covered);
        // 縁がぴったり一致していても「覆われている」。
        assert_eq!(occlusion(maximized, &[maximized]), Occlusion::Covered);
    }

    #[test]
    fn a_partly_covered_window_stays_but_is_not_ocred() {
        // 一部だけ覆われた窓は**ユーザーに見えている**ので資料には残す。
        // ただし OCR すると覆われた部分に手前の窓の中身が写り、
        // 中身が別のウィンドウのものにすり替わる。
        let front = (0, 0, 800, 600);
        assert_eq!(occlusion((700, 500, 800, 600), &[front]), Occlusion::Partial);
    }

    #[test]
    fn an_unobstructed_window_is_clear() {
        let front = (0, 0, 800, 600);
        assert_eq!(occlusion((800, 0, 800, 600), &[front]), Occlusion::Clear);
        assert_eq!(occlusion((0, 0, 800, 600), &[]), Occlusion::Clear);
    }

    #[test]
    fn full_coverage_wins_over_a_partial_overlap_elsewhere() {
        // 順序に依存して「一部重なり」で確定してしまうと、
        // 完全に隠れた窓が資料に残り、画像を強制する。
        let nibble = (0, 0, 100, 100);
        let cover = (0, 0, 1920, 1080);
        let window = (50, 50, 300, 200);
        assert_eq!(occlusion(window, &[nibble, cover]), Occlusion::Covered);
        assert_eq!(occlusion(window, &[cover, nibble]), Occlusion::Covered);
    }

    #[test]
    fn coverage_needs_a_single_window_that_contains_it() {
        // 2 つの窓が寄り集まって覆っている場合は「完全被覆」と見ない。
        // 間違えると**見えている窓を資料から落とす**方向へ倒れる。
        let left = (0, 0, 500, 1080);
        let right = (500, 0, 500, 1080);
        assert_eq!(occlusion((100, 100, 800, 200), &[left, right]), Occlusion::Partial);
    }

    #[test]
    fn containment_is_not_confused_with_mere_overlap() {
        assert!(rect_contains((0, 0, 100, 100), (10, 10, 10, 10)));
        assert!(!rect_contains((0, 0, 100, 100), (90, 90, 20, 20)));
        // 空の矩形は誰も含まないし、誰にも含まれない。
        assert!(!rect_contains((0, 0, 0, 100), (0, 0, 0, 100)));
    }

    // --- どの読み取りを採るか ---

    #[test]
    fn a_readable_value_pattern_beats_an_unreadable_text_pattern() {
        // スパイクの実測: Typeless.exe は Text=0 / Value=105。木のどこかに
        // ある無関係な小さい入力欄 (TextPattern・20 字) が本文を追い出すと、
        // **読めているのに「読めない窓」として画像送りになる**。
        let body = ("あ".repeat(105), ContextSource::ValuePattern);
        let scrap = ("あ".repeat(20), ContextSource::TextPattern);
        assert!(is_better_read(
            (&body.0, body.1),
            (&scrap.0, scrap.1)
        ));
        assert!(!is_better_read((&scrap.0, scrap.1), (&body.0, body.1)));
    }

    #[test]
    fn among_readable_reads_the_route_still_decides() {
        let text = "あ".repeat(200);
        let value = "い".repeat(500);
        // どちらも読めているなら、本文に近い経路を採る (長さより優先)。
        assert!(is_better_read(
            (&text, ContextSource::TextPattern),
            (&value, ContextSource::ValuePattern)
        ));
    }

    #[test]
    fn among_equal_routes_the_longer_read_wins() {
        let short = "あ".repeat(50);
        let long = "あ".repeat(500);
        assert!(is_better_read(
            (&long, ContextSource::TextPattern),
            (&short, ContextSource::TextPattern)
        ));
        assert!(!is_better_read(
            (&short, ContextSource::TextPattern),
            (&long, ContextSource::TextPattern)
        ));
    }

    #[test]
    fn a_title_never_displaces_a_body() {
        let title = "設計ドキュメントの読み方について — 社内 Wiki — Google Chrome";
        let body = "あ".repeat(MIN_USABLE_CHARS);
        assert!(!is_better_read(
            (title, ContextSource::ElementName),
            (&body, ContextSource::Legacy)
        ));
    }

    #[test]
    fn overlapping_windows_are_never_ocred() {
        let front = (0, 0, 800, 600);
        // 1 px でも重なれば駄目。覆われた部分には手前の窓の中身が写り、
        // それを奥の窓のテキストとして載せると中身がすり替わる。
        assert!(rects_overlap(front, (799, 599, 400, 300)));
        assert!(rects_overlap(front, (100, 100, 50, 50)), "内包も重なり");
        // 辺が接するだけなら重なっていない。
        assert!(!rects_overlap(front, (800, 0, 400, 300)));
        assert!(!rects_overlap(front, (0, 600, 400, 300)));
        // 空の矩形は誰とも重ならない (0 除算ではなく素通しにする)。
        assert!(!rects_overlap(front, (100, 100, 0, 300)));
    }

    #[test]
    fn window_rects_are_clipped_into_the_monitor() {
        let monitor = (0, 0, 1920, 1080);
        // 左と上へはみ出した窓。
        assert_eq!(
            clip_to_monitor((-200, -100, 800, 600), monitor),
            Some((0, 0, 600, 500))
        );
        // 完全に画面内ならそのまま。
        assert_eq!(
            clip_to_monitor((100, 100, 800, 600), monitor),
            Some((100, 100, 800, 600))
        );
        // 2 枚目のモニタでも原点を跨がない。
        let second = (1920, 0, 1920, 1080);
        assert_eq!(
            clip_to_monitor((1800, 0, 400, 600), second),
            Some((1920, 0, 280, 600))
        );
    }

    #[test]
    fn a_sliver_of_a_window_is_not_worth_ocring() {
        let monitor = (0, 0, 1920, 1080);
        assert_eq!(clip_to_monitor((-790, 0, 800, 600), monitor), None);
        assert_eq!(clip_to_monitor((5000, 0, 800, 600), monitor), None);
    }

    #[test]
    fn the_ocr_buffer_is_flipped_and_made_opaque() {
        // 2x2。ボトムアップなので入力の 1 行目が出力の下端になる。
        let mut src = vec![0u8; 2 * 2 * 4];
        for x in 0..2 {
            src[x * 4] = 255; // 下端: 青
            src[(2 + x) * 4 + 2] = 255; // 上端: 赤
        }
        let out = bgra_bottom_up_to_top_down(&src, 2, 2).expect("変換できる");
        // 出力の 0 行目 (上端) は赤。B=0 G=0 R=255。
        assert_eq!(&out[0..3], &[0, 0, 255]);
        assert_eq!(&out[8..11], &[255, 0, 0], "下端が青のまま来ていない");
        // アルファは全部 255。0 のままだと乗算済みアルファとして
        // 真っ黒に解釈され、OCR が必ず 0 文字を返す。
        // (`chunks_exact` ではなく alpha バイトだけを直接 step_by で拾う:
        // clippy の `chunks_exact_to_as_chunks` は Rust 1.98 以降にしか無く、
        // それ未満のツールチェーンではこの書き方の方が両立する)
        assert!(out.iter().skip(3).step_by(4).all(|&alpha| alpha == 255));
    }

    #[test]
    fn a_truncated_ocr_buffer_is_rejected_instead_of_panicking() {
        assert!(bgra_bottom_up_to_top_down(&[0u8; 4], 100, 100).is_none());
        assert!(bgra_bottom_up_to_top_down(&[0u8; 16], 0, 2).is_none());
    }

    #[test]
    fn japanese_ocr_words_are_joined_without_spaces() {
        // 空白でつなぐと「本 文 が こ の よ う に」なり、固有名詞が割れる。
        assert_eq!(ocr_word_separator("ja-JP"), "");
        assert_eq!(ocr_word_separator("zh-Hans-CN"), "");
        // 地域つきタグで返るので、完全一致で書くと日本語に空白が入る。
        assert_eq!(ocr_word_separator("JA"), "");
    }

    #[test]
    fn western_ocr_words_keep_their_spaces() {
        assert_eq!(ocr_word_separator("en-US"), " ");
        assert_eq!(ocr_word_separator("de-DE"), " ");
        // 韓国語は分かち書きする。CJK でまとめると単語が全部くっつく。
        assert_eq!(ocr_word_separator("ko-KR"), " ");
        assert_eq!(ocr_word_separator(""), " ");
    }

    #[test]
    fn ascii_words_inside_a_japanese_line_keep_their_space() {
        // 「Windows Update を実行」が「WindowsUpdateを実行」になると、
        // 固有名詞・コマンド名・エラーコードが潰れる — 画面質問で
        // まさに訊かれる対象なので、答えの精度に直接効く。
        let words = words(&["Windows", "Update", "を", "実行"]);
        assert_eq!(join_ocr_words(&words, ""), "Windows Updateを実行");
    }

    #[test]
    fn japanese_words_are_still_joined_tightly() {
        let words = words(&["本", "文", "が", "この", "ように"]);
        assert_eq!(join_ocr_words(&words, ""), "本文がこのように");
    }

    #[test]
    fn symbols_do_not_gain_spaces() {
        // 囲みや区切り記号の隣に空白を差し込むと、今度はそちらが壊れる。
        let words = words(&["(", "Windows", ")", "の", "設定"]);
        assert_eq!(join_ocr_words(&words, ""), "(Windows)の設定");
    }

    #[test]
    fn a_space_separated_language_is_joined_as_before() {
        let words = words(&["Open", "the", "settings"]);
        assert_eq!(join_ocr_words(&words, " "), "Open the settings");
    }

    #[test]
    fn empty_words_do_not_leave_double_separators() {
        let words = words(&["Open", "", "settings"]);
        assert_eq!(join_ocr_words(&words, " "), "Open settings");
        assert_eq!(join_ocr_words(&[], " "), "");
    }

    // --- スクリーンショットの要否 ---

    #[test]
    fn a_fully_readable_monitor_needs_no_screenshot() {
        assert!(!needs_screenshot(&[true, true, true], 0));
    }

    #[test]
    fn one_unreadable_window_is_enough_to_take_a_screenshot() {
        assert!(needs_screenshot(&[true, false, true], 0));
    }

    #[test]
    fn an_empty_monitor_still_gets_a_screenshot() {
        // デスクトップだけが見えている状況で「今なにが出てる?」と
        // 訊かれたら、答えは画像にしか無い。
        assert!(needs_screenshot(&[], 0));
    }

    #[test]
    fn windows_we_never_got_to_also_force_a_screenshot() {
        // 走査は件数上限と時間予算のどちらでも打ち切られる。読んだ分が
        // 全部読めていても、**手つかずの窓が残っていれば画像は要る** —
        // そこで画像を付けないと、打ち切られた窓について訊かれたときに
        // テキストにも画像にも資料が無い。
        assert!(needs_screenshot(&[true, true, true], 1));
        assert!(needs_screenshot(&[true; 12], 5));
        // 手つかずが無ければ従来どおり。
        assert!(!needs_screenshot(&[true; 12], 0));
    }

    // --- 走査の占有 (返ってこない 1 本に永久に塞がれないこと) ---

    #[test]
    fn an_idle_slot_can_be_claimed() {
        assert_eq!(claim_scan(0, 5_000, 60_000), Some(5_000));
    }

    #[test]
    fn a_running_scan_blocks_a_new_one() {
        // 1 本制限そのもの。予算内で走っている走査は追い越さない。
        assert_eq!(claim_scan(5_000, 8_000, 60_000), None);
    }

    #[test]
    fn a_scan_that_never_returns_is_eventually_taken_over() {
        // UIA 呼び出し 1 回そのものには打ち切りが無い。ハングした相手に
        // 掴まったまま永久にフラグが立っていると、**以後の画面質問が
        // 再起動まで全部失敗する**。
        assert_eq!(claim_scan(5_000, 65_000, 60_000), Some(65_000));
        // ちょうど境界でも取れる。
        assert_eq!(claim_scan(5_000, 65_000 - 1, 60_000), None);
    }

    #[test]
    fn a_claim_is_never_zero() {
        // 0 は「空き」の意味。プロセス起動直後の now_ms = 0 と
        // 区別が付かないと、取ったつもりで空きのままになる。
        assert_eq!(claim_scan(0, 0, 60_000), Some(1));
    }

    #[test]
    fn only_the_owner_may_release_the_slot() {
        assert!(may_release_scan(5_000, 5_000));
        // 横取りされたあと。ここで 0 を書くと、走り出したばかりの
        // 新しい走査の占有を消してしまう。
        assert!(!may_release_scan(65_000, 5_000));
        assert!(!may_release_scan(0, 5_000));
    }

    #[test]
    fn the_stale_threshold_is_longer_than_the_budget() {
        // 予算どおりに終わる走査を追い越してしまうと、1 本制限が意味を失う。
        assert!(STALE_AFTER > SCAN_BUDGET);
    }

    #[test]
    fn the_clock_moves_forward_and_never_wraps_to_zero_twice() {
        let a = now_ms();
        let b = now_ms();
        assert!(b >= a, "時刻が巻き戻った");
    }

    // --- 位置の説明 ---

    #[test]
    fn a_left_half_window_is_described_as_left() {
        let monitor = (0, 0, 1920, 1080);
        assert!(describe_position((0, 0, 960, 1080), monitor).starts_with("左"));
        assert!(describe_position((960, 0, 960, 1080), monitor).starts_with("右"));
    }

    #[test]
    fn a_full_screen_window_is_not_called_left() {
        // 画面いっぱいの窓を「左上」と書くと、「左の窓の」という指示語が
        // 全画面アプリを指してしまう。
        let monitor = (0, 0, 1920, 1080);
        assert_eq!(describe_position((0, 0, 1920, 1080), monitor), "画面ほぼ全体");
    }

    #[test]
    fn positions_are_relative_to_the_monitor_origin() {
        // 2 枚目のモニタは原点がずれる。絶対座標で判定すると全部「右」になる。
        let second = (1920, 0, 1920, 1080);
        assert!(describe_position((1920, 0, 700, 500), second).starts_with("左"));
        assert!(describe_position((3100, 600, 700, 400), second).starts_with("右"));
    }

    #[test]
    fn a_zero_sized_monitor_does_not_panic() {
        assert_eq!(describe_position((0, 0, 10, 10), (0, 0, 0, 0)), "位置不明");
    }

    // --- 画像 ---

    #[test]
    fn scaling_keeps_the_aspect_ratio_and_never_upscales() {
        assert_eq!(plan_scale(3840, 2160, 1536), (1536, 864));
        assert_eq!(plan_scale(1080, 1920, 1536), (864, 1536));
        // 小さい画像は拡大しない (情報は増えないのに送信量だけ増える)。
        assert_eq!(plan_scale(800, 600, 1536), (800, 600));
        assert_eq!(plan_scale(0, 0, 1536), (1, 1));
    }

    #[test]
    fn downscaling_flips_the_bottom_up_bitmap() {
        // GetDIBits は既定でボトムアップ。反転を忘れると上下逆の画像を
        // 送ることになり、「上のペイン」という指示語が全部裏返る。
        // 2x2 の入力: 入力の最終行 (= 出力の 0 行目) を赤にする。
        let mut src = vec![0u8; 2 * 2 * 4];
        // row 0 (= 画像の下端) は青、row 1 (= 画像の上端) は赤。
        for x in 0..2 {
            let i = x * 4;
            src[i] = 255; // B
            let j = (2 + x) * 4;
            src[j + 2] = 255; // R
        }
        let out = downscale_bgra_bottom_up(&src, 2, 2, 2, 2).expect("縮小できる");
        // 出力の 0 行目 (上端) は赤。
        assert_eq!(&out[0..3], &[255, 0, 0]);
        // 出力の 1 行目 (下端) は青。
        assert_eq!(&out[6..9], &[0, 0, 255]);
    }

    #[test]
    fn downscaling_averages_instead_of_dropping_pixels() {
        // 4x1 の白黒縞を 2x1 へ。間引きなら 255/255、平均なら 127/127。
        // 間引きだと細い字の縦棒がまるごと消える。
        let mut src = vec![0u8; 4 * 4];
        for x in [0usize, 2] {
            let i = x * 4;
            src[i] = 255;
            src[i + 1] = 255;
            src[i + 2] = 255;
        }
        let out = downscale_bgra_bottom_up(&src, 4, 1, 2, 1).expect("縮小できる");
        assert_eq!(out, vec![127, 127, 127, 127, 127, 127]);
    }

    #[test]
    fn a_truncated_buffer_is_rejected_instead_of_panicking() {
        assert!(downscale_bgra_bottom_up(&[0u8; 4], 100, 100, 10, 10).is_none());
        assert!(downscale_bgra_bottom_up(&[0u8; 16], 2, 2, 0, 2).is_none());
    }

    #[test]
    fn png_encoding_round_trips_the_size() {
        let rgb = vec![9u8; 4 * 3 * 3];
        let png = encode_png(&rgb, 4, 3).expect("PNG にできる");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "PNG 署名が無い");
        assert!(png.len() > 8);
    }

    #[test]
    fn png_encoding_rejects_a_mismatched_buffer() {
        assert!(encode_png(&[0u8; 10], 4, 3).is_err());
    }

    // --- 資料量の頭打ち ---

    #[test]
    fn the_total_text_budget_keeps_the_front_windows() {
        let mut windows = vec![
            window(&"あ".repeat(100)),
            window(&"い".repeat(100)),
            window(&"う".repeat(100)),
        ];
        cap_total_text(&mut windows, 150);
        assert_eq!(windows[0].text.chars().count(), 100, "手前の窓が削られた");
        assert_eq!(windows[1].text.chars().count(), 50, "途中まで残らない");
        assert!(windows[2].text.is_empty());
    }

    #[test]
    fn windows_beyond_the_budget_keep_their_title() {
        // 本文は落としても「そこに窓があった」ことは資料に残す。
        // 行ごと消すと「一覧を出して」の答えからその窓が漏れる。
        let mut windows = vec![window(&"あ".repeat(500)), window("短い本文")];
        cap_total_text(&mut windows, 100);
        assert!(windows[1].text.is_empty());
        assert_eq!(windows[1].title, "タイトル", "タイトルまで消している");
    }

    #[test]
    fn a_generous_budget_changes_nothing() {
        let mut windows = vec![window("あいうえお"), window("かきくけこ")];
        let before = windows.clone();
        cap_total_text(&mut windows, MAX_TOTAL_TEXT);
        assert_eq!(windows, before);
    }

    #[test]
    fn an_empty_scan_reports_a_reason_instead_of_looking_successful() {
        // design.md「0 件と欠測を混同しない」。
        let scan = ScreenScan::failed("UIA を初期化できません");
        assert!(!scan.has_material());
        assert_eq!(scan.failure.as_deref(), Some("UIA を初期化できません"));
    }

    #[test]
    fn a_scan_with_only_a_screenshot_still_has_material() {
        let scan = ScreenScan {
            screenshot: Some(Screenshot {
                png: vec![1, 2, 3],
                width: 10,
                height: 10,
            }),
            ..Default::default()
        };
        assert!(scan.has_material());
    }

    /// 実画面を走査して**そのまま Gemini へ投げる**通しの疎通確認。
    ///
    /// [`live_screen_scan`] は走査までしか見ておらず、
    /// [`crate::format::tests::live_gemini_screen_ask`] は画像を含まない合成データを送る。
    /// **画像込みの往復だけがどちらでも検証されない**ので、ここで塞ぐ。
    ///
    /// UIA で本文が読めないアプリ (Electron 等) では、実運用の資料は
    /// 実質スクリーンショット 1 枚だけになる。**縮小した画面から
    /// モデルが実際に読み取れるのか**がこの機能の成否そのものなので、
    /// 合成データではなく本物の画面で確かめる必要がある。
    ///
    /// **画面の内容がクラウドへ送られる** (design.md R1)。`#[ignore]` を外さないこと。
    /// 実行: `cargo test --lib -- --ignored --nocapture live_screen_ask_end_to_end`
    #[test]
    #[ignore = "実画面を Gemini へ送る。GEMINI_API_KEY が必要"]
    fn live_screen_ask_end_to_end() {
        let Ok(key) = std::env::var("GEMINI_API_KEY") else {
            println!("GEMINI_API_KEY が無いのでスキップします");
            return;
        };
        let question = std::env::var("NOX_ASK")
            .unwrap_or_else(|_| "画面に見えている内容を箇条書きで説明してください".to_string());

        let started = Instant::now();
        let scan = super::start_scan(true).expect("走査を開始できる").wait();
        let scanned = started.elapsed();

        println!("モニタ : {}", scan.monitor.label());
        println!("走査   : {scanned:?}");
        for window in &scan.windows {
            println!(
                "  {:<22} {:<14} {} 文字",
                window.process.chars().take(20).collect::<String>(),
                window.route.label(),
                window.text.chars().count()
            );
        }
        match &scan.screenshot {
            Some(shot) => println!("画像   : {}x{} / {} KB", shot.width, shot.height, shot.png.len() / 1024),
            None => println!("画像   : なし"),
        }

        // 本番と同じ組み立て (crate::answer_screen_question と同形)。
        let windows: Vec<crate::format::AskWindow<'_>> = scan
            .windows
            .iter()
            .map(|w| crate::format::AskWindow {
                title: &w.title,
                process: &w.process,
                position: &w.position,
                text: &w.text,
            })
            .collect();
        let images: Vec<crate::format::AskImage<'_>> = scan
            .screenshot
            .iter()
            .map(|shot| crate::format::AskImage {
                mime: "image/png",
                bytes: &shot.png,
            })
            .collect();

        let cfg = crate::config::Config::default();
        let asker = crate::format::GeminiFormatter::new(
            crate::stt::build_http_client().expect("クライアント"),
            cfg.gemini_url(),
            crate::config::Secret::new(key),
        );

        let ask_started = Instant::now();
        let answer = {
            use crate::format::ScreenAnswerer;
            asker.ask(&crate::format::AskRequest {
                question: &question,
                monitor: scan.monitor.label(),
                windows: &windows,
                images: &images,
            })
        };
        let asked = ask_started.elapsed();

        match answer {
            Ok(text) => {
                println!("質問   : {question}");
                println!("往復   : {asked:?} (走査込み {:?})", started.elapsed());
                println!("回答:
{text}");
                assert!(!text.trim().is_empty(), "空の回答が返った");
            }
            Err(e) => panic!("画面質問に失敗: {e}"),
        }
    }

    /// 実機で「モニタ 1 枚がどう読めるか」を調べる診断。
    ///
    /// **本文は絶対に出さない** (他人の画面の内容なので)。出すのは
    /// アプリ名・経路・文字数・位置と、画像の有無・大きさだけ。
    /// この一覧を見れば、どのアプリが UIA で読めてどれが画像頼みになるかが
    /// 分かる — 時間予算が実用に耐えるかも所要時間で判断できる。
    ///
    /// 実行: `cargo test -- --ignored --nocapture live_screen_scan`
    #[test]
    #[ignore = "実機の画面を読む。開いているウィンドウに依存する"]
    fn live_screen_scan() {
        let started = Instant::now();
        let Some(handle) = super::start_scan(true) else {
            panic!("走査を開始できなかった");
        };
        let scan = handle.wait();
        let elapsed = started.elapsed();

        println!("モニタ : {}", scan.monitor.label());
        println!("所要   : {elapsed:?}");
        println!("{:<24} {:<16} {:<12} 文字数", "アプリ", "経路", "位置");
        println!("{}", "-".repeat(64));
        for window in &scan.windows {
            println!(
                "{:<24} {:<16} {:<12} {}",
                window.process.chars().take(22).collect::<String>(),
                window.route.label(),
                window.position,
                window.text.chars().count()
            );
        }
        let by_ocr = scan
            .windows
            .iter()
            .filter(|w| w.route == ContextSource::Ocr)
            .count();
        println!("OCR    : {by_ocr} ウィンドウ (UIA で読めなかった分の埋め合わせ)");
        match &scan.screenshot {
            Some(shot) => println!(
                "画像   : {}x{} / {} KB **これがクラウドへ送られる**",
                shot.width,
                shot.height,
                shot.png.len() / 1024
            ),
            None => println!("画像   : なし (全ウィンドウを UIA か OCR で読めた)"),
        }
        if let Some(reason) = &scan.failure {
            println!("失敗   : {reason}");
        }

        // 予算が効いていること。効いていないと、遅いアプリ 1 つで
        // 「答えが返ってこない」になる。
        assert!(
            elapsed < SCAN_BUDGET + Duration::from_millis(500),
            "打ち切りが効いていない: {elapsed:?}"
        );
        // 自分自身のウィンドウを読んでいないこと。**録音中でなくても
        // メインウィンドウは開いていることがある。**
        assert!(
            !scan.windows.iter().any(|w| w.process == "nox-voice.exe"),
            "自分自身のウィンドウを読んでいる"
        );
        assert!(scan.windows.len() <= MAX_WINDOWS);
        for window in &scan.windows {
            assert!(window.text.chars().count() <= MAX_TEXT_PER_WINDOW);
        }
    }

    #[test]
    fn a_scan_with_only_blank_windows_has_no_material() {
        let scan = ScreenScan {
            windows: vec![window("   ")],
            ..Default::default()
        };
        assert!(!scan.has_material());
    }
}

// ---------------------------------------------------------------------------
// Win32 実装。ここから下は単体テストできないので、判定は上の純関数へ委ねる。
// ---------------------------------------------------------------------------
mod win32;

pub use win32::start_scan;
