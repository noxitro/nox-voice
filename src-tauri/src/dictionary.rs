//! ユーザー辞書 — 固有名詞・専門用語の表記を安定させる。
//!
//! 1 語 = [`DictionaryEntry`] 1 件。表記・読みに加えて、**優先度 (★) /
//! 追加日時 / 出所 (手動 / 自動候補)** を持つ。旧形式 (`Vec<String>` の
//! `表記` / `表記,よみ` 行) も読める ([`DictionaryEntry`] の
//! `Deserialize` 実装が 1 行を 1 件へ引き上げる)。
//!
//! 辞書は 2 か所で使う:
//!
//! - **STT (Whisper) の `prompt`**: 音になる前の段階で表記を寄せる。
//!   誤変換をそもそも起こさせないので、整形での修復より確実。
//! - **整形 (Gemini) のプロンプト**: 転写後の表記ゆれを直す。
//!   `よみ` があれば「よみ → 表記」の変換指示として渡せる。
//!
//! **読みは STT へは渡せない**。Whisper は end-to-end のモデルで、
//! 「この音はこの表記」という対応表を受け取る口が無い (`prompt` は
//! ただのテキスト)。読みが効くのは整形側だけ。
//!
//! # `prompt` の予算と語の選抜 (2026-08-30)
//!
//! Whisper の `prompt` は **224 トークン**しか見ない。超過分は
//! **先頭から黙って捨てられる**。日本語は実測で約 **1.05 トークン/文字**
//! (公式の `multilingual.tiktoken` で計測) なので、223 トークンに入る
//! 辞書語は日本語でおよそ 36 語。
//!
//! **枠に入らない語を無言で捨ててはいけない。** 以前の実装は登録順に
//! 先勝ちで詰めて、溢れた語を `continue` で捨てていた。リストの末尾 =
//! **今しがた登録した語**ほど落ちるので、「効かないから登録したのに、
//! その語が真っ先に捨てられる」という逆転が起きていた。
//!
//! いまは [`select_dictionary_terms`] が**優先度で枠を配分**し、
//! 溢れた語は [`DictionaryStatus`] として設定画面に出す (黙って捨てない)。
//!
//! # 自動追加語との同居 (2026-08-30)
//!
//! [`crate::learn`] が貼付後の手直しから語を勝手に足す。同じ枠を
//! 奪い合わせると、**ユーザーが自分で登録した語が黙って押し出される** —
//! しかもユーザーは何もしていないので理由を知りようがない。
//! そこで [`rank_key`] が「手動 > 自動」を★の次の順位に置き、
//! 自動追加語は**余った枠だけ**を使う。件数自体も
//! [`DICTIONARY_AUTO_MAX_ENTRIES`] で頭打ちにする。
//!
//! **枠が余っても語を増やせばよいわけではない。** 語を入れすぎると
//! 句読点・言語の自動判定・整形全体が引きずられる (superwhisper の
//! 公式ガイダンス、CB-Whisper 論文の MER 悪化)。実効レンジは 20〜50 語で、
//! [`DICTIONARY_MAX_TERMS`] はその上端に置いてある。

use serde::{Deserialize, Deserializer, Serialize};

/// `prompt` 全体に使える文字数。
///
/// 上限は 224 トークン (末尾 223 + `sot_prev`)。日本語 1.05 トークン/文字の
/// 実測に対し 190 文字 ≒ 200 トークンで、23 トークンぶんを安全余裕として
/// 残す。**区切りの「、」もトークンを食う**ので、余裕は見た目より薄い。
/// かつては 150 文字だったが、これは実測の約 2 倍保守的だった。
pub const WHISPER_PROMPT_MAX_CHARS: usize = 190;

/// そのうち辞書が使ってよい文字数。**画面コンテキストとは独立**に決める。
///
/// [`build_whisper_prompt`] は同じ予算を画面コンテキストと分け合う。
/// 辞書側を「残り全部」にすると、辞書が伸びた日にコンテキストが消え、
/// 逆に辞書の実効上限が「コンテキストがあるかどうか」で毎回変わる。
/// **設定画面に「上限 N 語」と書く以上、その N は日によって変わっては
/// いけない**ので、辞書は常にこの枠内で選抜する。
/// 残り (190 − 140 − 1 = 49 文字) がコンテキストの取り分。
pub const DICTIONARY_PROMPT_MAX_CHARS: usize = 140;

/// `prompt` へ載せる語数の上限。
///
/// 文字数の枠 ([`DICTIONARY_PROMPT_MAX_CHARS`]) とは別に語数でも切る。
/// 短い語ばかりだと文字数の枠には 50 語以上入ってしまうが、**語を
/// 入れすぎること自体が転写を悪くする** (モジュール doc)。
pub const DICTIONARY_MAX_TERMS: usize = 36;

/// 自動追加された語を辞書に何件まで溜めるか。
///
/// **`prompt` の枠 ([`DICTIONARY_MAX_TERMS`]) とは別の話**。こちらは
/// 「設定ファイルと設定画面の一覧が無限に伸びない」ための上限で、
/// 溢れたら**古い自動追加語から捨てる** ([`merge_auto_entries`])。
///
/// 枠より大きくしてあるのは、載らなかった語にも意味があるため —
/// ユーザーが★を付ければ即座に載るし、「自動追加」の一覧は
/// 「アプリが何を学んだか」を確かめる場所でもある。
/// 手動登録語はここで数えないし、捨てもしない。
pub const DICTIONARY_AUTO_MAX_ENTRIES: usize = 60;

