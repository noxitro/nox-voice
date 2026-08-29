//! 辞書の自動学習 — **貼り付けた後にユーザーが直したところ**から語を覚える。
//!
//! 誤変換を辞書へ登録する作業は、ユーザーから見れば二度手間でしかない。
//! 直したのだからアプリが気づけばよい。Typeless / Wispr Flow が
//! 「自動追加」としてやっているのがこれで、ここでも同じ形を採る。
//!
//! ```text
//! 貼付完了 ──► GetFocusedElement で対象要素を掴む (2〜20ms)
//!              ├─ must_not_read (パスワード欄) なら諦める
//!              └─ B0 を読み、**貼ったテキストが中に見つかったときだけ**続ける
//!                 ▼
//!            TextChanged / FocusChanged を購読
//!                 ▼ 1.5 秒の無変更 (デバウンス) または フォーカス離脱
//!            B1 を読み直し、**貼った範囲に限って**差分を取る
//!                 ▼
//!            置換されたトークン対 (誤り → 直したもの) を候補にする
//!                 ▼ 除外規則 ([`reject_reason`])
//!            辞書へ `origin: auto` で追加する
//! ```
//!
//! # なぜ「貼ったテキストが見つかったときだけ」なのか
//!
//! これが**この機能の安全装置の本体**。見つかったということは、
//! その要素は「たったいま自分が書き込んだ欄」に他ならない。
//! 見つからなければ (フォーカスが移った・貼付が届かなかった・
//! 相手が読み取りを許さない) 何も読まずに黙って諦める。
//! **他人の文章を読みに行く経路がそもそも存在しない**ようにしてある。
//!
//! # プライバシー (design.md R1)
//!
//! - 読んだ本文は**ログにも履歴にも出さない**。出すのは文字数・件数・経路だけ
//!   ([`crate::screen`] / [`crate::context`] と同じ方針)
//! - 読んだ本文は**プロセスの外へ出ない**。差分計算はすべてローカルで、
//!   クラウドへ行くのは抽出された語が辞書に載ってからの話
//! - `IsPassword` の要素は読まない ([`crate::context::must_not_read`])
//! - 抽出した語は設定画面に出る。**これは意図した可視化**で、
//!   「アプリが何を勝手に覚えたか」を確かめられないほうが危ない
//!
//! # 実測 (スパイク `uia_spike.rs`、2026-08-30)
//!
//! - `GetFocusedElement` は **2〜20ms** で Chromium の中まで一発で届く。
//!   木を降りる必要は無い (Wikipedia 記事 1 ページの全走査は 2.5〜3.0 秒)
//! - `TextChanged` の最初の 1 通まで 103〜150ms。ただし **Chromium は
//!   1 変更につき複数通投げる** (変更 2 回/秒に対し 8〜10 通/秒)。
//!   **デバウンスは必須**で、1 通ごとに読み返す作りにしてはいけない
//! - 読み取りは **3 経路すべて**を試す。VSCode の `native-edit-context` は
//!   Text=68 / Value=0、Typeless.exe は Text=0 / Value=105 だった
//! - **Chromium 内部の要素は `CurrentNativeWindowHandle()` が 0**。
//!   プロセスの特定は `CurrentProcessId()` で行う
//! - **Chromium は非アクティブなタブを木に出さない。** タブを切り替えられたら
//!   要素は消える → 「学習の断念」として静かに捨てる。エラーにしない

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
#[cfg(windows)]
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationEventHandler,
    IUIAutomationEventHandler_Impl, IUIAutomationFocusChangedEventHandler,
    IUIAutomationFocusChangedEventHandler_Impl, TreeScope_Subtree, UIA_EVENT_ID,
    UIA_Text_TextChangedEventId,
};

// ---------------------------------------------------------------------------
// 定数
// ---------------------------------------------------------------------------

/// 学習の対象にしてよい欄の長さの上限。
///
/// deep context の [`crate::context::MAX_CONTEXT_CHARS`] (2,000) より大きい。
/// あちらは「プロンプトへ載せる分」なので**切り詰めてよい**が、こちらは
/// **差分の基準**なので切り詰めてはいけない — 切れ目から後ろを丸ごと
/// 「消された」と読んでしまう。
///
/// # だから「切る」ではなく「降りる」
///
/// 3 経路のうち上限を渡せるのは `TextPattern` だけで、`ValuePattern` と
/// `LegacyIAccessible` は**値を丸ごと返す**(COM の `BSTR` を受け取る時点で
/// 長さは決まっており、こちらから制限する口が無い)。したがって
/// 「上限まで読む」は 3 経路で意味が揃わない。
///
/// そこで**読めた長さがこの上限に達していたら学習そのものを見送る**
/// ([`read_learnable`])。長文編集は辞書語の抽出先ではないので、
/// 見送っても失うものは無い。逆にここを切り詰めて続行すると、
/// 上の「切ってはいけない」という前提を自分で破ることになる。
///
/// あわせて、[`sole_occurrence`] が O(欄長 × 貼付長) なので、
/// 巨大な値を返す欄 (ログビューア等) で裏スレッドが CPU を焼くのも防ぐ。
pub const LEARN_READ_CAP: usize = 8_000;

/// 貼ったテキストが欄に現れるのを待つ時間。
///
/// `Ctrl+V` を送出してから相手アプリが反映するまでにはラグがある
/// ([`crate::inject`] のモジュール doc — 送出は「貼られた保証」ではない)。
const BASELINE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// 基準値を取りに行く間隔。
const BASELINE_POLL: Duration = Duration::from_millis(120);

/// 変更が止まったとみなすまでの無変更時間。
///
/// **Chromium は 1 変更につき 8〜10 通/秒のイベントを投げる** (実測)。
/// 1 通ごとに読み返すと相手アプリを叩き続けることになるうえ、
/// 「打っている途中の半端な文字列」を学習してしまう。
const DEBOUNCE: Duration = Duration::from_millis(1_500);

/// 監視ループを回す間隔。
const POLL: Duration = Duration::from_millis(120);

/// 1 回の貼付を見張る上限。
///
/// これを過ぎたら黙って手を引く。**イベントハンドラを張りっぱなしに
/// すると、相手アプリが閉じた後もこちらが掴み続ける**ので、
/// 「いつか必ず外れる」時間を決めておく。
const WATCH_MAX: Duration = Duration::from_secs(90);

/// 1 回の編集から採る候補の上限 (Wispr Flow の公開仕様に合わせる)。
const MAX_CANDIDATES_PER_EDIT: usize = 4;

/// 1 語に許すトークン数の上限 (同上)。
const MAX_TERM_TOKENS: usize = 4;

/// 1 語に許す文字数の上限。文単位の書き換えは「語」ではない。
const MAX_TERM_CHARS: usize = 24;

/// 差分を取るトークン数の上限。
///
/// これを超える書き換えは「誤変換の手直し」ではなく文章の作り直しで、
/// 語を抜き出す意味が無い。LCS が O(n·m) なので計算量の蓋も兼ねる。
const MAX_DIFF_TOKENS: usize = 64;

/// 短すぎる貼付では学習しない (直すところが無い)。
const MIN_PASTED_CHARS: usize = 4;

// ---------------------------------------------------------------------------
// 候補と除外規則 (ここは純関数。実機なしで全部テストできる)
// ---------------------------------------------------------------------------

/// 覚える候補 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 誤認識されていた側。読みの補完に使う。**辞書には載らない**。
    pub wrong: String,
    /// ユーザーが直した側 = 覚える表記。
    pub written: String,
    /// 読み (仮名で書けたときだけ)。
    pub reading: Option<String>,
}

