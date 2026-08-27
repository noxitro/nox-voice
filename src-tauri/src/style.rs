//! アプリ別の整形スタイル。
//!
//! 同じ発話でも、Slack に書くのとメールに書くのでは望ましい文体が違う。
//! 挿入先のプロセス名 (必要ならウィンドウタイトル) でプロファイルを選び、
//! 整形プロンプトへスタイル指示として差し込む。
//!
//! ブラウザのように 1 つのプロセスで用途が変わるものは、タイトルの
//! 部分一致 (`title_contains`) で絞り込める。
//!
//! # 既定プロファイルの版管理 (2026-08-27)
//!
//! 既定プロファイルは設定ファイルへ**丸ごと保存される**。つまり
//! アプリを更新して [`default_profiles`] を増やしても、既存ユーザーの
//! config.json には永久に届かない。そこで
//!
//! - 既定の各項目に**安定 id** ([`StyleProfile::id`]) を振り、
//! - 設定側に「取り込み済みの版」と「ユーザーが消した既定 id」を持ち、
//! - 起動時に [`merge_default_profiles`] で差分だけを足す
//!
//! という形にした。**一番壊れやすい約束は「ユーザーが編集・削除した
//! ものを復活させない」こと**なので、そこを型で支える:
//!
//! | 状態 | 表現 |
//! |---|---|
//! | 既定のまま | `id` が非空 / `user_edited == false` |
//! | 既定を書き換えた | `id` が非空 / `user_edited == true` → **上書きしない** |
//! | 既定を消した | `id` が削除済み集合に入る → **足し直さない** |
//! | ユーザーが作った | `id` が空 → 既定側は一切触れない |
//!
//! `user_edited` を「今の既定と中身が違うか」で毎回計算しないのは、
//! **アプリ側が指示文を改訂したときに全件が「ユーザー編集」に見えてしまう**
//! から。編集は保存の瞬間にしか起きないので、そこで一度だけ印を付ける
//! ([`crate::config::Config::apply`])。

use serde::{Deserialize, Serialize};

/// 同梱既定プロファイルの版。**リリース後に中身を変えたら必ず上げる**。
///
/// 上げないと、その版を取り込み済みの設定へ改訂が届かない
/// (未編集の既定は id で照合して更新されるが、**新規追加はこの版が
/// 進まない限り検討されない**)。
///
/// 逆に、**まだ出していない版の中身を直すときは上げない**。版 1 は
/// この仕組みごと入った未リリースの版で、版 1 を持つ config.json は
/// まだ世の中に無いため、番号を進めても意味が無いどころか、
/// 「版 2 で何が変わったのか」の記録が空になる。
///
/// 0 は「この仕組みが無かった頃の設定ファイル」を意味する予約値
/// ([`merge_default_profiles`] が旧 7 件からの移行を行う)。
pub const STYLE_DEFAULTS_VERSION: u32 = 1;

/// 主要ブラウザのプロセス名の並び。
///
/// ブラウザ内で使うサービス (Gmail / Notion / ChatGPT …) を
/// 「ブラウザ 1 種 × サービス 1 個」で書くと組み合わせ爆発を起こす
/// (5 ブラウザ × 10 サービス = 50 件)。プロセス条件を `|` 区切りの
/// **いずれかに一致**にして、1 件で全ブラウザを覆う。
///
/// `chrome` は Chromium 系の派生 (chrome.exe / GoogleChrome.exe /
/// chromium.exe) にまとめて当たる。`msedge` は `chrome` を含まないので
/// 別に要る。
const BROWSERS: &str = "chrome|msedge|firefox|brave|vivaldi|opera";

/// ターミナルのプロセス名の並び。
///
/// 「素のターミナル」と「その中で動く CLI エージェント」の 2 つの既定が
/// 同じ並びを共有する。**片方だけに足すと、そのターミナルから起動した
/// エージェントだけ指示が真逆になる** (コマンド保護 ↔ AI プロンプト)。
const TERMINALS: &str =
    "windowsterminal.exe|powershell.exe|cmd.exe|alacritty.exe|wezterm-gui.exe";

/// 1 つのスタイルプロファイル。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleProfile {
    /// プロセス名の一致条件 (大文字小文字を無視した部分一致)。
    /// `|` で区切ると**いずれかに一致**。例: `slack.exe`、`chrome|msedge`
    pub process: String,
    /// ウィンドウタイトルの追加条件 (部分一致)。ブラウザの用途分けに使う。
    #[serde(default)]
    pub title_contains: Option<String>,
    /// 整形へ渡すスタイル指示。
    pub instruction: String,
    /// 同梱既定に由来する項目の安定 id。**ユーザーが作ったものは空**。
    ///
    /// 空にしておくことに意味がある: 既定の差分マージは id を持つ項目
    /// だけを見るので、ユーザー作成分に手を出す経路が存在しない。
    #[serde(default)]
    pub id: String,
    /// 既定由来だがユーザーが書き換えたか。真なら**アプリ更新で上書きしない**。
    #[serde(default)]
    pub user_edited: bool,
}

impl StyleProfile {
    /// ユーザーが作った項目か (既定由来でない)。
    pub fn is_user_made(&self) -> bool {
        self.id.trim().is_empty()
    }

    /// 一致条件と指示が同じか。`id` / `user_edited` は**見ない**
    /// (「中身を書き換えたか」の判定に使うため、印そのものを比べては意味が無い)。
    fn same_content(&self, other: &Self) -> bool {
        self.process == other.process
            && self.title_contains == other.title_contains
            && self.instruction == other.instruction
    }

    fn matches(&self, process: &str, title: &str) -> bool {
        if !process_matches(&self.process, process) {
            return false;
        }
        match self.title_contains.as_deref().map(str::trim) {
            Some(needle) if !needle.is_empty() => contains_ignore_case(title, needle),
            _ => true,
        }
    }