/// 辞書 1 語がどこから来たか。
///
/// **出所は表示のためだけのものではない。** [`rank_key`] が枠の配分で
/// 手動登録語を自動追加語より上に置くので、ここを取り違えると
/// 「勝手に覚えた語がユーザーの登録した語を押し出す」が起きる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictionaryOrigin {
    /// ユーザーが自分で登録した。
    #[default]
    Manual,
    /// アプリが自動で覚えた ([`crate::learn`] — 貼付後の手直しから抽出)。
    Auto,
}

/// 辞書 1 語。
///
/// `Deserialize` は**手書き**。旧形式の設定ファイル (`["nox-voice",
/// "塩谷,しおや"]`) をそのまま読めるようにするため、文字列 1 行も
/// 受け付ける ([`DictionaryEntry::deserialize`])。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DictionaryEntry {
    /// 出力してほしい表記。**これだけが Whisper の `prompt` へ行く**。
    pub written: String,
    /// 読み (任意)。整形プロンプトでだけ使う (STT へは渡せない)。
    pub reading: Option<String>,
    /// ユーザーが「絶対に効かせたい」と指定した語か (Wispr Flow の★相当)。
    ///
    /// 枠の配分で最優先される。ここが**ユーザーが手で握れる唯一のつまみ**
    /// なので、他の要素 (新しさ) が何をしようとこれが勝つ。
    pub pinned: bool,
    /// 追加日時 (UNIX epoch ミリ秒)。**0 は「不明」**。
    ///
    /// 新しい語ほど枠で有利にするために持つ。使用頻度だけで選抜すると
    /// **使用実績ゼロの新規登録語が真っ先に溢れる**が、それは
    /// 「効かないから登録した」語であって、一番落としてはいけない。
    /// 0 のまま残さないよう [`crate::config::Config::normalize`] が
    /// 読み込み時に現在時刻を入れる (旧形式からの移行分は横並びになり、
    /// 同着は登録順で解ける)。
    pub added_at_ms: i64,
    /// 出所 (手動 / 自動候補)。
    pub origin: DictionaryOrigin,
}

impl DictionaryEntry {
    /// 手動登録の 1 語を作る (日時は未設定 = 0)。
    pub fn new(written: impl Into<String>) -> Self {
        Self {
            written: written.into(),
            reading: None,
            pinned: false,
            added_at_ms: 0,
            origin: DictionaryOrigin::Manual,
        }
    }

    /// 整形プロンプトへ 1 行で書ける形にする。
    pub fn as_instruction(&self) -> String {
        match &self.reading {
            Some(reading) => format!("{}(よみ: {}) ← この音は必ずこの表記にする", self.written, reading),
            None => self.written.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for DictionaryEntry {
    /// 旧形式 (文字列 1 行) と新形式 (オブジェクト) の両方を受ける。
    ///
    /// **移行を専用の処理として書かない**のが要点。ここで吸収しておけば、
    /// 設定ファイル・IPC のパッチ・テストのどこから来ても同じ形になる。
    /// 各フィールドが `#[serde(default)]` なのも同じ理由で、
    /// 「新しい項目が無い JSON」を落とさずに読むため
    /// (`config.rs` のコンテナ default の罠と対になる注意)。
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            written: String,
            #[serde(default)]
            reading: Option<String>,
            #[serde(default)]
            pinned: bool,
            #[serde(default)]
            added_at_ms: i64,
            #[serde(default)]
            origin: DictionaryOrigin,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            /// 旧形式: `"表記"` / `"表記,よみ"`。
            Line(String),
            Full(Fields),
        }

        Ok(match Raw::deserialize(d)? {
            // 解釈できない行 (空行など) は空の表記で通し、normalize に
            // 捨てさせる。ここでエラーにすると**設定ファイル全体が
            // 読めなくなり、API キーごと既定へ倒れる**。
            Raw::Line(line) => parse_entry(&line).unwrap_or_else(|| DictionaryEntry::new("")),
            Raw::Full(f) => DictionaryEntry {
                written: f.written,
                reading: f.reading.filter(|r| !r.trim().is_empty()),
                pinned: f.pinned,
                added_at_ms: f.added_at_ms,
                origin: f.origin,
            },
        })
    }
}

/// 旧形式の生の行をまとめて辞書エントリへ変換する。**テスト専用**。
///
/// 本番の移行経路は [`DictionaryEntry`] の `Deserialize` 1 本
/// (行を 1 件へ引き上げるのは下の [`parse_entry`])。ここを本番から
/// も呼べる形で残すと、移行の入口が 2 つになって片方だけ直す事故が起きる。
#[cfg(test)]
pub fn parse_entries(lines: &[String]) -> Vec<DictionaryEntry> {
    lines
        .iter()
        .filter_map(|line| parse_entry(line))
        .collect()
}

fn parse_entry(line: &str) -> Option<DictionaryEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // 半角・全角どちらのカンマでも読みの区切りとみなす。
    let split = line.find(',').or_else(|| line.find('，'));
    let (written, reading) = match split {
        Some(idx) => {
            let (w, r) = line.split_at(idx);
            // 区切り文字ぶんを飛ばす (全角は 3 バイト)。
            let r = r.strip_prefix(',').or_else(|| r.strip_prefix('，')).unwrap_or("");
            (w.trim(), r.trim())
        }
        None => (line, ""),
    };

    if written.is_empty() {
        return None;
    }
    Some(DictionaryEntry {
        reading: (!reading.is_empty()).then(|| reading.to_string()),
        ..DictionaryEntry::new(written)
    })
}