/// 候補を捨てた理由。**テストが「なぜ落ちたか」まで主張できるように**
/// 型で持つ (bool を返すと、意図した規則で落ちたのか偶然かが分からない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// 変わっていない。
    Unchanged,
    /// 純挿入・純削除 (片側が空)。書き間違いの訂正ではない。
    PureInsertOrDelete,
    /// 大文字小文字だけの違い。
    CaseOnly,
    /// 空白の入れ方だけの違い。
    WhitespaceOnly,
    /// トークンが多すぎる (4 語超)。
    TooManyTokens,
    /// 長すぎる。文の書き換えであって語ではない。
    TooLong,
    /// 短すぎる (1 文字)。日本語の 1 文字は語として弱く誤爆しかしない。
    TooShort,
    /// 中身のある文字 (かな・漢字・英数) が無い。記号だけ、など。
    NoContentChars,
    /// 数字・数詞だけの違い (「3」→「三」など)。
    NumeralsOnly,
    /// 改行を含む。段落の入れ替えは語の訂正ではない。
    MultiLine,
    /// URL・メールアドレス・パスに見える。語ではないうえ個人情報になりやすい。
    UrlLike,
    /// フィラー。
    Filler,
    /// 一般語。
    CommonWord,
}

impl Reject {
    /// ログ用の短い理由 (**本文は出さない**ので、これだけが手がかりになる)。
    pub fn label(self) -> &'static str {
        match self {
            Reject::Unchanged => "変化なし",
            Reject::PureInsertOrDelete => "純挿入・純削除",
            Reject::CaseOnly => "大文字小文字のみ",
            Reject::WhitespaceOnly => "空白のみ",
            Reject::TooManyTokens => "語数超過",
            Reject::TooLong => "長すぎる",
            Reject::TooShort => "短すぎる",
            Reject::NoContentChars => "中身のある文字が無い",
            Reject::NumeralsOnly => "数詞のみ",
            Reject::MultiLine => "改行を含む",
            Reject::UrlLike => "URL・パスに見える",
            Reject::Filler => "フィラー",
            Reject::CommonWord => "一般語",
        }
    }
}

/// 英語の一般語。ここに載る語を辞書へ入れても表記は安定しない。
const ENGLISH_COMMON: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "is", "are", "was", "were", "be", "been", "being",
    "to", "of", "in", "on", "at", "it", "its", "this", "that", "these", "those", "for", "with",
    "as", "by", "from", "not", "no", "yes", "we", "you", "they", "he", "she", "i", "me", "my",
    "our", "your", "their", "do", "does", "did", "have", "has", "had", "will", "would", "shall",
    "should", "can", "could", "may", "might", "must", "so", "then", "than", "there", "here",
    "when", "where", "what", "which", "who", "how", "why", "all", "any", "some", "more", "most",
    "very", "just", "now", "ok", "okay",
];

/// 日本語の一般語。
///
/// # 「一般語」をどう決めたか
///
/// 機械的な規則を先に置き、リストは**それで落ちないものだけ**にしてある。
/// 規則で落ちる語をリストにも書くと、どちらが効いているのか分からなくなる。
///
/// 機械的な規則 ([`reject_reason`]):
///
/// 1. **ひらがなだけの語は覚えない。** 助詞・活用語尾・和語の一般語が
///    ほぼここに入る。固有名詞や専門用語をひらがなで辞書登録する動機は
///    まず無く、逆にここを許すと「て・に・を・は」を延々覚え続ける
/// 2. **1 文字の語は覚えない。** 日本語の 1 文字は多義で、
///    「直した」のか「別の語に置き換えた」のかを区別できない
/// 3. **カタカナ 2 文字以下も覚えない** (「アプリ」は 3 文字なので残る)
///
/// 下のリストは、この 3 つを通り抜けてしまう**漢字を含む高頻度語**だけ。
/// 誤って落としても損は小さい (ユーザーが手で登録できる) が、
/// 誤って覚えると 36 語の枠を毎日削っていくので、迷ったら落とす側に倒す。
const JAPANESE_COMMON: &[&str] = &[
    "事", "物", "時", "人", "方", "話", "日", "年", "月", "週", "今日", "明日", "昨日", "自分",
    "場合", "必要", "確認", "対応", "実際", "以上", "以下", "問題", "内容", "理由", "状態",
    "結果", "連絡", "時間", "今回", "前回", "次回", "一部", "全部", "全体", "下さい", "有難う",
    "宜しく", "御願い", "出来", "感じ", "部分", "方法", "説明", "質問", "回答", "作業", "多分",
    "本当", "普通", "最近", "今後", "一緒", "大丈夫", "所謂", "為",
];

/// フィラー。**ほとんどはひらがな規則で先に落ちる**が、
/// カタカナ書き (「エート」) や漢字混じりの言い淀みのために持つ。
const FILLERS: &[&str] = &[
    "えーと", "えっと", "ええと", "あのー", "あの", "その", "まあ", "なんか", "うーん", "ええ",
    "あー", "えー", "んー", "エート", "アノー", "マア", "ナンカ",
];

/// 候補として認めるか。`None` なら学習してよい。**純関数**。
///
/// 規則の順番は「安いもの・広く効くものから」。理由を型で返すので、
/// テストは「落ちたこと」ではなく「意図した規則で落ちたこと」を主張できる。
pub fn reject_reason(wrong: &str, written: &str) -> Option<Reject> {
    let wrong = wrong.trim();
    let written = written.trim();

    if wrong.is_empty() || written.is_empty() {
        return Some(Reject::PureInsertOrDelete);
    }
    if wrong == written {
        return Some(Reject::Unchanged);
    }
    if collapse_whitespace(wrong) == collapse_whitespace(written) {
        return Some(Reject::WhitespaceOnly);
    }
    if wrong.to_lowercase() == written.to_lowercase() {
        return Some(Reject::CaseOnly);
    }
    if written.contains('\n') || wrong.contains('\n') {
        return Some(Reject::MultiLine);
    }
    if is_url_like(written) || is_url_like(wrong) {
        return Some(Reject::UrlLike);
    }

    let len = written.chars().count();
    if len > MAX_TERM_CHARS || wrong.chars().count() > MAX_TERM_CHARS {
        return Some(Reject::TooLong);
    }
    if len < 2 {
        return Some(Reject::TooShort);
    }
    if token_count(written) > MAX_TERM_TOKENS || token_count(wrong) > MAX_TERM_TOKENS {
        return Some(Reject::TooManyTokens);
    }
    if !written.chars().any(is_content_char) {
        return Some(Reject::NoContentChars);
    }
    if written.chars().all(is_numeral) {
        return Some(Reject::NumeralsOnly);
    }
    if FILLERS.contains(&written) || FILLERS.contains(&wrong) {
        return Some(Reject::Filler);
    }
    if is_common_word(written) {
        return Some(Reject::CommonWord);
    }
    None
}

/// 一般語か。機械的な規則 3 つ + リスト ([`JAPANESE_COMMON`] の doc)。
fn is_common_word(written: &str) -> bool {
    let chars: Vec<char> = written.chars().collect();
    // 1. ひらがなだけ。
    if chars.iter().all(|&c| is_hiragana(c) || is_kana_mark(c)) {
        return true;
    }
    // 3. カタカナ 2 文字以下。
    if chars.len() <= 2 && chars.iter().all(|&c| is_katakana(c) || is_kana_mark(c)) {
        return true;
    }
    if JAPANESE_COMMON.contains(&written) {
        return true;
    }
    // 英語は小文字化して照合する (大文字小文字だけの違いは既に落としてある)。
    let lower = written.to_lowercase();
    written.is_ascii() && ENGLISH_COMMON.contains(&lower.as_str())
}

/// URL・メールアドレス・ファイルパスに見えるか。
///
/// 語ではないうえ、**個人を特定しうる文字列**が辞書経由でクラウドへ
/// 出ていく経路になる。判定は緩め (疑わしきは落とす) でよい。
fn is_url_like(text: &str) -> bool {
    if text.contains("://")
        || text.contains('/')
        || text.contains('\\')
        || (text.contains('@') && text.contains('.'))
    {
        // スラッシュ・バックスラッシュを含む語は、URL かパスか
        // 「A/B」のような略記のどれか。**どれも辞書語ではない**ので、
        // 区別せずに落とす (`/home/user/secret` もここで止まる)。
        return true;
    }
    // スキームの無いドメイン (`example.com`)。最後の `.` の後ろが
    // 英字だけなら拡張子かトップレベルドメインとみなす。
    // `GPT-4.0` のように後ろが数字のものは巻き込まない。
    match text.rsplit_once('.') {
        Some((head, tail)) => {
            !head.is_empty()
                && tail.len() >= 2
                && tail.chars().all(|c| c.is_ascii_alphabetic())
                && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        }
        None => false,
    }
}