    /// 条件の細かさ。同点の場合により具体的な方を採る。
    ///
    /// `|` 区切りのときは**最短の候補**の長さを使う。並びのうち一番ゆるい
    /// ものがこの条件の当たり幅を決めるので、最長を採ると
    /// 「`chrome|msedge` は `opera` より具体的」という嘘の順位になる。
    fn specificity(&self) -> usize {
        let title_bonus = match self.title_contains.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => 1_000,
            _ => 0,
        };
        let shortest = process_alternatives(&self.process)
            .map(|alt| alt.chars().count())
            .min()
            .unwrap_or(0);
        title_bonus + shortest
    }
}

/// `|` 区切りのプロセス条件を、空でない候補の並びへ分解する。
fn process_alternatives(pattern: &str) -> impl Iterator<Item = &str> {
    pattern.split('|').map(str::trim).filter(|s| !s.is_empty())
}

/// プロセス条件が当たるか。候補が 1 つも無い (空欄・`|` だけ) なら偽。
///
/// 空欄を「全部に当てはまる」と解釈すると、設定の書きかけが
/// すべての発話に効いてしまう。
fn process_matches(pattern: &str, process: &str) -> bool {
    process_alternatives(pattern).any(|alt| contains_ignore_case(process, alt))
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// 挿入先に合うプロファイルを選ぶ。
///
/// 複数当てはまる場合は**より具体的な方**を採る
/// (タイトル条件つき > プロセス名が長い)。同点なら先に定義された方。
/// これにより、汎用の `chrome` プロファイルを置いたうえで
/// `chrome` + `Gmail` の特例を足す、という書き方ができる。
pub fn match_profile<'a>(
    profiles: &'a [StyleProfile],
    process: &str,
    title: &str,
) -> Option<&'a StyleProfile> {
    // `max_by_key` は同点で**最後**を返すので使わない。
    // 「同点なら先に定義された方」を守るため、真に上回ったときだけ差し替える。
    profiles
        .iter()
        .filter(|p| p.matches(process, title))
        .fold(None, |best: Option<&'a StyleProfile>, candidate| match best {
            Some(current) if current.specificity() >= candidate.specificity() => Some(current),
            _ => Some(candidate),
        })
}

/// 履歴に書き残す「当たったプロファイル」の表現。
///
/// - `Some(id)` … 既定由来 (安定 id)
/// - `Some(process)` … ユーザー作成 (id が無いので一致条件で代用)
/// - `Some("")` … **どれにも当たらなかった**
///
/// 空文字を「当たらなかった」に割り当てるのは、SQL の NULL を
/// 「まだ記録していない古い行」に取っておきたいから。`process` も `id` も
/// 空では保存できない ([`crate::config::Config::apply`] が落とす) ので、
/// 空文字がこの意味と衝突することはない。
pub fn history_label(profile: Option<&StyleProfile>) -> String {
    match profile {
        Some(p) if !p.id.trim().is_empty() => p.id.trim().to_string(),
        Some(p) => p.process.trim().to_string(),
        None => String::new(),
    }
}

/// 履歴から数えた「挿入先ごとの使用量」。提案の入力になる。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessUsage {
    /// 前景プロセス名 (履歴に残っていたもの)。
    pub process: String,
    /// その挿入先で確定した録音の件数。
    pub sessions: u64,
}

/// 「よく使っているのにプロファイルが無いアプリ」を絞り込む。
///
/// **判定には [`match_profile`] をそのまま使う。** ここで「プロセス名が
/// 一致するか」を独自に書くと、`|` 区切りやタイトル条件の扱いが本番と
/// ずれて「提案には出るのに実は当たっている」が起きる。
///
/// タイトルは履歴に無いので空文字で照会する。つまり**タイトル条件つきの
/// プロファイルだけでは「覆われている」と見なさない** — ブラウザは
/// タイトル無しの汎用プロファイルを既定に持たせてあるので、これで
/// 「Gmail は設定済みなのに chrome が提案される」にはならない。
pub fn suggest_uncovered(
    usage: &[ProcessUsage],
    profiles: &[StyleProfile],
    limit: usize,
) -> Vec<ProcessUsage> {
    usage
        .iter()
        .filter(|u| {
            let name = u.process.trim();
            // 前景が取れなかった録音 (`<unknown>`) を提案しても始まらない。
            !name.is_empty()
                && !name.starts_with('<')
                && match_profile(profiles, name, "").is_none()
        })
        .take(limit)
        .cloned()
        .collect()
}

/// 差分マージの結果 (ログと UI の説明用)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// 新しく足した既定の id。
    pub added: Vec<String>,
    /// 手つかずだったので中身を更新した既定の id。
    pub updated: Vec<String>,
    /// 旧形式から引き継いだ (id を与えた) 件数。
    pub adopted: usize,
}

impl MergeReport {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.adopted == 0
    }
}