/// 語の選抜結果。添字は渡した並びに対するもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionarySelection {
    /// `prompt` へ載せる語 (**並べる順**。優先度の低いものが先頭)。
    pub kept: Vec<usize>,
    /// 枠から溢れた語 (登録順)。**表記が空の語は含まない** —
    /// あれは「溢れた」のではなく「語ですらない」。
    pub dropped: Vec<usize>,
    /// [`kept`](Self::kept) を「、」で繋いだときの文字数。
    pub used_chars: usize,
}

/// 設定画面へ返す「いま何語が効いているか」。
///
/// **黙って捨てないための型**。`in_prompt` は渡した並びと同じ長さ・同じ
/// 順で、行と 1 対 1 に対応する (UI が添字で引ける)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DictionaryStatus {
    /// 語数の上限 ([`DICTIONARY_MAX_TERMS`])。
    pub max_terms: usize,
    /// 文字数の上限 ([`DICTIONARY_PROMPT_MAX_CHARS`])。
    pub max_chars: usize,
    /// 実際に `prompt` へ載る語数。
    pub used_terms: usize,
    /// 実際に使う文字数 (区切りの「、」込み)。
    pub used_chars: usize,
    /// 行ごとの「載っているか」。
    pub in_prompt: Vec<bool>,
}

impl DictionaryStatus {
    pub fn of(entries: &[DictionaryEntry]) -> Self {
        let selection = select_dictionary_terms(entries);
        let mut in_prompt = vec![false; entries.len()];
        for &i in &selection.kept {
            in_prompt[i] = true;
        }
        Self {
            max_terms: DICTIONARY_MAX_TERMS,
            max_chars: DICTIONARY_PROMPT_MAX_CHARS,
            used_terms: selection.kept.len(),
            used_chars: selection.used_chars,
            in_prompt,
        }
    }
}

/// 選抜の順位。**大きいほど残る**。
///
/// 要素は 4 つだけにしてある:
///
/// 1. `pinned` — ユーザーが手で決めた優先度。他の何にも負けない。
///    自動追加語に★を付ければ手動登録語より上に来る (**それでよい** —
///    ★はユーザーの明示の操作で、出所より新しい意思表示だから)
/// 2. **手動登録か** — 自動追加語は手動登録語を**決して押し出さない**。
///    自動学習 ([`crate::learn`]) は黙って語を増やすので、ここを入れないと
///    「1 日使ったら自分で登録した語が全部枠外になっていた」が起きる。
///    これは「黙って捨てる」の最悪の形 — ユーザーは自分が何もしていない
///    のに辞書が効かなくなった理由を知りようがない
/// 3. `added_at_ms` — **新しいほど強い**。使用頻度で並べると使用実績ゼロの
///    新規登録語が真っ先に溢れるが、それは本末転倒 (フィールドの doc)
/// 4. 登録順の添字 — 同着の解決。日時が 0 (不明) の旧形式が横並びに
///    なったとき、**後から書いた行ほど新しい**とみなす
///
/// 「直近の使用頻度」は**入れていない**。履歴を語で走査するにせよ
/// カウンタを足すにせよ、上の要素で足りない証拠が出るまでは
/// 履歴スキーマを増やす理由が無い (design.md 2026-08-30)。
fn rank_key(index: usize, entry: &DictionaryEntry) -> (u8, u8, i64, usize) {
    (
        u8::from(entry.pinned),
        u8::from(entry.origin == DictionaryOrigin::Manual),
        entry.added_at_ms,
        index,
    )
}

/// 自動学習で得た語を既存の辞書へ足す。**純関数に近い手続き** (時刻だけ外から)。
///
/// ここが「勝手に増える」を抑える唯一の場所なので、判断を全部集めてある:
///
/// - **既にある表記は足さない。** 手動・自動どちらの重複も見る。
///   同じ誤変換を何度直しても行が増えないようにするため
/// - 比較は `trim` + ASCII の大文字小文字を無視。`Nox-Voice` と `nox-voice`
///   を別の語として 2 行持つ意味は無い
/// - 表記が空の語は無視する ([`Config::normalize`](crate::config::Config::normalize)
///   がどのみち捨てるが、捨てた語を「追加した」と報告してしまうのを防ぐ)
/// - **自動追加語が [`DICTIONARY_AUTO_MAX_ENTRIES`] を超えたら古い方から捨てる。**
///   捨てるのは**自動追加語だけ**で、手動登録語には指一本触れない
///
/// 戻り値は**実際に足した語**。呼び出し側 (UI への通知・ログ) は
/// これを見る。「追加しようとした語」を報告すると、重複で足されなかった
/// ときに UI が嘘をつく。
pub fn merge_auto_entries(
    entries: &mut Vec<DictionaryEntry>,
    terms: &[(String, Option<String>)],
    now_ms: i64,
) -> Vec<DictionaryEntry> {
    let mut added: Vec<DictionaryEntry> = Vec::new();
    for (written, reading) in terms {
        let written = written.trim();
        if written.is_empty() {
            continue;
        }
        let exists = entries
            .iter()
            .chain(added.iter())
            .any(|e| e.written.trim().eq_ignore_ascii_case(written));
        if exists {
            continue;
        }
        added.push(DictionaryEntry {
            written: written.to_string(),
            reading: reading
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_string),
            pinned: false,
            added_at_ms: now_ms,
            origin: DictionaryOrigin::Auto,
        });
    }
    entries.extend(added.iter().cloned());
    evict_old_auto_entries(entries);
    added
}