/// かなだけで書かれているか (読みとして使えるか)。
pub fn is_kana_only(text: &str) -> bool {
    let mut any = false;
    for c in text.chars() {
        if is_kana_mark(c) {
            continue;
        }
        if is_hiragana(c) || is_katakana(c) {
            any = true;
            continue;
        }
        return false;
    }
    any
}

fn is_hiragana(c: char) -> bool {
    ('\u{3041}'..='\u{309F}').contains(&c)
}

fn is_katakana(c: char) -> bool {
    ('\u{30A0}'..='\u{30FF}').contains(&c) || ('\u{FF66}'..='\u{FF9F}').contains(&c)
}

/// 長音符・繰り返し記号など、かなに付いて回る記号。
fn is_kana_mark(c: char) -> bool {
    // U+30FC が「ー」そのもの。残りは繰り返し記号 (ゝ ゞ ヽ ヾ) と々。
    matches!(c, '\u{30FC}' | '\u{309D}' | '\u{309E}' | '\u{30FD}' | '\u{30FE}' | '\u{3005}')
}

fn is_kanji(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
        || ('\u{3400}'..='\u{4DBF}').contains(&c)
        || ('\u{F900}'..='\u{FAFF}').contains(&c)
        || c == '\u{3005}'
}

/// 「中身のある文字」か。記号と空白だけの候補を弾くために使う。
fn is_content_char(c: char) -> bool {
    c.is_alphanumeric() || is_kanji(c)
}

/// 数字・漢数字か。
fn is_numeral(c: char) -> bool {
    c.is_ascii_digit()
        || ('\u{FF10}'..='\u{FF19}').contains(&c)
        || matches!(
            c,
            '〇' | '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十' | '百'
                | '千' | '万' | '億' | '兆' | '.' | ',' | '第' | '番'
        )
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// トークン化と差分
// ---------------------------------------------------------------------------

/// 文字の種別。**種別が変わるところが語の切れ目**という近似。
///
/// 形態素解析器は積まない。辞書 1 つのために数十 MB の辞書と依存を
/// 抱える割に、ここで要るのは「漢字の連なり」「カタカナの連なり」
/// 「英数の連なり」を切り出すことだけで、字種の切り替わりでほぼ足りる。
/// 「塩谷さん」→ `塩谷` + `さん` のように、**欲しい側 (漢字) が
/// そのまま 1 トークンになる**のが日本語表記の性質。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Kanji,
    Hiragana,
    Katakana,
    /// 英数字とその内部で使われる記号 (`nox-voice` を割らないため)。
    Word,
    Space,
    Other,
}

fn classify(c: char) -> Class {
    if c.is_whitespace() {
        return Class::Space;
    }
    if c.is_ascii_alphanumeric()
        || ('\u{FF21}'..='\u{FF3A}').contains(&c)
        || ('\u{FF41}'..='\u{FF5A}').contains(&c)
        || ('\u{FF10}'..='\u{FF19}').contains(&c)
        || matches!(c, '-' | '_')
    {
        return Class::Word;
    }
    if is_hiragana(c) {
        return Class::Hiragana;
    }
    if is_katakana(c) {
        return Class::Katakana;
    }
    if is_kanji(c) {
        return Class::Kanji;
    }
    Class::Other
}

/// 文字位置の範囲で表したトークン。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Token {
    start: usize,
    end: usize,
}

/// 字種の切れ目でトークンに割る。
fn tokenize(chars: &[char]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let class = classify(chars[i]);
        let start = i;
        // 長音符・繰り返し記号は直前のかなにくっつける (「コンピューター」)。
        while i < chars.len() && (classify(chars[i]) == class || (i > start && is_kana_mark(chars[i])))
        {
            i += 1;
        }
        out.push(Token { start, end: i });
    }
    out
}

/// 語数 (空白・記号のトークンは数えない)。
fn token_count(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    tokenize(&chars)
        .into_iter()
        .filter(|t| {
            let class = classify(chars[t.start]);
            class != Class::Space && class != Class::Other
        })
        .count()
}

fn slice(chars: &[char], start: usize, end: usize) -> String {
    chars[start..end].iter().collect()
}

/// 置換されたトークンの並び 1 組。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    wrong: String,
    written: String,
}