/// 同梱の既定を、ユーザーの設定へ**差分だけ**取り込む。
///
/// `imported_version` は設定ファイルが持っていた版 (0 = この仕組みより前)。
/// `removed_ids` は「ユーザーが消した既定 id」で、必要なら書き足される。
///
/// # 旧形式 (版 0) の扱い
///
/// 旧い設定ファイルの既定 7 件には id が無い。プロセス条件で
/// [`legacy_v0_profiles`] と突き合わせて id を与え、指示文が違っていれば
/// `user_edited` を立てる。そして**突き合わなかった旧既定 id は
/// 「ユーザーが消した」側へ回す**。版 0 には削除の記録が無い以上、
/// 「消した」と「元から無かった」は原理的に区別できない。区別できない
/// ときに復活させる側へ倒すと、消したはずのプロファイルが更新のたびに
/// 生き返る — こちらの方が実害が大きいので、消えたまま側へ倒す。
pub fn merge_default_profiles(
    profiles: &mut Vec<StyleProfile>,
    removed_ids: &mut Vec<String>,
    imported_version: u32,
    catalog: &[StyleProfile],
) -> MergeReport {
    let mut report = MergeReport::default();

    if imported_version == 0 {
        report.adopted = adopt_legacy_ids(profiles);
        let present: Vec<String> = profiles
            .iter()
            .map(|p| p.id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        for legacy in legacy_v0_profiles() {
            if !present.contains(&legacy.id) && !removed_ids.contains(&legacy.id) {
                removed_ids.push(legacy.id);
            }
        }
    }

    for entry in catalog {
        let id = entry.id.trim();
        if id.is_empty() {
            debug_assert!(false, "既定プロファイルに id が無い: {}", entry.process);
            continue;
        }
        match profiles.iter_mut().find(|p| p.id.trim() == id) {
            Some(existing) => {
                // 手を入れられたものは触らない。これがこの機能の中心の約束。
                if !existing.user_edited && !existing.same_content(entry) {
                    existing.process = entry.process.clone();
                    existing.title_contains = entry.title_contains.clone();
                    existing.instruction = entry.instruction.clone();
                    report.updated.push(id.to_string());
                }
            }
            None => {
                if removed_ids.iter().any(|r| r == id) {
                    continue;
                }
                profiles.push(entry.clone());
                report.added.push(id.to_string());
            }
        }
    }
    report
}

/// 旧形式の既定 7 件へ id を与える。戻り値は引き継げた件数。
fn adopt_legacy_ids(profiles: &mut [StyleProfile]) -> usize {
    let legacy = legacy_v0_profiles();
    let mut adopted = 0;
    // 同じ id を 2 行に配らない。旧設定に `slack.exe` の行が 2 つあると
    // (旧 UI は重複を弾かなかった)、両方が `chat.slack` を名乗り、
    // マージは片方しか更新しない幽霊行を作る。
    let mut taken: Vec<&str> = Vec::new();
    for profile in profiles.iter_mut() {
        if !profile.id.trim().is_empty() {
            continue;
        }
        // 一致は**プロセス条件が完全に同じ**ときだけ。部分一致で拾うと、
        // ユーザーが自分で足した `code` 用の項目まで既定に化ける。
        let Some(hit) = legacy.iter().find(|l| {
            l.process.eq_ignore_ascii_case(profile.process.trim())
                && !taken.contains(&l.id.as_str())
        }) else {
            continue;
        };
        taken.push(hit.id.as_str());
        profile.id = hit.id.clone();
        // 指示文を変えていたなら、以後アプリ側の改訂で上書きしない。
        profile.user_edited = profile.instruction.trim() != hit.instruction.trim()
            || profile.title_contains.is_some();
        adopted += 1;
    }
    adopted
}

/// この仕組みが入る前 (版 0) に同梱していた既定 7 件。
///
/// **凍結する**。ここを現行カタログに合わせて書き換えると、旧設定の
/// 引き継ぎ判定が変わり、「ユーザーが編集したかどうか」を誤って判定する。
fn legacy_v0_profiles() -> Vec<StyleProfile> {
    let make = |id: &str, process: &str, instruction: &str| StyleProfile {
        process: process.to_string(),
        title_contains: None,
        instruction: instruction.to_string(),
        id: id.to_string(),
        user_edited: false,
    };
    vec![
        make(
            "chat.slack",
            "slack.exe",
            "チャットの発言。簡潔な口語で、丁寧すぎない自然な調子にする。挨拶や定型の前置きは付けない",
        ),
        make(
            "chat.discord",
            "discord.exe",
            "チャットの発言。簡潔な口語で、丁寧すぎない自然な調子にする",
        ),
        make(
            "chat.teams",
            "teams.exe",
            "社内チャットの発言。簡潔だが失礼にならない程度の丁寧さを保つ",
        ),
        make(
            "mail.outlook",
            "outlook.exe",
            "メール本文。ですます調の丁寧な文体にし、文の区切りで改行を入れる。宛名や署名は追加しない",
        ),
        make(
            "code.vscode",
            "code.exe",
            "技術的な文章。専門用語・製品名・コード片は原形のまま保ち、勝手に言い換えない",
        ),
        make(
            "code.visualstudio",
            "devenv.exe",
            "技術的な文章。専門用語・製品名・コード片は原形のまま保ち、勝手に言い換えない",
        ),
        make(
            "code.terminal",
            "windowsterminal.exe",
            "コマンドやコード片。日本語の句読点を足さず、入力された記号や英数字をそのまま保つ",
        ),
    ]
}

// --- 指示文の共通部品 --------------------------------------------------------
//
// 同じ文言を 26 か所へ手で書くと、直すときに必ず数か所が取り残される。
// 「カテゴリで同じもの」は定数にして、差分だけを各項目に書く。

/// AI への指示 (プロンプト) 用。**整えすぎないことが正解**。
///
/// チャットやメールと要求が逆になる。プロンプトは相手に読ませる文章では
/// なく**命令**なので、丁寧語へ寄せたり語を言い換えたりすると意図が壊れる。
/// 「これ」「さっきの」のような指示語は文脈を指しているので消してはならず、
/// 固有名詞・ファイルパス・コード片も原形で残す必要がある。
const AI_PROMPT: &str = "AI への指示文。整えすぎない。意図・指示語 (これ / さっきの / 上の)・固有名詞・ファイルパス・コード片・英数字は原形のまま保ち、言い換えや要約をしない。「えー」「あの」のような言いよどみと言い直しだけを取り除き、文の構造は話したままにする";

/// コードエディタ向け。コメントにも識別子にもなりうる。
const CODE_EDITOR: &str = "技術的な文章。専門用語・製品名・API 名・ファイルパス・コード片は原形のまま保ち、勝手に言い換えない。英単語をカタカナへ開かない";

/// 気軽なチャット。
const CHAT_CASUAL: &str =
    "チャットの発言。簡潔な口語で、丁寧すぎない自然な調子にする。挨拶や定型の前置きは付けない";

/// 私信のメッセンジャー (LINE / WhatsApp / Telegram)。
///
/// 相手は家族・友人なので、敬体へ寄せると別人が書いたようになる。
/// **長さの上限を指示しない** — 「短く」「1〜2 文で」と書くと、長い発話で
/// 整形モデルが内容を削る誘因になる。発話を失わないことが常に優先。
const CHAT_MESSENGER: &str = "私信の短いメッセージ。です・ます調に直さず、話した通りの砕けた口語のままにする。絵文字や顔文字は足さない。短い発話はそのまま、長い発話でも要約せず内容を削らない";

/// 長文を書く場所 (ドキュメント / ノート)。
const LONGFORM: &str = "文書の本文。ですます調で整え、話の切れ目で改行を入れる。見出しや箇条書きは、話した内容がそうなっているときだけ使う";

/// 同梱する既定プロファイル。
///
/// ユーザーが設定を書かなくても、よく使うアプリで文体が合うようにする。
/// 設定 UI から編集・削除でき、消したものは更新で戻らない
/// ([`merge_default_profiles`])。
///
/// # 並びの意味
///
/// 選択は [`match_profile`] の具体性順で決まるので、ここでの並びは
/// **同点のときの優先順**にしか効かない。読みやすさのためカテゴリ順に
/// 並べてある。ブラウザは「汎用 → サービス別」の順に置く。
///
/// # id の付け方
///
/// `カテゴリ.名前`。**一度出したら変えない** — 変えると、ユーザーが
/// 編集した項目が「知らない id」になり、更新で新しい既定が二重に生える。
pub fn default_profiles() -> Vec<StyleProfile> {
    let make = |id: &str, process: &str, instruction: &str| StyleProfile {
        process: process.to_string(),
        title_contains: None,
        instruction: instruction.to_string(),
        id: id.to_string(),
        user_edited: false,
    };
    let site = |id: &str, title: &str, instruction: &str| StyleProfile {
        process: BROWSERS.to_string(),
        title_contains: Some(title.to_string()),
        instruction: instruction.to_string(),
        id: id.to_string(),
        user_edited: false,
    };

    vec![
        // --- チャット ---------------------------------------------------
        // 短く、口語で。丁寧語へ寄せると「音声入力くさい」文になる。
        make("chat.slack", "slack.exe", CHAT_CASUAL),
        make("chat.discord", "discord.exe", CHAT_CASUAL),
        // 社内向けは Slack より一段固い。相手が上司でも読める幅にしておく。
        make(
            "chat.teams",
            "teams.exe|ms-teams.exe",
            "社内チャットの発言。簡潔だが失礼にならない程度の丁寧さを保つ。挨拶や定型の前置きは付けない",
        ),
        // LINE は家族・友人が相手のことが多い。敬体へ寄せない。
        // 絵文字を足さないと明記するのは、口語調にすると整形モデルが
        // 気を利かせて付けたがるため。
        //
        // **「1〜2 文に収める」とは書かない。** 長さの上限を指示すると、
        // 長い発話で整形モデルが内容を削る誘因になる。このアプリの最優先は
        // 「発話を黙って失わない」ことなので、短さは結果であって目標ではない。
        make("chat.line", "line.exe", CHAT_MESSENGER),
        make("chat.whatsapp", "whatsapp.exe", CHAT_MESSENGER),
        make("chat.telegram", "telegram.exe", CHAT_MESSENGER),
        // --- メール -----------------------------------------------------
        // 宛名・署名を「足さない」と書くのが要。書かないと整形モデルが
        // 「お世話になっております」から始まる定型を勝手に生やす。
        make(
            "mail.outlook",
            "outlook.exe",
            "メール本文。ですます調の丁寧な文体にし、文の区切りで改行を入れる。宛名・挨拶・署名は追加しない",
        ),
        make(
            "mail.thunderbird",
            "thunderbird.exe",
            "メール本文。ですます調の丁寧な文体にし、文の区切りで改行を入れる。宛名・挨拶・署名は追加しない",
        ),
        // --- コード・ターミナル -----------------------------------------
        make("code.vscode", "code.exe", CODE_EDITOR),
        make("code.cursor", "cursor.exe", CODE_EDITOR),
        make("code.visualstudio", "devenv.exe", CODE_EDITOR),
        make("code.jetbrains", "idea64.exe|pycharm64.exe|rustrover64.exe", CODE_EDITOR),
        // ターミナルは「文」ではない。句読点を足されるとコマンドが壊れる。
        make(
            "code.terminal",
            TERMINALS,
            "コマンドやコード片。日本語の句読点を足さず、入力された記号や英数字をそのまま保つ",
        ),
        // ターミナルの中で動く CLI エージェント (Claude Code / Codex CLI)。
        // これは「コマンド」ではなく「AI への指示」なので要求が真逆になる。
        // **タイトルに頼るのが弱点**: Windows Terminal のタブ名は既定で
        // 実行中プロセス名や作業ディレクトリになるため、当たらない環境が
        // ある。当たらなければ 1 つ上のターミナル既定へ落ちるだけで壊れない。
        // プロセスの並びは 1 つ上の `code.terminal` と**そろえる**。
        // 片方に cmd.exe が無いと、cmd から起動した CLI エージェントだけが
        // 「コマンド」扱いになり、指示が真逆になる。
        StyleProfile {
            process: TERMINALS.to_string(),
            title_contains: Some("claude".to_string()),
            instruction: AI_PROMPT.to_string(),
            id: "ai.claude-code".to_string(),
            user_edited: false,
        },
        StyleProfile {
            process: TERMINALS.to_string(),
            title_contains: Some("codex".to_string()),
            instruction: AI_PROMPT.to_string(),
            id: "ai.codex-cli".to_string(),
            user_edited: false,
        },
        // --- AI デスクトップアプリ --------------------------------------
        make("ai.claude-app", "claude.exe", AI_PROMPT),
        make("ai.chatgpt-app", "chatgpt.exe", AI_PROMPT),
        // --- ドキュメント・ノート ---------------------------------------
        make("doc.word", "winword.exe", LONGFORM),
        // スライドは本文と違い、1 枚に載る短い言葉になる。
        make(
            "doc.powerpoint",
            "powerpnt.exe",
            "スライドに載せる文。1 行を短くし、体言止めを許す。冗長な接続詞と言いよどみを落とす",
        ),
        // メモ帳に喋るときは、たいてい下書きか控え。勝手に敬体へ直さない。
        make(
            "doc.notepad",
            "notepad.exe",
            "素のメモ。文体を変えず、話した言葉のまま書き起こす。言いよどみと言い直しだけを取り除く",
        ),
        // Obsidian / Notion は Markdown が通る。記法を壊さないことが要点。
        make(
            "doc.obsidian",
            "obsidian.exe",
            "Markdown のノート。話した内容をそのまま書き留める調子にし、`#` や `-` などの記法・リンク・コード片は原形のまま保つ",
        ),
        // OneNote は Word と違い、走り書きと清書が混ざる。長文寄りの
        // 指示にしつつ、記法 (箇条書き・タグ) を壊さないことを足す。
        make(
            "doc.onenote",
            "onenote",
            "ノートの本文。ですます調で整え、話の切れ目で改行を入れる。箇条書きや固有名詞は原形のまま保つ",
        ),
        make(
            "doc.notion-app",
            "notion.exe",
            "ノートの本文。ですます調で整え、話の切れ目で改行を入れる。記法や固有名詞は原形のまま保つ",
        ),
        // --- ブラウザ (汎用) --------------------------------------------
        // **既定でブラウザ用が 1 件も無いのが元の穴だった。** サービス別
        // だけを足すと、それ以外のページでは無指定に戻ってしまう。加えて
        // この汎用項目が無いと、履歴からの提案が毎回 chrome.exe を
        // 「未設定」として挙げ続ける (提案は空タイトルで照会するため)。
        StyleProfile {
            process: BROWSERS.to_string(),
            title_contains: None,
            instruction: "ブラウザの入力欄。用途が幅広いので文体を大きく変えず、言いよどみと言い直しを取り除いて読みやすくする程度に留める".to_string(),
            id: "web.generic".to_string(),
            user_edited: false,
        },
        // --- ブラウザ (サービス別) --------------------------------------
        // タイトルの部分一致。ページタイトルは「件名 - Gmail」のように
        // サービス名で終わることが多いので、そこを拾う。
        site(
            "web.gmail",
            "Gmail",
            "メール本文。ですます調の丁寧な文体にし、文の区切りで改行を入れる。宛名・挨拶・署名は追加しない",
        ),
        // Google ドキュメントは UI 言語でタイトルが変わる。日本語環境を
        // 主にしつつ、英語 UI 用も別 id で持つ (1 件では両方を覆えない)。
        site("web.gdocs-ja", "Google ドキュメント", LONGFORM),
        site("web.gdocs-en", "Google Docs", LONGFORM),
        site(
            "web.notion",
            "Notion",
            "ノートの本文。ですます調で整え、話の切れ目で改行を入れる。記法や固有名詞は原形のまま保つ",
        ),
        // AI サービスはすべて AI_PROMPT。**チャットやメールと逆に、
        // 整えないことが正しい**という点でひとまとまりになる。
        site("web.claude", "Claude", AI_PROMPT),
        site("web.chatgpt", "ChatGPT", AI_PROMPT),
        site("web.gemini", "Gemini", AI_PROMPT),
        site("web.perplexity", "Perplexity", AI_PROMPT),
        site("web.grok", "Grok", AI_PROMPT),
        // Slack / Discord をブラウザで使う人向け。デスクトップ版と
        // 同じ指示にしておかないと、同じ相手に別の文体で書くことになる。
        site("web.slack", "Slack", CHAT_CASUAL),
        site("web.discord", "Discord", CHAT_CASUAL),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(process: &str, title: Option<&str>, instruction: &str) -> StyleProfile {
        StyleProfile {
            process: process.to_string(),
            title_contains: title.map(str::to_string),
            instruction: instruction.to_string(),
            id: String::new(),
            user_edited: false,
        }
    }

    fn with_id(id: &str, process: &str, instruction: &str) -> StyleProfile {
        StyleProfile {
            id: id.to_string(),
            ..profile(process, None, instruction)
        }
    }

    #[test]
    fn matches_the_process_name_ignoring_case() {
        let profiles = vec![profile("slack.exe", None, "カジュアル")];
        let hit = match_profile(&profiles, "Slack.EXE", "").expect("一致する");
        assert_eq!(hit.instruction, "カジュアル");
    }

    #[test]
    fn matches_on_a_partial_process_name() {
        // 実行ファイル名が環境で変わるアプリのために部分一致にしてある。
        let profiles = vec![profile("chrome", None, "ブラウザ")];
        assert!(match_profile(&profiles, "chrome.exe", "").is_some());
        assert!(match_profile(&profiles, "GoogleChrome.exe", "").is_some());
    }

    #[test]
    fn a_non_matching_process_selects_nothing() {
        let profiles = vec![profile("slack.exe", None, "カジュアル")];
        assert!(match_profile(&profiles, "notepad.exe", "").is_none());
    }

    #[test]
    fn a_title_condition_narrows_the_match() {
        let profiles = vec![profile("chrome", Some("Gmail"), "メール")];
        assert!(match_profile(&profiles, "chrome.exe", "受信トレイ - Gmail").is_some());
        assert!(
            match_profile(&profiles, "chrome.exe", "ニュースサイト").is_none(),
            "タイトル条件が効いていない"
        );
    }

    #[test]
    fn the_more_specific_profile_wins() {
        // 汎用のブラウザ設定に、Gmail だけの特例を足せること。
        let profiles = vec![
            profile("chrome", None, "汎用ブラウザ"),
            profile("chrome", Some("Gmail"), "メール"),
        ];
        let hit = match_profile(&profiles, "chrome.exe", "受信トレイ - Gmail").expect("一致");
        assert_eq!(hit.instruction, "メール", "特例より汎用が勝ってしまった");

        let hit = match_profile(&profiles, "chrome.exe", "ニュース").expect("一致");
        assert_eq!(hit.instruction, "汎用ブラウザ");
    }

    #[test]
    fn a_longer_process_pattern_beats_a_shorter_one() {
        let profiles = vec![
            profile("code", None, "汎用エディタ"),
            profile("code.exe", None, "VS Code"),
        ];
        let hit = match_profile(&profiles, "code.exe", "").expect("一致");
        assert_eq!(hit.instruction, "VS Code");
    }

    #[test]
    fn ties_go_to_the_first_definition() {
        // doc の約束どおり、同じ具体性なら先に書いた方が勝つ。
        let profiles = vec![
            profile("slack.exe", None, "先に定義"),
            profile("slack.exe", None, "後に定義"),
        ];
        let hit = match_profile(&profiles, "slack.exe", "").expect("一致");
        assert_eq!(hit.instruction, "先に定義");
    }

    #[test]
    fn ties_with_title_conditions_also_go_to_the_first() {
        let profiles = vec![
            profile("chrome", Some("Gmail"), "先に定義"),
            profile("chrome", Some("Gmail"), "後に定義"),
        ];
        let hit = match_profile(&profiles, "chrome.exe", "受信トレイ - Gmail").expect("一致");
        assert_eq!(hit.instruction, "先に定義");
    }

    #[test]
    fn an_empty_process_pattern_never_matches() {
        // 空欄を「全部に当てはまる」と解釈すると、設定の書きかけが
        // すべての発話に効いてしまう。
        let profiles = vec![profile("   ", None, "壊れた設定")];
        assert!(match_profile(&profiles, "notepad.exe", "何か").is_none());
    }

    #[test]
    fn a_pattern_of_only_separators_never_matches() {
        let profiles = vec![profile(" | | ", None, "壊れた設定")];
        assert!(match_profile(&profiles, "notepad.exe", "何か").is_none());
    }

    #[test]
    fn an_empty_title_condition_is_ignored() {
        let profiles = vec![profile("slack.exe", Some("  "), "カジュアル")];
        assert!(match_profile(&profiles, "slack.exe", "").is_some());
    }

    #[test]
    fn alternatives_match_any_of_the_listed_processes() {
        // 「ブラウザ 1 種 × サービス 1 個」の組み合わせ爆発を避ける仕掛け。
        let profiles = vec![profile("chrome|msedge|firefox", None, "ブラウザ")];
        for p in ["chrome.exe", "msedge.exe", "firefox.exe"] {
            assert!(match_profile(&profiles, p, "").is_some(), "{p} が外れた");
        }
        assert!(match_profile(&profiles, "notepad.exe", "").is_none());
    }

    #[test]
    fn specificity_of_an_alternation_uses_its_weakest_member() {
        // `chrome|opera` は `opera` (5 文字) と同じ幅で当たる。最長を
        // 採ると「並べるほど具体的」という嘘の順位になる。
        let profiles = vec![
            profile("chrome|opera", None, "並び"),
            profile("chrome", None, "単体"),
        ];
        let hit = match_profile(&profiles, "chrome.exe", "").expect("一致");
        assert_eq!(hit.instruction, "単体");
    }

    #[test]
    fn unknown_target_falls_through_to_no_style() {
        // 既定プロファイルに無いアプリでは、スタイル指示なしで整形する。
        assert!(match_profile(&default_profiles(), "explorer.exe", "無題").is_none());
    }

    #[test]
    fn bundled_profiles_cover_the_common_cases() {
        let profiles = default_profiles();
        let slack = match_profile(&profiles, "slack.exe", "").expect("Slack");
        assert!(slack.instruction.contains("口語"));
        let outlook = match_profile(&profiles, "OUTLOOK.EXE", "").expect("Outlook");
        assert!(outlook.instruction.contains("ですます"));
        let editor = match_profile(&profiles, "Code.exe", "").expect("エディタ");
        assert!(editor.instruction.contains("専門用語"));
    }

    #[test]
    fn bundled_profiles_cover_browsers_and_their_services() {
        let profiles = default_profiles();
        // ブラウザ既定が 1 件も無かったのが元の穴。まず汎用が当たること。
        for browser in ["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe"] {
            let hit = match_profile(&profiles, browser, "何かのページ")
                .unwrap_or_else(|| panic!("{browser} に汎用が無い"));
            assert_eq!(hit.id, "web.generic", "{browser}");
        }
        // そのうえでサービス別が勝つこと。
        let gmail = match_profile(&profiles, "msedge.exe", "受信トレイ - Gmail").expect("Gmail");
        assert_eq!(gmail.id, "web.gmail");
        let claude = match_profile(&profiles, "firefox.exe", "設計の相談 - Claude").expect("Claude");
        assert_eq!(claude.id, "web.claude");
    }

    #[test]
    fn ai_targets_are_told_not_to_over_polish() {
        // ここが逆になっていると機能の意味が無い: チャット/メールは
        // 「整える」、AI へのプロンプトは「整えすぎない」。
        let profiles = default_profiles();
        let prompt = match_profile(&profiles, "chrome.exe", "ChatGPT").expect("ChatGPT");
        assert!(prompt.instruction.contains("整えすぎない"), "{prompt:?}");
        assert!(prompt.instruction.contains("指示語"), "{prompt:?}");
        let mail = match_profile(&profiles, "chrome.exe", "Gmail").expect("Gmail");
        assert!(mail.instruction.contains("ですます調"), "{mail:?}");
    }

    #[test]
    fn cli_agents_share_the_terminal_process_list() {
        // 片方に cmd.exe が無いと、cmd から起動した Claude Code だけが
        // 「コマンドなので句読点を足すな」側になり、指示が真逆になる。
        let profiles = default_profiles();
        let terminal = profiles
            .iter()
            .find(|p| p.id == "code.terminal")
            .expect("ターミナル既定");
        for id in ["ai.claude-code", "ai.codex-cli"] {
            let agent = profiles.iter().find(|p| p.id == id).expect(id);
            assert_eq!(agent.process, terminal.process, "{id} の並びがずれている");
        }
        // 実際に cmd.exe + claude で AI 側が勝つこと。
        let hit = match_profile(&profiles, "cmd.exe", "claude — myproject").expect("一致");
        assert_eq!(hit.id, "ai.claude-code");
    }

    #[test]
    fn messenger_defaults_never_cap_the_length() {
        // 長さの上限を指示すると、長い発話で整形モデルが内容を削る。
        // このアプリの最優先は「発話を黙って失わない」こと。
        for id in ["chat.line", "chat.whatsapp", "chat.telegram"] {
            let p = default_profiles()
                .into_iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("{id} が無い"));
            for banned in ["1〜2 文", "文に収め", "短くまとめ", "要約"] {
                assert!(
                    !p.instruction.contains(banned)
                        || p.instruction.contains(&format!("{banned}せず")),
                    "{id} に長さの上限が書かれている: {}",
                    p.instruction
                );
            }
            assert!(p.instruction.contains("削らない"), "{id}");
        }
    }

    #[test]
    fn bundled_profile_ids_are_unique_and_present() {
        let profiles = default_profiles();
        let mut ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.iter().all(|id| !id.is_empty()), "id の無い既定がある");
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "id が重複している: {ids:?}");
    }

    #[test]
    fn legacy_ids_all_exist_in_the_current_catalog() {
        // 旧既定の id を現行カタログから消すと、旧ユーザーの項目が
        // 「知らない id」になり、同じアプリの既定が二重に生える。
        let current = default_profiles();
        for legacy in legacy_v0_profiles() {
            assert!(
                current.iter().any(|p| p.id == legacy.id),
                "旧既定 {} が現行カタログから消えている",
                legacy.id
            );
        }
    }

    #[test]
    fn bundled_profiles_are_serializable() {
        // 設定ファイルへ往復できること (UI から編集するため)。
        let json = serde_json::to_string(&default_profiles()).expect("直列化");
        let back: Vec<StyleProfile> = serde_json::from_str(&json).expect("復元");
        assert_eq!(back, default_profiles());
    }

    #[test]
    fn a_profile_without_title_field_deserializes() {
        // 手書きの設定で title_contains を省略できること。
        let json = r#"[{"process":"slack.exe","instruction":"カジュアル"}]"#;
        let profiles: Vec<StyleProfile> = serde_json::from_str(json).expect("復元");
        assert_eq!(profiles[0].title_contains, None);
        assert_eq!(profiles[0].id, "", "旧形式は id なし = ユーザー作成扱い");
        assert!(!profiles[0].user_edited);
    }

    // --- 差分マージ ---------------------------------------------------------

    #[test]
    fn a_fresh_config_gets_the_whole_catalog() {
        // シナリオ 1: 新規ユーザー (設定ファイルが無い → 版は現行)。
        let mut profiles = Vec::new();
        let mut removed = Vec::new();
        let catalog = default_profiles();
        let report =
            merge_default_profiles(&mut profiles, &mut removed, STYLE_DEFAULTS_VERSION, &catalog);
        assert_eq!(profiles.len(), catalog.len());
        assert_eq!(report.added.len(), catalog.len());
        assert!(removed.is_empty(), "新規なのに削除済みが記録された");
    }

    #[test]
    fn a_legacy_config_with_no_profiles_stays_empty_of_old_defaults() {
        // 旧 UI で全部消した人。版 0 に削除の記録は無いが、「1 件も無い」
        // という事実そのものが記録になる。旧既定は足し直さない。
        let mut profiles = Vec::new();
        let mut removed = Vec::new();
        let catalog = default_profiles();
        merge_default_profiles(&mut profiles, &mut removed, 0, &catalog);
        for legacy in legacy_v0_profiles() {
            assert!(
                !profiles.iter().any(|p| p.id == legacy.id),
                "消したはずの旧既定 {} が戻った",
                legacy.id
            );
        }
        // 旧既定に無かった新カタログ分は届く (それがこの仕組みの目的)。
        assert!(profiles.iter().any(|p| p.id == "web.generic"));
    }

    #[test]
    fn an_edited_default_is_never_overwritten() {
        // シナリオ 2: 既定を編集したユーザー。
        let mut profiles = vec![StyleProfile {
            user_edited: true,
            ..with_id("chat.slack", "slack.exe", "私が書いた指示")
        }];
        let mut removed = Vec::new();
        let catalog = vec![with_id("chat.slack", "slack.exe", "アプリの新しい指示")];
        let report = merge_default_profiles(&mut profiles, &mut removed, 1, &catalog);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].instruction, "私が書いた指示");
        assert!(report.updated.is_empty());
    }

    #[test]
    fn an_untouched_default_is_refreshed() {
        let mut profiles = vec![with_id("chat.slack", "slack.exe", "古い指示")];
        let mut removed = Vec::new();
        let catalog = vec![with_id("chat.slack", "slack.exe", "新しい指示")];
        let report = merge_default_profiles(&mut profiles, &mut removed, 1, &catalog);
        assert_eq!(profiles[0].instruction, "新しい指示");
        assert_eq!(report.updated, vec!["chat.slack".to_string()]);
    }

    #[test]
    fn a_deleted_default_never_comes_back() {
        // シナリオ 3: 既定を削除したユーザー。**この機能で一番壊れやすい約束**。
        let mut profiles = vec![with_id("chat.discord", "discord.exe", "そのまま")];
        let mut removed = vec!["chat.slack".to_string()];
        let catalog = vec![
            with_id("chat.slack", "slack.exe", "既定"),
            with_id("chat.discord", "discord.exe", "そのまま"),
        ];
        let report = merge_default_profiles(&mut profiles, &mut removed, 1, &catalog);
        assert!(report.added.is_empty(), "消した既定が生き返った: {report:?}");
        assert_eq!(profiles.len(), 1);
        // 何度起動しても同じ (冪等)。
        let report = merge_default_profiles(&mut profiles, &mut removed, 1, &catalog);
        assert!(report.is_empty());
        assert_eq!(profiles.len(), 1);
    }

    #[test]
    fn a_legacy_config_is_adopted_without_duplicates() {
        // シナリオ 4: 旧形式 (版 0 / id 無しの 7 件) からの移行。
        let mut profiles: Vec<StyleProfile> = legacy_v0_profiles()
            .into_iter()
            .map(|p| StyleProfile {
                id: String::new(),
                ..p
            })
            .collect();
        // うち 1 件はユーザーが指示を書き換えている。
        profiles[0].instruction = "自分で書いた".to_string();
        let mut removed = Vec::new();
        let catalog = default_profiles();
        let report = merge_default_profiles(&mut profiles, &mut removed, 0, &catalog);

        assert_eq!(report.adopted, 7, "旧既定を引き継げていない");
        assert!(removed.is_empty(), "全件そろっているのに削除扱いが出た");
        // slack.exe が 2 件に増えていないこと。
        let slack: Vec<_> = profiles.iter().filter(|p| p.id == "chat.slack").collect();
        assert_eq!(slack.len(), 1, "旧既定と新既定が二重に生えた");
        assert_eq!(slack[0].instruction, "自分で書いた");
        assert!(slack[0].user_edited, "編集済みの印が付いていない");
        // 新規カタログ分は足されている。
        assert!(profiles.iter().any(|p| p.id == "web.generic"));
    }

    #[test]
    fn a_legacy_config_missing_a_default_keeps_it_missing() {
        // 版 0 には削除の記録が無い。「消した」と「元から無い」を区別
        // できないので、復活させない側へ倒す (doc 参照)。
        let mut profiles: Vec<StyleProfile> = legacy_v0_profiles()
            .into_iter()
            .filter(|p| p.id != "chat.slack")
            .map(|p| StyleProfile {
                id: String::new(),
                ..p
            })
            .collect();
        let mut removed = Vec::new();
        let catalog = default_profiles();
        merge_default_profiles(&mut profiles, &mut removed, 0, &catalog);
        assert!(removed.contains(&"chat.slack".to_string()));
        assert!(
            !profiles.iter().any(|p| p.id == "chat.slack"),
            "消したはずの Slack 既定が戻った"
        );
    }

    #[test]
    fn a_duplicated_legacy_row_only_adopts_the_id_once() {
        // 旧 UI は重複行を弾かなかった。両方に同じ id を配ると、
        // マージは片方しか更新せず、更新されない幽霊行が残り続ける。
        let mut profiles = vec![
            profile("slack.exe", None, "1 つ目"),
            profile("slack.exe", None, "2 つ目"),
        ];
        let adopted = adopt_legacy_ids(&mut profiles);
        assert_eq!(adopted, 1);
        assert_eq!(profiles[0].id, "chat.slack");
        assert!(profiles[1].is_user_made(), "2 行目まで既定を名乗った");
    }

    #[test]
    fn a_legacy_user_profile_is_left_alone() {
        // 旧形式にユーザーが自分で足した項目。id を与えてはいけない。
        let mut profiles = vec![profile("myapp.exe", None, "自作")];
        let mut removed = Vec::new();
        let catalog = default_profiles();
        merge_default_profiles(&mut profiles, &mut removed, 0, &catalog);
        let mine = profiles.iter().find(|p| p.process == "myapp.exe").expect("残る");
        assert!(mine.is_user_made(), "ユーザー作成が既定に化けた");
        assert!(!mine.user_edited);
    }

    #[test]
    fn a_version_bump_adds_only_the_new_entries() {
        // シナリオ 5: 版が進んだとき。既存はそのまま、新規だけ足す。
        let v1 = vec![with_id("chat.slack", "slack.exe", "既定")];
        let mut profiles = v1.clone();
        let mut removed = Vec::new();
        let v2 = vec![
            with_id("chat.slack", "slack.exe", "既定"),
            with_id("chat.line", "line.exe", "新しい既定"),
        ];
        let report = merge_default_profiles(&mut profiles, &mut removed, 1, &v2);
        assert_eq!(report.added, vec!["chat.line".to_string()]);
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0], v1[0], "既存が書き換わった");
    }

    // --- 履歴との連携 -------------------------------------------------------

    #[test]
    fn the_history_label_distinguishes_none_from_a_hit() {
        let default = with_id("chat.slack", "slack.exe", "既定");
        let mine = profile("myapp.exe", None, "自作");
        assert_eq!(history_label(Some(&default)), "chat.slack");
        assert_eq!(history_label(Some(&mine)), "myapp.exe");
        assert_eq!(history_label(None), "", "未一致は空文字 (NULL とは別物)");
    }

    #[test]
    fn suggestions_skip_apps_that_are_already_covered() {
        let profiles = default_profiles();
        let usage = vec![
            ProcessUsage {
                process: "slack.exe".to_string(),
                sessions: 90,
            },
            ProcessUsage {
                process: "chrome.exe".to_string(),
                sessions: 80,
            },
            ProcessUsage {
                process: "figma.exe".to_string(),
                sessions: 40,
            },
        ];
        let out = suggest_uncovered(&usage, &profiles, 10);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].process, "figma.exe");
    }

    #[test]
    fn suggestions_drop_unknown_targets_and_respect_the_limit() {
        let usage = vec![
            ProcessUsage {
                process: "<unknown>".to_string(),
                sessions: 100,
            },
            ProcessUsage {
                process: "  ".to_string(),
                sessions: 50,
            },
            ProcessUsage {
                process: "a.exe".to_string(),
                sessions: 9,
            },
            ProcessUsage {
                process: "b.exe".to_string(),
                sessions: 8,
            },
        ];
        let out = suggest_uncovered(&usage, &[], 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].process, "a.exe");
    }

    #[test]
    fn a_suggestion_is_hidden_once_the_user_adds_a_profile() {
        // 提案から作った項目が、次の集計で提案側から消えること
        // (提案と適用が同じ判定を使っている、という担保)。
        let usage = vec![ProcessUsage {
            process: "figma.exe".to_string(),
            sessions: 40,
        }];
        let added = vec![profile("figma.exe", None, "デザインツールのコメント")];
        assert!(suggest_uncovered(&usage, &added, 10).is_empty());
    }
}