/// 自動追加語が上限を超えたぶんを古い方から落とす。**手動登録語は触らない**。
///
/// # ★ を付けた自動追加語は上限より優先する
///
/// 捨てる候補から★を外してあるので、**自動追加語を 60 件すべて★にすると
/// 件数は上限を超えて伸びる**。これは意図した挙動。★は「この語は残す」と
/// ユーザーが手で言ったことであり、内部の都合 (一覧が伸びない) のために
/// それを黙って消すのは、この機能全体で避けている「無言で捨てる」そのもの。
/// 上限は**アプリが勝手に増やしたぶん**を抑えるためのものなので、
/// ユーザーが明示的に残した語には効かせない。
fn evict_old_auto_entries(entries: &mut Vec<DictionaryEntry>) {
    let auto_count = entries
        .iter()
        .filter(|e| e.origin == DictionaryOrigin::Auto)
        .count();
    let Some(excess) = auto_count.checked_sub(DICTIONARY_AUTO_MAX_ENTRIES) else {
        return;
    };
    if excess == 0 {
        return;
    }
    // 古い順 = (日時, 登録順) の小さい方から。★が付いた語は
    // ユーザーが明示的に残したいと言った語なので、捨てる対象から外す。
    let mut victims: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.origin == DictionaryOrigin::Auto && !e.pinned)
        .map(|(i, _)| i)
        .collect();
    victims.sort_by_key(|&i| (entries[i].added_at_ms, i));
    victims.truncate(excess);
    victims.sort_unstable();
    for &i in victims.iter().rev() {
        entries.remove(i);
    }
}

/// `prompt` へ載せる語を優先度で選ぶ。**純関数**。
///
/// 予算を超える語に当たっても**打ち切らずに次を見る**。長い語が 1 つ
/// 上位にあるだけで、下位の短い語が全部落ちてしまうのを避けるため。
pub fn select_dictionary_terms(entries: &[DictionaryEntry]) -> DictionarySelection {
    let mut order: Vec<usize> = (0..entries.len()).collect();
    // 順位の高い順に見る。ここが「登録順で先勝ち」をやめた本体。
    order.sort_by(|&a, &b| rank_key(b, &entries[b]).cmp(&rank_key(a, &entries[a])));

    let mut kept: Vec<usize> = Vec::new();
    let mut dropped: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for index in order {
        let len = entries[index].written.trim().chars().count();
        if len == 0 {
            // 表記が空の行は選抜の対象ですらない (溢れたとも言わない)。
            continue;
        }
        // 2 語目以降は区切りの「、」も予算を食う。
        let extra = if kept.is_empty() { len } else { len + 1 };
        if kept.len() >= DICTIONARY_MAX_TERMS || used + extra > DICTIONARY_PROMPT_MAX_CHARS {
            dropped.push(index);
            continue;
        }
        used += extra;
        kept.push(index);
    }

    // 並べ直す: **優先度の低いものを先頭に**。予算の見積もり (文字数 →
    // トークン) を外して API 側に切られたとき、削られるのが先頭側なので、
    // 一番落としたくない語 (★・新しい語) を末尾に置く。
    kept.sort_by_key(|&i| rank_key(i, &entries[i]));
    dropped.sort_unstable();
    DictionarySelection {
        kept,
        dropped,
        used_chars: used,
    }
}

/// Whisper の `prompt` を組み立てる。
///
/// # 並び順が仕様
///
/// 上限を超えた分は API 側で**先頭から**捨てられる。したがって
/// **落としたくないものほど後ろに置く**。ここでは
/// 「画面コンテキスト → 辞書」の順にして、溢れたときに削れるのが
/// 画面コンテキスト側になるようにしている (優先度: 辞書 > コンテキスト)。
///
/// 自前でも文字数を切り詰めるのは、サーバ側の切り捨てに頼ると
/// 「境界で語が割れて別の語になる」事故が起きるため。
pub fn build_whisper_prompt(entries: &[DictionaryEntry], context: Option<&str>) -> Option<String> {
    let selection = select_dictionary_terms(entries);
    let dictionary = selection
        .kept
        .iter()
        .map(|&i| entries[i].written.trim())
        .collect::<Vec<_>>()
        .join("、");
    let dict_len = dictionary.chars().count();

    let context = context
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(collapse_whitespace);

    match (dictionary.is_empty(), context) {
        (true, None) => None,
        (true, Some(ctx)) => {
            // 辞書が無い日はコンテキストが全部使ってよい。
            let ctx = take_last_chars(&ctx, WHISPER_PROMPT_MAX_CHARS);
            (!ctx.is_empty()).then_some(ctx)
        }
        (false, None) => Some(dictionary),
        (false, Some(ctx)) => {
            // 辞書のぶんを引いた残りにコンテキストを入れる。辞書は
            // DICTIONARY_PROMPT_MAX_CHARS までなので、残りは必ず正。
            let remaining = WHISPER_PROMPT_MAX_CHARS.saturating_sub(dict_len + 1);
            if remaining == 0 {
                return Some(dictionary);
            }
            let ctx = take_last_chars(&ctx, remaining);
            if ctx.is_empty() {
                Some(dictionary)
            } else {
                Some(format!("{ctx} {dictionary}"))
            }
        }
    }
}

/// 末尾から `max` 文字を取る (文字境界で切る)。
fn take_last_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