/// トークン列同士の差分から、**両側が空でない**置換だけを取り出す。
///
/// 純挿入 (旧が空) と純削除 (新が空) は最初から作らない — 除外規則で
/// 落とすのではなく、そもそも候補にしない。
fn replacement_hunks(old: &[char], new: &[char]) -> Vec<Hunk> {
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    if old_tokens.len() > MAX_DIFF_TOKENS || new_tokens.len() > MAX_DIFF_TOKENS {
        // 文章の作り直し。語の訂正ではないので手を出さない。
        return Vec::new();
    }

    let old_text: Vec<String> = old_tokens.iter().map(|t| slice(old, t.start, t.end)).collect();
    let new_text: Vec<String> = new_tokens.iter().map(|t| slice(new, t.start, t.end)).collect();

    // 共通部分列 (LCS) の対応表。n·m は MAX_DIFF_TOKENS^2 で抑えてある。
    let (n, m) = (old_text.len(), new_text.len());
    let mut table = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old_text[i] == new_text[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    // 一致したトークンの位置対を並べ、その隙間を「置換」とみなす。
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_text[i] == new_text[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matches.push((n, m)); // 末尾の番兵。

    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut prev_i, mut prev_j) = (0usize, 0usize);
    for (mi, mj) in matches {
        if mi > prev_i && mj > prev_j {
            let wrong = slice(old, old_tokens[prev_i].start, old_tokens[mi - 1].end);
            let written = slice(new, new_tokens[prev_j].start, new_tokens[mj - 1].end);
            hunks.push(Hunk {
                wrong: wrong.trim().to_string(),
                written: written.trim().to_string(),
            });
        }
        prev_i = mi + 1;
        prev_j = mj + 1;
    }
    hunks
}

/// 貼付前後の欄の中身から候補を作る。**この関数が判定の全体**。
///
/// - `before` … 貼った直後の欄 (B0)
/// - `after`  … ユーザーが手を入れた後の欄 (B1)
/// - `pasted` … 自分が貼ったテキスト
///
/// 候補が無ければ空を返す。**「学習しない」は失敗ではない**ので、
/// 呼び出し側はこれを静かに受け取る。
pub fn candidates_from_edit(before: &str, after: &str, pasted: &str) -> Vec<Candidate> {
    let before: Vec<char> = normalize_newlines(before).chars().collect();
    let after: Vec<char> = normalize_newlines(after).chars().collect();
    let pasted: Vec<char> = normalize_newlines(pasted).chars().collect();

    if pasted.len() < MIN_PASTED_CHARS {
        return Vec::new();
    }
    // 貼った文字列が 2 か所以上にあると、**どちらが自分の貼付か決められない**。
    // 当てずっぽうで片方を選ぶくらいなら学習しない。
    let Some(paste_start) = sole_occurrence(&before, &pasted) else {
        return Vec::new();
    };
    let paste_end = paste_start + pasted.len();

    // 変化した範囲を、前後の一致で挟み込んで最小化する。
    let prefix = before
        .iter()
        .zip(after.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = before.len().min(after.len()) - prefix;
    let suffix = (0..max_suffix)
        .take_while(|&k| before[before.len() - 1 - k] == after[after.len() - 1 - k])
        .count();

    let minimal = prefix..before.len() - suffix;

    // **貼った範囲の外だけを直したなら、それは自分と無関係な編集**。
    // 他人が書いていた文章の手直しを覚えてはいけない。
    // 範囲が空 (純挿入・純削除) のときも重なりは生じない。
    //
    // **判定は語へ広げる前の範囲で行う。** 広げた後に見ると、貼付範囲の
    // すぐ外を直しただけの編集が、同じ字種で地続きだったせいで
    // 「貼った範囲に掛かっている」ことにされてしまう。
    // 実際に変わったのはどこか、が問いなので、最小の範囲が答え。
    let overlaps = minimal.start < paste_end && paste_start < minimal.end;
    if !overlaps {
        return Vec::new();
    }

    // ここまでの範囲は**語の途中で切れている**。「塩屋」→「塩谷」なら
    // 一致しない 1 文字 (屋 / 谷) しか残らず、1 文字の語として捨てられる。
    // 語の切れ目まで広げてから差分を取る ([`widen_to_word`])。
    let (old_span, new_span) = widen_to_word(
        &before,
        &after,
        minimal,
        prefix..after.len() - suffix,
    );

    let mut out: Vec<Candidate> = Vec::new();
    for hunk in replacement_hunks(&before[old_span], &after[new_span]) {
        if let Some(reason) = reject_reason(&hunk.wrong, &hunk.written) {
            // **理由だけを出す。語そのものは出さない** — 候補にならなかった
            // 語は UI にも出ないので、ログに書けばそこだけが本文の写しになる。
            log::debug!("自動学習: 候補を除外しました ({})", reason.label());
            continue;
        }
        if out.iter().any(|c: &Candidate| c.written == hunk.written) {
            continue;
        }
        // 誤認識された側がかなだけなら、それは**まさに「聞こえた音」**。
        // 読みとして使える唯一の形なので、そのときだけ埋める。
        let reading = (is_kana_only(&hunk.wrong) && !is_kana_only(&hunk.written))
            .then(|| hunk.wrong.clone());
        out.push(Candidate {
            wrong: hunk.wrong,
            written: hunk.written,
            reading,
        });
        if out.len() >= MAX_CANDIDATES_PER_EDIT {
            break;
        }
    }
    out
}

/// 一致しない範囲を、**語の切れ目まで外側へ広げる**。
///
/// # なぜ要るか
///
/// 前後の一致で挟み込んだ範囲は語の途中で切れている。「塩屋」→「塩谷」
/// なら残るのは `屋` と `谷` の 1 文字ずつで、これは語ではない
/// (実際 [`Reject::TooShort`] で落ちる)。**辞書に入れたいのは `塩谷` の方**。
///
/// # なぜ「トークンの境界まで」ではないのか
///
/// トークン ([`tokenize`]) の境界まで一律に広げると、直前の文脈が
/// 同じ字種のときに巻き込む。「これは**のっくすぼいす**の話です」→
/// 「これは**nox-voice**の話です」では、`のっくすぼいす` は直前の
/// `これは` と地続きのひらがな 1 トークンなので、`これはのっくすぼいすの`
/// ごと 1 語にされてしまう。
///
/// そこで**両側の字種が揃っているあいだだけ**広げる。上の例では
/// 広げる先の字種 (ひらがな) が新側の先頭 `n` (英数) と食い違うので
/// 1 文字も広がらず、`のっくすぼいす` → `nox-voice` が残る。
/// 「塩屋」の例では両側とも漢字なので `塩` まで広がる。
fn widen_to_word(
    before: &[char],
    after: &[char],
    old: std::ops::Range<usize>,
    new: std::ops::Range<usize>,
) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let (mut os, mut oe) = (old.start, old.end);
    let (mut ns, mut ne) = (new.start, new.end);

    // 左へ。`before[os - 1] == after[ns - 1]` は共通接頭辞なので保証済み。
    while os > 0 && ns > 0 && os < oe && ns < ne {
        let class = classify(before[os - 1]);
        if class == classify(before[os]) && class == classify(after[ns]) {
            os -= 1;
            ns -= 1;
        } else {
            break;
        }
    }
    // 右へ。同じく `before[oe] == after[ne]` は共通接尾辞。
    while oe < before.len() && ne < after.len() && oe > os && ne > ns {
        let class = classify(before[oe]);
        if class == classify(before[oe - 1]) && class == classify(after[ne - 1]) {
            oe += 1;
            ne += 1;
        } else {
            break;
        }
    }
    (os..oe, ns..ne)
}

/// `needle` がちょうど 1 回だけ現れる位置。0 回・2 回以上なら `None`。
fn sole_occurrence(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let mut found = None;
    for start in 0..=hay.len() - needle.len() {
        if hay[start..start + needle.len()] == *needle {
            if found.is_some() {
                return None;
            }
            found = Some(start);
        }
    }
    found
}

/// 改行を `\n` に揃える。相手アプリが `\r\n` で持っていても差分が壊れないように。
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

// ---------------------------------------------------------------------------
// 実機の監視 (Windows のみ)
// ---------------------------------------------------------------------------

/// 監視の世代。**新しい貼付が来たら古い監視は黙って手を引く**。
///
/// 前の貼付を見張ったまま次の貼付を受けると、後の貼付そのものを
/// 「ユーザーの手直し」として学習してしまう。世代が変わったら諦める。
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// 学習結果を UI へ知らせるイベント。
///
/// 設定画面が開いたまま語が増えると、**画面の一覧が古いまま保存されて
/// 覚えた語が消える** (UI は辞書を全置換で送るため)。行を足させる。
pub const EVENT_DICTIONARY_LEARNED: &str = "nox://dictionary-learned";

/// 走っている監視をすべて諦めさせる (アプリ終了時など)。
pub fn abandon_all() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// 監視の結末。ログとテストのために名前を付けておく。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchOutcome {
    /// 設定で無効。
    Disabled,
    /// 貼付が短すぎる等、始める前に降りた。
    NotStarted,
    /// 基準値を取れなかった (貼付が見つからない・パスワード欄・読めない)。
    NoBaseline,
    /// 見張ったが手直しが無かった。
    NoEdit,
    /// 手直しはあったが、候補として認められなかった。
    NoCandidate,
    /// 語を覚えた。
    Learned(usize),
    /// 途中で対象を見失った (タブ切替・ウィンドウが閉じた等)。
    Lost,
    /// 新しい貼付に追い越された。
    Superseded,
}

impl WatchOutcome {
    pub fn label(self) -> &'static str {
        match self {
            WatchOutcome::Disabled => "無効",
            WatchOutcome::NotStarted => "対象外",
            WatchOutcome::NoBaseline => "基準値を取れず",
            WatchOutcome::NoEdit => "手直しなし",
            WatchOutcome::NoCandidate => "候補なし",
            WatchOutcome::Learned(_) => "学習",
            WatchOutcome::Lost => "対象を見失った",
            WatchOutcome::Superseded => "次の貼付に追い越された",
        }
    }
}

/// 貼付直後に呼ぶ。**呼び出し側をブロックしない** (専用スレッドへ逃がす)。
///
/// `pasted` は自分が貼ったテキスト。設定が無効なら何もしない。
#[cfg(windows)]
pub fn start_watch(app: &tauri::AppHandle, enabled: bool, pasted: &str) {
    // 始める前に降りる場合も、**結末に名前を付けてログへ出す**。
    // 「そもそも始めなかった」と「見張ったが何も無かった」は別のことで、
    // 効いていないときにどちらなのか分からないと切り分けができない。
    let refused = if !enabled {
        Some(WatchOutcome::Disabled)
    } else if pasted.chars().count() < MIN_PASTED_CHARS {
        Some(WatchOutcome::NotStarted)
    } else {
        None
    };
    if let Some(outcome) = refused {
        log::debug!("辞書の自動学習: {}", outcome.label());
        return;
    }
    // 前の監視に手を引かせてから、自分の世代を決める。
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    let pasted = pasted.to_string();

    let spawned = std::thread::Builder::new()
        .name("nox-uia-learn".to_string())
        .spawn(move || {
            // 覚える段だけを差し替えられるようにしてある。実機の診断テスト
            // (`live_learn_watch`) が**本番と同じ監視ループ**を回しつつ、
            // 設定ファイルには触れずに済ませるため。
            let register = |candidates: &[Candidate]| register(&app, candidates);
            let outcome =
                watch_blocking(generation, &pasted, BASELINE_TIMEOUT, &register);
            // **本文は出さない**。件数と結末だけ。
            match outcome {
                WatchOutcome::Learned(n) => {
                    log::info!("辞書の自動学習: {n} 語を追加しました")
                }
                other => log::debug!("辞書の自動学習: {}", other.label()),
            }
        });
    if let Err(e) = spawned {
        log::warn!("自動学習のスレッドを起動できません: {e}");
    }
}

/// Windows 以外では何もしない (UIA が無い)。
#[cfg(not(windows))]
pub fn start_watch(_app: &tauri::AppHandle, _enabled: bool, _pasted: &str) {}

/// COM を MTA で初期化し、drop で解放する。
///
/// **UIA は COM。初期化したスレッドで使い切り、必ず対で解放する。**
#[cfg(windows)]
struct ComGuard;

#[cfg(windows)]
impl ComGuard {
    fn new() -> Option<Self> {
        // SAFETY: このスレッドで初期化し、Drop で対の CoUninitialize を呼ぶ。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        hr.is_ok().then_some(Self)
    }
}

#[cfg(windows)]
impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: new() の CoInitializeEx と対。
        unsafe { CoUninitialize() };
    }
}

/// イベントハンドラの購読を握り、**drop で必ず外す**。
///
/// ここが「張りっぱなしにしない」の本体。早期 return が何本あっても、
/// 相手アプリが先に閉じても、このスコープを出れば購読は外れる。
/// 外し忘れると UIA はこちらの参照を保持し続け、**相手のプロセスが
/// 終わった後も掴んだまま**になる。
#[cfg(windows)]
struct Subscription {
    automation: IUIAutomation,
    element: IUIAutomationElement,
    text_handler: Option<IUIAutomationEventHandler>,
    focus_handler: Option<IUIAutomationFocusChangedEventHandler>,
}

#[cfg(windows)]
impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(handler) = self.text_handler.take() {
            // SAFETY: 購読時と同じ automation / element / handler の 3 つ組。
            // これが揃っていないと UIA は外してくれない。
            unsafe {
                let _ = self.automation.RemoveAutomationEventHandler(
                    UIA_Text_TextChangedEventId,
                    &self.element,
                    &handler,
                );
            }
        }
        if let Some(handler) = self.focus_handler.take() {
            // SAFETY: 購読時と同じ automation / handler。
            unsafe {
                let _ = self.automation.RemoveFocusChangedEventHandler(&handler);
            }
        }
    }
}

/// TextChanged を受けて「最後に動いた時刻」だけを更新する。
///
/// **ハンドラの中で本文を読まない。** ハンドラは相手プロセス由来の
/// RPC スレッドで走るうえ、Chromium は 8〜10 通/秒 投げてくる (実測)。
/// ここで読み返すと相手を叩き続けることになる。
#[cfg(windows)]
#[windows::core::implement(IUIAutomationEventHandler)]
struct ChangeHandler {
    /// 監視開始からの経過 ms。0 は「まだ一度も動いていない」。
    last_change_ms: Arc<AtomicU64>,
    started: Instant,
}

#[cfg(windows)]
impl IUIAutomationEventHandler_Impl for ChangeHandler_Impl {
    fn HandleAutomationEvent(
        &self,
        _sender: windows::core::Ref<IUIAutomationElement>,
        _eventid: UIA_EVENT_ID,
    ) -> windows::core::Result<()> {
        let elapsed = self.started.elapsed().as_millis() as u64;
        // 0 は「未変更」の意味に使っているので、必ず 1 以上にする。
        self.last_change_ms.store(elapsed.max(1), Ordering::SeqCst);
        Ok(())
    }
}

/// FocusChanged は**旗を立てるだけ**。
///
/// このハンドラはデスクトップ全体に張るので、あらゆるアプリの
/// フォーカス移動で呼ばれる。ここで要素を比べに行くと、無関係な
/// アプリの操作のたびに COM 呼び出しが走る。「誰かが移った」だけ
/// 記録して、**判定は監視ループ側でこちらの要素とだけ突き合わせる**。
#[cfg(windows)]
#[windows::core::implement(IUIAutomationFocusChangedEventHandler)]
struct FocusHandler {
    moved: Arc<AtomicBool>,
}

#[cfg(windows)]
impl IUIAutomationFocusChangedEventHandler_Impl for FocusHandler_Impl {
    fn HandleFocusChangedEvent(
        &self,
        _sender: windows::core::Ref<IUIAutomationElement>,
    ) -> windows::core::Result<()> {
        self.moved.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// 監視の本体。専用スレッドで呼ばれる前提。
#[cfg(windows)]
fn watch_blocking(
    generation: u64,
    pasted: &str,
    baseline_timeout: Duration,
    register: &dyn Fn(&[Candidate]) -> usize,
) -> WatchOutcome {
    let Some(_com) = ComGuard::new() else {
        log::warn!("自動学習: COM を初期化できません");
        return WatchOutcome::NoBaseline;
    };
    // SAFETY: COM は初期化済み。CLSID は UIA のもの。
    let automation: IUIAutomation =
        match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) } {
            Ok(a) => a,
            Err(e) => {
                log::warn!("自動学習: UI Automation を生成できません: {e}");
                return WatchOutcome::NoBaseline;
            }
        };

    // --- 1. 対象要素と基準値 B0 ---
    let Some((element, baseline)) =
        acquire_baseline(&automation, generation, pasted, baseline_timeout)
    else {
        return WatchOutcome::NoBaseline;
    };

    // --- 2. 変化の購読 ---
    let started = Instant::now();
    let last_change_ms = Arc::new(AtomicU64::new(0));
    let focus_moved = Arc::new(AtomicBool::new(false));

    let text_handler: IUIAutomationEventHandler = ChangeHandler {
        last_change_ms: last_change_ms.clone(),
        started,
    }
    .into();
    let focus_handler: IUIAutomationFocusChangedEventHandler = FocusHandler {
        moved: focus_moved.clone(),
    }
    .into();

    // SAFETY: automation / element / handler はいずれも有効。cacherequest は None。
    // Chromium は本文を子要素として持つので、部分木まで拾う。
    let text_ok = unsafe {
        automation.AddAutomationEventHandler(
            UIA_Text_TextChangedEventId,
            &element,
            TreeScope_Subtree,
            None,
            &text_handler,
        )
    }
    .is_ok();
    // SAFETY: automation / focus_handler は有効。
    let focus_ok = unsafe { automation.AddFocusChangedEventHandler(None, &focus_handler) }.is_ok();

    // **購読した瞬間から Drop 保証の下に置く。** 以降の早期 return は
    // すべてここを通って購読を外す。
    let _subscription = Subscription {
        automation: automation.clone(),
        element: element.clone(),
        text_handler: text_ok.then_some(text_handler),
        focus_handler: focus_ok.then_some(focus_handler),
    };

    if !text_ok && !focus_ok {
        // どちらも張れないなら見張りようが無い。ポーリングで粘らない —
        // 相手アプリを一定間隔で叩き続ける価値は無い。
        log::debug!("自動学習: イベントを購読できませんでした");
        return WatchOutcome::NoBaseline;
    }

    // --- 3. 確定を待つ ---
    loop {
        std::thread::sleep(POLL);

        if GENERATION.load(Ordering::SeqCst) != generation {
            return WatchOutcome::Superseded;
        }
        if started.elapsed() > WATCH_MAX {
            // 時間切れでも、直前までの手直しは拾ってから降りる。
            break;
        }

        // フォーカスが自分の欄から離れたか。**確定イベントとして扱う** —
        // ユーザーが欄を離れた時点で、その欄の編集は終わっている。
        if focus_moved.swap(false, Ordering::SeqCst) && !still_focused(&automation, &element) {
            break;
        }

        let last = last_change_ms.load(Ordering::SeqCst);
        if last == 0 {
            continue;
        }
        // デバウンス。Chromium の連投を 1 回の「確定」に畳む。
        if started.elapsed().as_millis() as u64 >= last + DEBOUNCE.as_millis() as u64 {
            // 読み返して差分が出ればそこで終わり。出なければ (自分の貼付の
            // 取りこぼしイベント等) 見張りを続ける。
            match finish(generation, &element, &baseline, pasted, register) {
                Finish::Learned(n) => return WatchOutcome::Learned(n),
                Finish::Lost => return WatchOutcome::Lost,
                Finish::Superseded => return WatchOutcome::Superseded,
                Finish::NoChange => {
                    last_change_ms.store(0, Ordering::SeqCst);
                    continue;
                }
                Finish::NoCandidate => return WatchOutcome::NoCandidate,
            }
        }
    }