/// 連続する空白・改行を 1 個のスペースに畳む。
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 日時と★を明示して 1 語作る (選抜のテスト用)。
    fn entry(written: &str, pinned: bool, added_at_ms: i64) -> DictionaryEntry {
        DictionaryEntry {
            pinned,
            added_at_ms,
            ..DictionaryEntry::new(written)
        }
    }

    fn written_of(entries: &[DictionaryEntry], indexes: &[usize]) -> Vec<String> {
        indexes.iter().map(|&i| entries[i].written.clone()).collect()
    }

    // --- パース ---

    #[test]
    fn plain_terms_have_no_reading() {
        let entries = parse_entries(&lines(&["nox-voice", "Tauri"]));
        assert_eq!(
            entries,
            vec![DictionaryEntry::new("nox-voice"), DictionaryEntry::new("Tauri")]
        );
    }

    #[test]
    fn a_reading_can_follow_a_comma() {
        let entries = parse_entries(&lines(&["塩谷,しおや"]));
        assert_eq!(entries[0].written, "塩谷");
        assert_eq!(entries[0].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn full_width_comma_also_separates() {
        // 日本語入力では全角カンマの方が打ちやすい。
        let entries = parse_entries(&lines(&["塩谷，しおや"]));
        assert_eq!(entries[0].written, "塩谷");
        assert_eq!(entries[0].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn surrounding_space_is_trimmed() {
        let entries = parse_entries(&lines(&["  塩谷 , しおや  "]));
        assert_eq!(entries[0].written, "塩谷");
        assert_eq!(entries[0].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn blank_lines_and_empty_written_forms_are_dropped() {
        let entries = parse_entries(&lines(&["", "   ", ",よみだけ", "有効"]));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].written, "有効");
    }

    #[test]
    fn an_empty_reading_is_treated_as_absent() {
        let entries = parse_entries(&lines(&["表記,", "表記2,  "]));
        assert!(entries.iter().all(|e| e.reading.is_none()));
    }

    #[test]
    fn instruction_line_mentions_the_reading_only_when_present() {
        let with = DictionaryEntry {
            reading: Some("しおや".into()),
            ..DictionaryEntry::new("塩谷")
        };
        assert!(with.as_instruction().contains("しおや"));
        assert!(with.as_instruction().contains("塩谷"));
        let without = DictionaryEntry::new("Tauri");
        assert_eq!(without.as_instruction(), "Tauri");
    }

    #[test]
    fn a_new_entry_is_manual_and_unpinned() {
        // 出所の既定を取り違えると、後で足す「自動候補」の絞り込みが
        // 手動登録まで拾ってしまう。
        let e = DictionaryEntry::new("nox-voice");
        assert_eq!(e.origin, DictionaryOrigin::Manual);
        assert!(!e.pinned);
        assert_eq!(e.added_at_ms, 0);
    }

    // --- 旧形式からの移行 (serde) ---

    #[test]
    fn old_string_lines_deserialize_into_entries() {
        // 旧 config.json は `"dictionary": ["nox-voice", "塩谷,しおや"]`。
        let entries: Vec<DictionaryEntry> =
            serde_json::from_str(r#"["nox-voice", "塩谷,しおや"]"#).expect("読める");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].written, "nox-voice");
        assert_eq!(entries[0].reading, None);
        assert_eq!(entries[1].written, "塩谷");
        assert_eq!(entries[1].reading.as_deref(), Some("しおや"));
        // 移行分は手動・未★・日時不明で始まる。
        assert!(entries.iter().all(|e| e.origin == DictionaryOrigin::Manual));
        assert!(entries.iter().all(|e| !e.pinned && e.added_at_ms == 0));
    }

    #[test]
    fn an_unparsable_old_line_does_not_fail_the_whole_file() {
        // ここでエラーにすると設定ファイルが丸ごと読めなくなり、
        // API キーごと既定へ倒れる。空の表記で通して normalize に捨てさせる。
        let entries: Vec<DictionaryEntry> =
            serde_json::from_str(r#"["", "有効"]"#).expect("読める");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].written, "");
        assert_eq!(entries[1].written, "有効");
    }

    #[test]
    fn new_objects_deserialize_with_missing_fields_defaulted() {
        // 項目が増える前に書かれたオブジェクトも読めること。
        let entries: Vec<DictionaryEntry> =
            serde_json::from_str(r#"[{"written":"nox-voice"}]"#).expect("読める");
        assert_eq!(entries[0], DictionaryEntry::new("nox-voice"));
    }

    #[test]
    fn a_full_object_round_trips() {
        let original = DictionaryEntry {
            written: "塩谷".into(),
            reading: Some("しおや".into()),
            pinned: true,
            added_at_ms: 1_700_000_000_000,
            origin: DictionaryOrigin::Auto,
        };
        let json = serde_json::to_string(&original).expect("書ける");
        assert!(json.contains("\"origin\":\"auto\""), "{json}");
        let back: DictionaryEntry = serde_json::from_str(&json).expect("読める");
        assert_eq!(back, original);
    }

    #[test]
    fn a_mixed_array_reads_both_shapes() {
        // 移行の途中 (手で編集した設定ファイル) でも壊れないこと。
        let entries: Vec<DictionaryEntry> =
            serde_json::from_str(r#"["旧形式", {"written":"新形式","pinned":true}]"#)
                .expect("読める");
        assert_eq!(entries[0].written, "旧形式");
        assert!(!entries[0].pinned);
        assert_eq!(entries[1].written, "新形式");
        assert!(entries[1].pinned);
    }

    // --- 語の選抜 ---

    #[test]
    fn everything_fits_when_the_dictionary_is_small() {
        let entries = parse_entries(&lines(&["nox-voice", "Tauri"]));
        let selection = select_dictionary_terms(&entries);
        // 収まるときは登録順のまま。並べ替えて見せる理由が無い。
        assert_eq!(selection.kept, vec![0, 1]);
        assert!(selection.dropped.is_empty());
    }

    #[test]
    fn pinned_terms_survive_a_full_budget() {
        // ★ を付けた語は、どれだけ古くても、どれだけ後ろにあっても残る。
        let mut entries: Vec<DictionaryEntry> = (0..60)
            .map(|i| entry(&format!("語{i:03}"), false, 2_000 + i as i64))
            .collect();
        entries.push(entry("絶対に効かせたい語", true, 1));
        let selection = select_dictionary_terms(&entries);
        let last = *selection.kept.last().expect("1 語はある");
        assert_eq!(entries[last].written, "絶対に効かせたい語");
        assert!(!selection.dropped.contains(&(entries.len() - 1)));
    }

    #[test]
    fn a_freshly_added_term_is_not_the_first_to_be_dropped() {
        // ここが今回の修正の核心。旧実装は登録順の先勝ちだったので、
        // **末尾 = 今登録した語**が真っ先に落ちていた。
        let mut entries: Vec<DictionaryEntry> = (0..60)
            .map(|i| entry(&format!("既存語{i:03}"), false, 1_000 + i as i64))
            .collect();
        entries.push(entry("今登録した語", false, 9_999));
        let selection = select_dictionary_terms(&entries);
        assert!(
            selection.kept.contains(&(entries.len() - 1)),
            "新しく登録した語が溢れた: {selection:?}"
        );
        // 溢れるのは一番古い側。
        assert!(selection.dropped.contains(&0));
    }

    #[test]
    fn ties_on_an_unknown_timestamp_fall_back_to_registration_order() {
        // 旧形式からの移行直後は日時が横並びになる。そこでの同着は
        // 「後から書いた行ほど新しい」で解く。
        let entries: Vec<DictionaryEntry> = (0..60)
            .map(|i| entry(&format!("語{i:03}"), false, 0))
            .collect();
        let selection = select_dictionary_terms(&entries);
        assert!(selection.kept.contains(&(entries.len() - 1)));
        assert!(selection.dropped.contains(&0));
    }

    #[test]
    fn the_term_count_is_capped_even_when_the_characters_fit() {
        // 1 文字の語ばかりだと文字数の枠には 70 語入るが、
        // **語を入れすぎること自体が転写を悪くする**。
        let entries: Vec<DictionaryEntry> = (0..80)
            .map(|i| entry(&char::from(b'a' + (i % 26) as u8).to_string(), false, i as i64))
            .collect();
        let selection = select_dictionary_terms(&entries);
        assert_eq!(selection.kept.len(), DICTIONARY_MAX_TERMS);
        assert_eq!(selection.dropped.len(), 80 - DICTIONARY_MAX_TERMS);
    }

    #[test]
    fn the_selection_stays_within_the_character_budget() {
        let entries: Vec<DictionaryEntry> = (0..100)
            .map(|i| entry(&format!("用語{i:03}"), false, i as i64))
            .collect();
        let selection = select_dictionary_terms(&entries);
        assert!(selection.used_chars <= DICTIONARY_PROMPT_MAX_CHARS);
        // 落ちた語は 1 件残らず dropped に出る (無言で消えない)。
        assert_eq!(selection.kept.len() + selection.dropped.len(), entries.len());
    }

    #[test]
    fn an_oversized_term_does_not_drop_the_rest() {
        // 長い語で打ち切ると、順位が下の短い語が巻き添えで全滅する。
        let entries = vec![
            entry(&"あ".repeat(DICTIONARY_PROMPT_MAX_CHARS + 10), false, 9),
            entry("短語", false, 8),
            entry("別語", false, 7),
        ];
        let selection = select_dictionary_terms(&entries);
        assert_eq!(written_of(&entries, &selection.kept), vec!["別語", "短語"]);
        assert_eq!(selection.dropped, vec![0]);
    }

    #[test]
    fn blank_written_forms_are_neither_kept_nor_reported_as_dropped() {
        // 空行は「溢れた」のではない。溢れとして数えると、UI が
        // 「3 語が枠外です」と嘘をつく。
        let entries = vec![entry("有効", false, 1), entry("   ", false, 2)];
        let selection = select_dictionary_terms(&entries);
        assert_eq!(selection.kept, vec![0]);
        assert!(selection.dropped.is_empty());
    }

    #[test]
    fn the_status_maps_one_to_one_onto_the_rows() {
        let mut entries: Vec<DictionaryEntry> = (0..60)
            .map(|i| entry(&format!("語{i:03}"), false, 1_000 + i as i64))
            .collect();
        entries.push(entry("", false, 0));
        let status = DictionaryStatus::of(&entries);
        assert_eq!(status.in_prompt.len(), entries.len());
        assert_eq!(status.max_terms, DICTIONARY_MAX_TERMS);
        assert_eq!(status.max_chars, DICTIONARY_PROMPT_MAX_CHARS);
        assert_eq!(
            status.used_terms,
            status.in_prompt.iter().filter(|x| **x).count()
        );
        assert!(status.used_terms < entries.len(), "溢れが出ていない");
        // 空の行は載らない。
        assert!(!status.in_prompt[entries.len() - 1]);
    }

    // --- 自動追加語と手動登録語の同居 ---

    /// 日時と出所を明示して 1 語作る。
    fn auto(written: &str, added_at_ms: i64) -> DictionaryEntry {
        DictionaryEntry {
            added_at_ms,
            origin: DictionaryOrigin::Auto,
            ..DictionaryEntry::new(written)
        }
    }

    #[test]
    fn auto_terms_never_push_out_a_manual_one() {
        // 自動学習は黙って語を増やす。ここが効いていないと、
        // 使えば使うほどユーザー自身の登録語が枠外へ落ちていく。
        let mut entries: Vec<DictionaryEntry> = (0..DICTIONARY_MAX_TERMS + 20)
            .map(|i| auto(&format!("自動語{i:03}"), 9_000 + i as i64))
            .collect();
        // 一番古い手動語。日時では自動語すべてに負けている。
        entries.push(entry("手で登録した語", false, 1));
        let selection = select_dictionary_terms(&entries);
        assert!(
            selection.kept.contains(&(entries.len() - 1)),
            "手動登録語が自動追加語に押し出された: {selection:?}"
        );
    }

    #[test]
    fn a_pinned_auto_term_still_beats_an_unpinned_manual_one() {
        // ★はユーザーの明示の操作なので、出所より新しい意思表示。
        let mut entries: Vec<DictionaryEntry> = (0..DICTIONARY_MAX_TERMS)
            .map(|i| entry(&format!("手動語{i:03}"), false, 5_000 + i as i64))
            .collect();
        entries.push(DictionaryEntry {
            pinned: true,
            ..auto("★を付けた自動語", 1)
        });
        let selection = select_dictionary_terms(&entries);
        assert!(selection.kept.contains(&(entries.len() - 1)));
    }

    #[test]
    fn among_auto_terms_the_newest_wins() {
        // 手動が絡まないときの順位は従来どおり「新しいほど強い」。
        let mut entries: Vec<DictionaryEntry> = (0..DICTIONARY_MAX_TERMS + 10)
            .map(|i| auto(&format!("自動語{i:03}"), 1_000 + i as i64))
            .collect();
        entries.push(auto("今覚えた語", 9_999));
        let selection = select_dictionary_terms(&entries);
        assert!(selection.kept.contains(&(entries.len() - 1)));
        assert!(selection.dropped.contains(&0));
    }

    // --- 自動追加語のマージ ---

    #[test]
    fn merging_adds_new_terms_as_auto() {
        let mut entries = vec![DictionaryEntry::new("既存語")];
        let added = merge_auto_entries(
            &mut entries,
            &[("塩谷".to_string(), Some("しおや".to_string()))],
            1_700_000_000_000,
        );
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].written, "塩谷");
        assert_eq!(added[0].reading.as_deref(), Some("しおや"));
        assert_eq!(added[0].origin, DictionaryOrigin::Auto);
        assert_eq!(added[0].added_at_ms, 1_700_000_000_000);
        assert!(!added[0].pinned);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn merging_never_duplicates_an_existing_written_form() {
        // 同じ誤変換を何度直しても行が増えてはいけない。
        let mut entries = vec![DictionaryEntry::new("nox-voice")];
        let added = merge_auto_entries(
            &mut entries,
            &[
                ("nox-voice".to_string(), None),
                // 大文字小文字だけの違いも同じ語とみなす。
                ("NOX-Voice".to_string(), None),
                // 同じ呼び出しの中の重複も潰す。
                ("新語".to_string(), None),
                ("  新語  ".to_string(), None),
            ],
            1,
        );
        let written: Vec<&str> = added.iter().map(|e| e.written.as_str()).collect();
        assert_eq!(written, vec!["新語"]);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn merging_skips_blank_written_forms() {
        // normalize がどのみち捨てるが、「追加した」と報告してはいけない。
        let mut entries = Vec::new();
        let added = merge_auto_entries(&mut entries, &[("   ".to_string(), None)], 1);
        assert!(added.is_empty());
        assert!(entries.is_empty());
    }

    #[test]
    fn an_empty_reading_becomes_none() {
        let mut entries = Vec::new();
        merge_auto_entries(&mut entries, &[("語".to_string(), Some("  ".into()))], 1);
        assert_eq!(entries[0].reading, None);
    }

    #[test]
    fn auto_terms_are_capped_by_dropping_the_oldest() {
        let mut entries: Vec<DictionaryEntry> = (0..DICTIONARY_AUTO_MAX_ENTRIES)
            .map(|i| auto(&format!("自動語{i:03}"), 1_000 + i as i64))
            .collect();
        merge_auto_entries(&mut entries, &[("新しい自動語".to_string(), None)], 9_999);
        assert_eq!(entries.len(), DICTIONARY_AUTO_MAX_ENTRIES);
        // 一番古い語が消え、新しい語が入っている。
        assert!(!entries.iter().any(|e| e.written == "自動語000"));
        assert!(entries.iter().any(|e| e.written == "新しい自動語"));
    }

    #[test]
    fn the_cap_never_evicts_a_manual_or_pinned_term() {
        // 上限は「自動追加語が無限に伸びない」ためのもの。ユーザーの
        // 持ち物 (手動登録・★) を巻き添えにしたら本末転倒。
        let mut entries = vec![
            // 一番古い = 素直に数えれば真っ先に捨てられる位置。
            entry("一番古い手動語", false, 1),
            DictionaryEntry {
                pinned: true,
                ..auto("★を付けた自動語", 2)
            },
        ];
        entries.extend(
            (0..DICTIONARY_AUTO_MAX_ENTRIES).map(|i| auto(&format!("自動語{i:03}"), 1_000 + i as i64)),
        );
        merge_auto_entries(&mut entries, &[("さらに新しい語".to_string(), None)], 9_999);

        assert!(entries.iter().any(|e| e.written == "一番古い手動語"));
        assert!(entries.iter().any(|e| e.written == "★を付けた自動語"));
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.origin == DictionaryOrigin::Auto)
                .count(),
            DICTIONARY_AUTO_MAX_ENTRIES
        );
    }

    // --- Whisper プロンプト ---

    #[test]
    fn no_dictionary_and_no_context_means_no_prompt() {
        assert_eq!(build_whisper_prompt(&[], None), None);
        assert_eq!(build_whisper_prompt(&[], Some("   ")), None);
    }

    #[test]
    fn dictionary_only_prompt_lists_the_terms() {
        let entries = parse_entries(&lines(&["nox-voice", "Tauri"]));
        assert_eq!(
            build_whisper_prompt(&entries, None).as_deref(),
            Some("nox-voice、Tauri")
        );
    }

    #[test]
    fn context_only_prompt_is_collapsed() {
        let prompt = build_whisper_prompt(&[], Some("会議の\n\n議事録   です")).expect("ある");
        assert_eq!(prompt, "会議の 議事録 です");
    }

    #[test]
    fn the_dictionary_goes_last_so_truncation_eats_the_context() {
        // API は超過分を先頭から捨てる。辞書を末尾に置くのが仕様。
        let entries = parse_entries(&lines(&["nox-voice"]));
        let prompt = build_whisper_prompt(&entries, Some("画面のテキスト")).expect("ある");
        assert!(prompt.ends_with("nox-voice"), "辞書が末尾に無い: {prompt}");
        assert!(prompt.contains("画面のテキスト"));
    }

    #[test]
    fn the_pinned_term_sits_at_the_very_end() {
        // 文字数 → トークンの見積もりを外して API に切られたとき、
        // 最後まで残るのは末尾。一番落としたくない語をそこへ置く。
        let entries = vec![
            entry("普通の語", false, 100),
            entry("絶対に効かせたい語", true, 1),
        ];
        let prompt = build_whisper_prompt(&entries, None).expect("ある");
        assert_eq!(prompt, "普通の語、絶対に効かせたい語");
    }

    #[test]
    fn the_prompt_stays_within_the_budget() {
        let entries = parse_entries(&lines(&["用語"]));
        let long_context = "あ".repeat(1_000);
        let prompt = build_whisper_prompt(&entries, Some(&long_context)).expect("ある");
        assert!(
            prompt.chars().count() <= WHISPER_PROMPT_MAX_CHARS,
            "予算超過: {} 文字",
            prompt.chars().count()
        );
        // 溢れても辞書は残る。
        assert!(prompt.ends_with("用語"));
    }

    #[test]
    fn a_huge_dictionary_is_trimmed_at_term_boundaries() {
        // 語の途中で切れると別の語になってしまう。
        let terms: Vec<String> = (0..100).map(|i| format!("用語{i:03}")).collect();
        let entries = parse_entries(&terms);
        let prompt = build_whisper_prompt(&entries, None).expect("ある");
        assert!(prompt.chars().count() <= DICTIONARY_PROMPT_MAX_CHARS);
        for term in prompt.split('、') {
            assert!(
                terms.iter().any(|t| t == term),
                "語が途中で切れている: {term:?}"
            );
        }
    }

    #[test]
    fn a_single_oversized_term_yields_no_dictionary() {
        let long = "あ".repeat(DICTIONARY_PROMPT_MAX_CHARS + 10);
        let entries = parse_entries(&[long]);
        // 辞書が空でもコンテキストがあればプロンプトは作られる。
        let prompt = build_whisper_prompt(&entries, Some("画面テキスト")).expect("ある");
        assert_eq!(prompt, "画面テキスト");
        // 辞書もコンテキストも無ければ None。
        assert_eq!(build_whisper_prompt(&entries, None), None);
    }

    #[test]
    fn context_is_taken_from_the_end() {
        // キャレット付近 = 末尾の方が今の話題に近い。
        let context: String = (0..300).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let prompt = build_whisper_prompt(&[], Some(&context)).expect("ある");
        assert!(context.ends_with(&prompt), "末尾から取っていない");
    }

    #[test]
    fn a_full_dictionary_still_leaves_room_for_the_context() {
        // 辞書は独立した上限を持つ (旧実装は予算を全部食えたので、
        // 辞書が伸びた日に画面コンテキストが黙って消えていた)。
        let terms: Vec<String> = (0..100).map(|i| format!("語{i:03}")).collect();
        let entries = parse_entries(&terms);
        let prompt = build_whisper_prompt(&entries, Some("画面テキスト")).expect("ある");
        assert!(prompt.starts_with("画面テキスト"), "コンテキストが消えた: {prompt}");
        assert!(prompt.chars().count() <= WHISPER_PROMPT_MAX_CHARS);
        let dictionary = prompt.trim_start_matches("画面テキスト ");
        assert!(
            dictionary.chars().count() <= DICTIONARY_PROMPT_MAX_CHARS,
            "辞書が自分の枠を超えた: {} 文字",
            dictionary.chars().count()
        );
    }

    #[test]
    fn the_context_share_is_independent_of_the_dictionary_size() {
        // コンテキストの取り分は「190 − 辞書の実サイズ」。辞書が満杯でも
        // 49 文字は残る = 予算配分が壊れていないことの下限。
        let terms: Vec<String> = (0..100).map(|i| format!("語{i:03}")).collect();
        let entries = parse_entries(&terms);
        let context = "あ".repeat(500);
        let prompt = build_whisper_prompt(&entries, Some(&context)).expect("ある");
        let kept_context = prompt.chars().take_while(|c| *c == 'あ').count();
        assert!(
            kept_context >= WHISPER_PROMPT_MAX_CHARS - DICTIONARY_PROMPT_MAX_CHARS - 1,
            "コンテキストの取り分が足りない: {kept_context} 文字"
        );
    }
}