    // ループをどう抜けたにせよ、**最後に必ず 1 回読み返す**。
    // 「TextChanged が 1 通も来なかった」と「本当に変わっていない」は
    // 別のことで、イベントを出さない相手 (自前描画のエディタなど) では
    // 前者が普通に起きる。読み返しは 2〜20ms なので、粘らずに 1 回だけ払う。
    match finish(generation, &element, &baseline, pasted, register) {
        Finish::Learned(n) => WatchOutcome::Learned(n),
        Finish::Lost => WatchOutcome::Lost,
        Finish::Superseded => WatchOutcome::Superseded,
        Finish::NoChange => WatchOutcome::NoEdit,
        Finish::NoCandidate => WatchOutcome::NoCandidate,
    }
}

/// 確定処理の結末。
#[cfg(windows)]
enum Finish {
    Learned(usize),
    NoCandidate,
    NoChange,
    Lost,
    /// 確定処理に入る直前に次の貼付へ追い越された。**何も書かない**。
    Superseded,
}

/// 対象要素を掴み、基準値 B0 を得る。
///
/// **貼ったテキストが中に見つかるまでリトライする。** `Ctrl+V` の送出から
/// 相手アプリの反映までにはラグがあるので、1 回読んで無いからといって
/// 諦めると、速い相手でしか学習できなくなる。
#[cfg(windows)]
fn acquire_baseline(
    automation: &IUIAutomation,
    generation: u64,
    pasted: &str,
    timeout: Duration,
) -> Option<(IUIAutomationElement, String)> {
    let deadline = Instant::now() + timeout;
    let needle = normalize_newlines(pasted);
    loop {
        if GENERATION.load(Ordering::SeqCst) != generation {
            return None;
        }
        // SAFETY: automation は有効。フォーカスが無ければ Err。
        if let Ok(element) = unsafe { automation.GetFocusedElement() } {
            // パスワード欄は絶対に読まない。deep context と同じ判定を通す。
            if crate::context::must_not_read(&element) {
                log::debug!("自動学習: フォーカスがパスワード欄なので学習しません");
                return None;
            }
            if let Some((text, source)) = read_learnable(&element) {
                if normalize_newlines(&text).contains(&needle) {
                    // 本文は出さない。長さと経路だけ。
                    log::debug!(
                        "自動学習: 基準値を取得しました ({} 文字 / {})",
                        text.chars().count(),
                        source.label()
                    );
                    return Some((element, text));
                }
            }
        }
        if Instant::now() >= deadline {
            // 相手が読めない・貼付が届いていない・タブが変わった。
            // **どれも失敗ではない**ので静かに降りる。
            return None;
        }
        std::thread::sleep(BASELINE_POLL);
    }
}

/// 本文を読み、**学習の対象にしてよい長さのときだけ**返す。
///
/// [`LEARN_READ_CAP`] の doc にあるとおり、上限に達した値は
/// 切り詰めずに丸ごと諦める。読み取りの入口をここ 1 つに絞ってあるので、
/// 基準値 (B0) と読み返し (B1) で判定がずれることがない。
#[cfg(windows)]
fn read_learnable(element: &IUIAutomationElement) -> Option<(String, crate::context::ContextSource)> {
    let (text, source) = crate::context::read_body(element, LEARN_READ_CAP)?;
    if text.chars().count() >= LEARN_READ_CAP {
        // 本文は出さない。長さと経路だけ。
        log::debug!(
            "自動学習: 欄が長すぎるので学習しません ({} 文字以上 / {})",
            LEARN_READ_CAP,
            source.label()
        );
        return None;
    }
    Some((text, source))
}

/// フォーカスがまだ同じ要素にあるか。
///
/// 要素が消えている (Chromium の非アクティブタブ・閉じたウィンドウ) と
/// 比較自体が Err になる。その場合も「離れた」とみなす。
#[cfg(windows)]
fn still_focused(automation: &IUIAutomation, element: &IUIAutomationElement) -> bool {
    // SAFETY: automation は有効。
    let Ok(current) = (unsafe { automation.GetFocusedElement() }) else {
        return false;
    };
    // SAFETY: 両方とも有効な要素。比較できなければ「別物」に倒す。
    unsafe { automation.CompareElements(element, &current) }
        .map(|b| b.as_bool())
        .unwrap_or(false)
}

/// 読み返して差分を取り、覚えるところまで。
#[cfg(windows)]
fn finish(
    generation: u64,
    element: &IUIAutomationElement,
    baseline: &str,
    pasted: &str,
    register: &dyn Fn(&[Candidate]) -> usize,
) -> Finish {
    // 追い越されていないか、**書き込む直前にもう一度**見る。
    // WATCH_MAX 到達やフォーカス離脱の直後に次の貼付が重なると、
    // ループの世代チェックをすり抜けた監視が設定を 1 回書いてしまう。
    if GENERATION.load(Ordering::SeqCst) != generation {
        return Finish::Superseded;
    }
    let Some((after, _source)) = read_learnable(element) else {
        // 要素が消えた / 欄が長すぎる。**どちらもエラーにしない** —
        // 非アクティブタブに入った Chromium の要素は木から消えるので、
        // 日常的に起きる。
        return Finish::Lost;
    };
    if normalize_newlines(&after) == normalize_newlines(baseline) {
        return Finish::NoChange;
    }

    let candidates = candidates_from_edit(baseline, &after, pasted);
    if candidates.is_empty() {
        log::debug!("自動学習: 手直しはありましたが候補になりませんでした");
        return Finish::NoCandidate;
    }
    match register(&candidates) {
        0 => Finish::NoCandidate,
        n => Finish::Learned(n),
    }
}

/// 候補を辞書へ入れ、UI へ知らせる。**実際に足せた件数**を返す。
#[cfg(windows)]
fn register(app: &tauri::AppHandle, candidates: &[Candidate]) -> usize {
    use tauri::{Emitter, Manager};

    let terms: Vec<(String, Option<String>)> = candidates
        .iter()
        .map(|c| (c.written.clone(), c.reading.clone()))
        .collect();

    let state = app.state::<crate::AppState>();
    let added = match state.config.add_auto_dictionary_entries(&terms) {
        Ok(added) => added,
        Err(e) => {
            // 設定が書けないのはユーザーに関係のある失敗だが、
            // **音声入力そのものは成立している**ので騒がない (ログのみ)。
            log::warn!("自動学習: 辞書を保存できません: {e}");
            return 0;
        }
    };
    if added.is_empty() {
        return 0;
    }

    // 設定画面が開いたままだと、画面の一覧は古い。そのまま保存されると
    // **いま覚えた語が消える** (UI は辞書を全置換で送る)。行を足させる。
    if let Err(e) = app.emit(EVENT_DICTIONARY_LEARNED, &added) {
        log::warn!("自動学習の通知を送出できません: {e}");
    }
    added.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 貼付 → 手直し の 1 往復を組み立てる。
    fn edit(before_tail: &str, pasted: &str, edited: &str) -> Vec<Candidate> {
        let before = format!("{before_tail}{pasted}");
        let after = format!("{before_tail}{edited}");
        candidates_from_edit(&before, &after, pasted)
    }

    fn written_of(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.written.as_str()).collect()
    }

    // --- 本筋: 直したところが候補になる ---

    #[test]
    fn a_corrected_proper_noun_becomes_a_candidate() {
        let found = edit(
            "本日の打ち合わせは",
            "塩屋さんと進めます",
            "塩谷さんと進めます",
        );
        assert_eq!(written_of(&found), vec!["塩谷"]);
        // 誤り側は漢字なので、読みにはできない。
        assert_eq!(found[0].reading, None);
        assert_eq!(found[0].wrong, "塩屋");
    }

    #[test]
    fn a_kana_misrecognition_fills_the_reading() {
        // 誤認識された側がかなだけ = **まさに聞こえた音**。
        // 読みに使えるのはこの形のときだけ。
        let found = edit("これは", "のっくすぼいすの話です", "nox-voiceの話です");
        assert_eq!(written_of(&found), vec!["nox-voice"]);
        assert_eq!(found[0].reading.as_deref(), Some("のっくすぼいす"));
    }

    #[test]
    fn an_edit_at_the_very_end_is_still_found() {
        // 覚えるのは `公園` ではなく `日比谷公園` — 漢字が地続きなら
        // そこまでが 1 語 ([`widen_to_word`])。`公園` だけを覚えても
        // 「日比谷公演」はまた同じように誤変換される。
        let found = edit("", "会場は日比谷公演", "会場は日比谷公園");
        assert_eq!(written_of(&found), vec!["日比谷公園"]);
    }

    #[test]
    fn a_fix_is_widened_to_the_word_but_not_into_a_different_script() {
        // 左が同じ字種なら巻き込む。
        let found = edit("本日は", "塩屋さんの件", "塩谷さんの件");
        assert_eq!(written_of(&found), vec!["塩谷"]);
        // 左がひらがなで新側が英数なら巻き込まない (地続きでも別の語)。
        let found = edit("これは", "のっくすぼいすの件", "nox-voiceの件");
        assert_eq!(written_of(&found), vec!["nox-voice"]);
    }

    #[test]
    fn several_fixes_in_one_edit_all_surface() {
        // **本当に 2 箇所直す。** 1 箇所しか直していないケースでは、
        // 「複数の修正が全部出る」ことを何も確かめられない。
        // 間に挟まった一致部分 (「さんが」「で待機します」) を
        // 手がかりに、置換対が 2 つに割れることを見る。
        let found = edit(
            "",
            "塩屋さんが東京都調で待機します",
            "塩谷さんが東京都庁で待機します",
        );
        assert_eq!(written_of(&found), vec!["塩谷", "東京都庁"]);
        assert_eq!(found[0].wrong, "塩屋");
        assert_eq!(found[1].wrong, "東京都調");
    }

    #[test]
    fn a_fix_just_outside_the_pasted_range_is_still_ignored() {
        // 貼付範囲の**すぐ外**を、同じ字種で地続きに直した場合。
        // 語へ広げた後で重なりを見ると、広がったせいで
        // 「貼った範囲に掛かっている」ことにされてしまう。
        // 実際に変わったのは貼付範囲の外なので、覚えてはいけない。
        let before = "旧住所は東京都調です。貼り付けた本文はこちら。";
        let after = "旧住所は東京都庁です。貼り付けた本文はこちら。";
        assert!(candidates_from_edit(before, after, "貼り付けた本文はこちら。").is_empty());
    }

    #[test]
    fn at_most_four_candidates_come_from_one_edit() {
        // Wispr Flow の公開仕様に合わせた上限。
        let before = "赤星 青葉 緑川 黄瀬 紫原 黒子";
        let after = "赤井 青野 緑谷 黄島 紫野 黒岩";
        let found = candidates_from_edit(before, after, before);
        assert!(
            found.len() <= MAX_CANDIDATES_PER_EDIT,
            "候補が多すぎる: {found:?}"
        );
    }

    // --- 貼った範囲の外は見ない ---

    #[test]
    fn an_edit_outside_the_pasted_range_is_ignored() {
        // 自分が書いていない文章の手直しを覚えてはいけない。
        let before = "前から在った文章です。貼り付けた本文はこちら。";
        let after = "前から有った文章です。貼り付けた本文はこちら。";
        assert!(candidates_from_edit(before, after, "貼り付けた本文はこちら。").is_empty());
    }

    #[test]
    fn an_edit_inside_the_pasted_range_is_kept_even_with_other_text_around() {
        let before = "前から在った文章です。塩屋さんの件。";
        let after = "前から在った文章です。塩谷さんの件。";
        let found = candidates_from_edit(before, after, "塩屋さんの件。");
        assert_eq!(written_of(&found), vec!["塩谷"]);
    }

    #[test]
    fn an_ambiguous_paste_is_not_learned_from() {
        // 貼った文字列が 2 か所にあると、どちらが自分の貼付か決められない。
        let before = "確認します。確認します。";
        let after = "確認します。カクニンします。";
        assert!(candidates_from_edit(before, after, "確認します。").is_empty());
    }

    #[test]
    fn a_paste_that_never_landed_yields_nothing() {
        // 貼付が届いていない = 基準値が信用できない。
        assert!(candidates_from_edit("別の文章", "別の文章", "貼ったはずの本文").is_empty());
    }

    #[test]
    fn no_edit_at_all_yields_nothing() {
        assert!(candidates_from_edit("塩屋さんの件", "塩屋さんの件", "塩屋さんの件").is_empty());
    }

    #[test]
    fn a_very_short_paste_is_not_watched() {
        assert!(candidates_from_edit("はい", "いいえ", "はい").is_empty());
    }

    #[test]
    fn carriage_returns_do_not_break_the_diff() {
        // 相手アプリが \r\n で持っていても差分が壊れないこと。
        let found = candidates_from_edit(
            "一行目\r\n塩屋さんの件です",
            "一行目\n塩谷さんの件です",
            "一行目\n塩屋さんの件です",
        );
        assert_eq!(written_of(&found), vec!["塩谷"]);
    }

    // --- 除外規則 ---

    #[test]
    fn pure_insertion_and_deletion_are_rejected() {
        assert_eq!(reject_reason("", "追加語"), Some(Reject::PureInsertOrDelete));
        assert_eq!(reject_reason("削除語", ""), Some(Reject::PureInsertOrDelete));
        // 実地でも: 語を足しただけの編集からは何も採らない。
        assert!(edit("", "会議の資料を確認します", "会議の資料を今日中に確認します").is_empty());
    }

    #[test]
    fn a_case_only_change_is_rejected() {
        assert_eq!(reject_reason("github", "GitHub"), Some(Reject::CaseOnly));
        assert_eq!(reject_reason("Tauri", "tauri"), Some(Reject::CaseOnly));
    }

    #[test]
    fn a_whitespace_only_change_is_rejected() {
        assert_eq!(reject_reason("nox voice", "nox  voice"), Some(Reject::WhitespaceOnly));
    }

    #[test]
    fn fillers_are_rejected() {
        assert_eq!(reject_reason("エート", "エートー"), Some(Reject::Filler));
    }

    #[test]
    fn a_hiragana_only_term_is_a_common_word() {
        // 助詞・活用語尾・和語の一般語がここに全部入る。ここを許すと
        // 「て・に・を・は」を延々覚え続ける。
        assert_eq!(reject_reason("これわ", "これは"), Some(Reject::CommonWord));
        assert_eq!(reject_reason("さくら", "すみれ"), Some(Reject::CommonWord));
    }

    #[test]
    fn a_single_character_term_is_rejected() {
        // 日本語の 1 文字は多義で、直したのか置き換えたのか分からない。
        assert_eq!(reject_reason("木", "気"), Some(Reject::TooShort));
    }

    #[test]
    fn a_short_katakana_term_is_a_common_word() {
        assert_eq!(reject_reason("アプ", "アピ"), Some(Reject::CommonWord));
        // 3 文字は残る (「アプリ」のような実在の語)。
        assert_eq!(reject_reason("アプリー", "アプリ"), None);
    }

    #[test]
    fn english_common_words_are_rejected() {
        assert_eq!(reject_reason("teh", "the"), Some(Reject::CommonWord));
        assert_eq!(reject_reason("adn", "and"), Some(Reject::CommonWord));
        // 固有名詞は残る。
        assert_eq!(reject_reason("tawri", "Tauri"), None);
    }

    #[test]
    fn japanese_common_words_are_rejected() {
        assert_eq!(reject_reason("確人", "確認"), Some(Reject::CommonWord));
        assert_eq!(reject_reason("時刊", "時間"), Some(Reject::CommonWord));
    }

    #[test]
    fn a_sentence_length_rewrite_is_rejected() {
        let long = "あ".repeat(MAX_TERM_CHARS + 1);
        assert_eq!(reject_reason("い", &long), Some(Reject::TooLong));
    }

    #[test]
    fn too_many_tokens_are_rejected() {
        assert_eq!(
            reject_reason("あ", "東京 大阪 名古屋 福岡 札幌"),
            Some(Reject::TooManyTokens)
        );
    }

    #[test]
    fn numerals_only_changes_are_rejected() {
        assert_eq!(reject_reason("13", "十三"), Some(Reject::NumeralsOnly));
    }

    #[test]
    fn symbols_only_changes_are_rejected() {
        assert_eq!(reject_reason("!!", "!?"), Some(Reject::NoContentChars));
    }

    #[test]
    fn multi_line_changes_are_rejected() {
        assert_eq!(reject_reason("あ", "一行目\n二行目"), Some(Reject::MultiLine));
    }

    #[test]
    fn url_like_values_are_rejected() {
        // 語ではないうえ、個人を特定しうる文字列がクラウドへ出ていく経路。
        assert_eq!(
            reject_reason("exemple.com", "https://example.com"),
            Some(Reject::UrlLike)
        );
        assert_eq!(
            reject_reason("nox@exemple.com", "nox@example.com"),
            Some(Reject::UrlLike)
        );
        assert_eq!(
            reject_reason(r"C:\tmp\a", r"C:\temp\a"),
            Some(Reject::UrlLike)
        );
        // スキームの無いドメインと Unix パスも塞ぐ。
        assert_eq!(
            reject_reason("exemple.com", "example.com"),
            Some(Reject::UrlLike)
        );
        assert_eq!(
            reject_reason("/home/user/secrets", "/home/user/secret"),
            Some(Reject::UrlLike)
        );
        // 巻き込みすぎないこと。バージョン番号は語の一部として残す。
        assert_eq!(reject_reason("GPT-4.O", "GPT-4.0"), None);
        assert_eq!(reject_reason("塩屋", "塩谷"), None);
    }

    #[test]
    fn an_unchanged_pair_is_rejected() {
        assert_eq!(reject_reason("同じ", "同じ"), Some(Reject::Unchanged));
        assert_eq!(reject_reason(" 同じ ", "同じ"), Some(Reject::Unchanged));
    }

    #[test]
    fn every_reject_reason_has_a_label() {
        for reason in [
            Reject::Unchanged,
            Reject::PureInsertOrDelete,
            Reject::CaseOnly,
            Reject::WhitespaceOnly,
            Reject::TooManyTokens,
            Reject::TooLong,
            Reject::TooShort,
            Reject::NoContentChars,
            Reject::NumeralsOnly,
            Reject::MultiLine,
            Reject::UrlLike,
            Reject::Filler,
            Reject::CommonWord,
        ] {
            assert!(!reason.label().is_empty(), "{reason:?}");
        }
    }

    #[test]
    fn every_watch_outcome_has_a_label() {
        for outcome in [
            WatchOutcome::Disabled,
            WatchOutcome::NotStarted,
            WatchOutcome::NoBaseline,
            WatchOutcome::NoEdit,
            WatchOutcome::NoCandidate,
            WatchOutcome::Learned(1),
            WatchOutcome::Lost,
            WatchOutcome::Superseded,
        ] {
            assert!(!outcome.label().is_empty(), "{outcome:?}");
        }
    }

    // --- トークン化 ---

    #[test]
    fn script_boundaries_split_tokens() {
        let chars: Vec<char> = "塩谷さんとnox-voiceのミーティング".chars().collect();
        let tokens: Vec<String> = tokenize(&chars)
            .iter()
            .map(|t| slice(&chars, t.start, t.end))
            .collect();
        assert_eq!(
            tokens,
            vec!["塩谷", "さんと", "nox-voice", "の", "ミーティング"]
        );
    }

    #[test]
    fn a_long_vowel_mark_stays_with_its_word() {
        let chars: Vec<char> = "コンピューター".chars().collect();
        assert_eq!(tokenize(&chars).len(), 1);
    }

    #[test]
    fn token_count_ignores_spaces_and_symbols() {
        assert_eq!(token_count("東京 大阪、名古屋"), 3);
        assert_eq!(token_count("nox-voice"), 1);
    }

    #[test]
    fn a_wholesale_rewrite_produces_no_hunks() {
        // 文章の作り直しから語は採れない。計算量の蓋も兼ねる。
        let old: Vec<char> = (0..MAX_DIFF_TOKENS + 10)
            .map(|i| char::from_u32(0x4E00 + i as u32).unwrap_or('あ'))
            .flat_map(|c| [c, 'の'])
            .collect();
        let new: Vec<char> = old.iter().rev().copied().collect();
        assert!(replacement_hunks(&old, &new).is_empty());
    }

    // --- 読みの判定 ---

    #[test]
    fn kana_only_detection() {
        assert!(is_kana_only("しおや"));
        assert!(is_kana_only("シオヤ"));
        assert!(is_kana_only("コンピューター"));
        assert!(!is_kana_only("塩谷"));
        assert!(!is_kana_only("nox"));
        assert!(!is_kana_only(""));
    }

    // --- 監視の状態 ---

    /// **実機で「効いているか」を自分で確かめるための診断。**
    ///
    /// 単体テストは差分と除外規則しか見られない。UIA が相手アプリの欄を
    /// 読み返せるか、イベントが飛んでくるか、デバウンスが効くかは
    /// **実機でしか分からない**。ここは本番と同じ [`watch_blocking`] を
    /// そのまま回し、覚える段だけを「画面に出す」へ差し替えてある
    /// (**設定ファイルには一切触らない**)。
    ///
    /// # 手順
    ///
    /// 1. 試したいアプリ (Chrome / VSCode / Slack など) の入力欄を開いておく
    /// 2. 下のコマンドを実行する
    /// 3. 30 秒以内に、その入力欄へ**課題文をそのまま**入力する
    ///    (貼り付けでよい。既定は「本日は塩屋さんと打ち合わせです」)
    /// 4. 「基準値を取得しました」が出たら、`塩屋` を `塩谷` に直す
    /// 5. 手を止めて 1.5 秒待つか、別の欄をクリックする
    /// 6. 候補として `塩谷` が出れば**この機能は効いている**
    ///
    /// 課題文は `NOX_LEARN_TEXT` で差し替えられる。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture live_learn_watch`
    #[cfg(windows)]
    #[test]
    #[ignore = "実機の入力欄を手で触る必要がある診断。最大 2 分かかる"]
    fn live_learn_watch() {
        let text = std::env::var("NOX_LEARN_TEXT")
            .unwrap_or_else(|_| "本日は塩屋さんと打ち合わせです".to_string());

        println!("== 辞書の自動学習の実機診断 ==");
        println!("1) 試したいアプリの入力欄をクリックしてください");
        println!("2) 次の文をそのまま入力 (または貼り付け) してください:");
        println!("     {text}");
        println!("3) 「基準値を取得しました」が出たら、誤りの語を直してください");
        println!("4) 手を止めて 1.5 秒待つか、別の欄をクリックすると確定します");
        println!("   (30 秒以内に入力が見つからなければ「基準値を取れず」で終わります)\n");

        let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        // 覚える段の差し替え。**候補は画面に出してよい** — 本番でも
        // 設定画面に出るものなので、ここだけの秘密ではない。
        let report = |candidates: &[Candidate]| {
            println!("\n候補 {} 件:", candidates.len());
            for c in candidates {
                println!(
                    "    {:?} → {:?}  読み={:?}",
                    c.wrong, c.written, c.reading
                );
            }
            candidates.len()
        };

        let started = Instant::now();
        let outcome = watch_blocking(generation, &text, Duration::from_secs(30), &report);
        println!(
            "\n結末: {} ({:?}) / 所要 {:?}",
            outcome.label(),
            outcome,
            started.elapsed()
        );

        // 何が起きても panic しないこと、時間内に必ず戻ることを見る。
        // 「学習した」まで求めると、手順どおり操作しないと落ちるテストになる。
        assert!(
            started.elapsed() < Duration::from_secs(30) + WATCH_MAX + Duration::from_secs(5),
            "監視が時間内に降りていない: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn abandoning_bumps_the_generation() {
        // 新しい貼付が来たら古い監視は手を引く。世代が動かないと、
        // 次の貼付そのものを「ユーザーの手直し」として学習してしまう。
        let before = GENERATION.load(Ordering::SeqCst);
        abandon_all();
        assert!(GENERATION.load(Ordering::SeqCst) > before);
    }
}
