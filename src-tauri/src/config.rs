//! 設定の永続化と API キーの解決。
//!
//! # API キーの扱い (厳守)
//!
//! - **ログ・エラーメッセージ・フロントへの応答にキーを出さない。**
//!   [`Secret`] は `Debug` / `Display` の両方で伏字になるので、うっかり
//!   `{:?}` で構造体ごと出しても漏れない。
//! - Gemini は URL クエリ (`?key=...`) ではなく `x-goog-api-key` ヘッダで渡す。
//!   クエリに載せるとプロキシログ・クラッシュレポート・履歴に残りうるため。
//! - フロントへ返すのは [`ConfigView`] (キーは「設定済みか」と取得元だけ)。
//!
//! 設定ファイルは Tauri の app_config_dir 配下に JSON で置く。
//! エンドポイントとモデル名も設定可能にしてある (design.md R1: 無料枠の
//! データ利用規約を避けて有料ティアやプロキシへ差し替えられるように)。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::dictionary::{DictionaryEntry, DictionaryStatus};
use crate::sound::{SoundChoice, SoundPreset};
use crate::style::StyleProfile;

/// Groq の既定エンドポイント (OpenAI 互換の transcriptions)。
pub const DEFAULT_GROQ_ENDPOINT: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
/// Gemini の既定エンドポイント。末尾に `/{model}:generateContent` が付く。
pub const DEFAULT_GEMINI_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";
pub const DEFAULT_STT_MODEL: &str = "whisper-large-v3";

/// 整形の控え (Groq) の既定エンドポイント。**完全な URL**。
///
/// [`DEFAULT_GROQ_ENDPOINT`] から導出しないこと — あちらは
/// transcriptions の完全な URL であってベース URL ではない。
pub const DEFAULT_GROQ_CHAT_ENDPOINT: &str = "https://api.groq.com/openai/v1/chat/completions";

/// 整形の控えの既定モデル。
///
/// # 実測 (2026-09-06、同一文の整形 / 数回ずつ)
///
/// | モデル | フィラー除去+言い直し解決 | 句読点なし119字 | 所要 |
/// |---|---|---|---|
/// | `openai/gpt-oss-120b` | 成功 | 句読点7・改行あり | 0.6〜1.3s |
/// | **`openai/gpt-oss-20b`** | **成功 (最も原文に忠実)** | 句読点6 | 0.4〜0.6s |
/// | `qwen/qwen3.8-27b` | 成功 | 句読点7 | 0.2〜0.3s |
///
/// 控えに求めるのは「主と同じ文体で、待たせずに返る」ことなので、
/// 最速の qwen ではなく**最も原文に忠実**だった 20b を既定にする。
/// いずれも推論モデルで、`max_tokens` に思考分を見込む必要がある
/// ([`crate::format_groq`] のモジュール doc)。設定で差し替え可能。
pub const DEFAULT_GROQ_FORMAT_MODEL: &str = "openai/gpt-oss-20b";

/// 整形モデルの既定。
///
/// # なぜ design.md の `gemini-2.5-flash` ではないか (実測 2026-08-17)
///
/// `gemini-2.5-flash` は新規のキーでは **404** になる
/// ("no longer available to new users")。ListModels には出てくるが呼べない。
/// 実測した候補の比較 (同一文の整形 / 1 回ずつ):
///
/// | モデル | 所要 | 結果 |
/// |---|---|---|
/// | `gemini-2.5-flash` | — | 404 (利用不可) |
/// | `gemini-flash-lite-latest` | 772 ms | 良好 |
/// | `gemini-3.5-flash-lite` | 872 ms | 良好 |
/// | `gemini-flash-latest` | 2.9 s | 良好だが遅い |
/// | `gemini-3.1-flash-lite` | 15.6 s | 語尾を書き換えた |
/// | `gemini-3.5-flash` | 27.2 s | 音声入力には論外 |
///
/// `-latest` エイリアスを選ぶのは、**固定版はいつか退役して 404 になる**
/// (まさに 2.5-flash で起きたこと) から。エイリアスの弱点は出力の癖が
/// 予告なく変わることだが、退役時に無言で壊れるより、R2 の劣化モードで
/// 拾える形にしておく方が本アプリには合う。設定で差し替え可能。
pub const DEFAULT_FORMAT_MODEL: &str = "gemini-flash-lite-latest";
pub const DEFAULT_LANGUAGE: &str = "ja";

/// 設定画面の候補リストに出す整形モデル 1 件。
///
/// 候補は**実測で良好だったものだけ**を載せる (上の 2 つの表)。遅い・
/// 文を書き換えた・呼べなかったモデルを並べると、選べること自体が罠になる。
/// 候補に無い名前も入力欄へ直接打てば使える (候補は入力の補助にすぎない)。
///
/// **候補を足すときはここに 1 行足す。** 画面は [`ConfigView`] 経由で
/// これを受け取るだけで、フロントに写しは無い (写しを置くと片方だけ
/// 直されて、画面の候補と実測の表がずれる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelCandidate {
    pub name: &'static str,
    /// 候補リストに添える短い説明 (実測の要約)。
    pub note: &'static str,
}

/// 主 (Gemini) の整形モデルの候補。先頭が既定 ([`DEFAULT_FORMAT_MODEL`])。
pub const GEMINI_FORMAT_MODEL_CANDIDATES: &[ModelCandidate] = &[
    ModelCandidate {
        name: DEFAULT_FORMAT_MODEL,
        note: "既定・最速 (実測 0.8 秒)",
    },
    ModelCandidate {
        name: "gemini-3.5-flash-lite",
        note: "良好 (実測 0.9 秒)",
    },
    ModelCandidate {
        name: "gemini-flash-latest",
        note: "良好だが遅い (実測 2.9 秒)",
    },
];

/// 控え (Groq) の整形モデルの候補。先頭が既定 ([`DEFAULT_GROQ_FORMAT_MODEL`])。
pub const GROQ_FORMAT_MODEL_CANDIDATES: &[ModelCandidate] = &[
    ModelCandidate {
        name: DEFAULT_GROQ_FORMAT_MODEL,
        note: "既定・原文に最も忠実 (実測 0.4〜0.6 秒)",
    },
    ModelCandidate {
        name: "openai/gpt-oss-120b",
        note: "良好 (実測 0.6〜1.3 秒)",
    },
    ModelCandidate {
        name: "qwen/qwen3.8-27b",
        note: "最速 (実測 0.2〜0.3 秒)",
    },
];

/// 整形の共通ルールの上限 (文字数)。
///
/// 既定の文は 400 字弱。上限は「指示を書き足す」には十分広く、
/// 「誤って長文を貼り付けた」ときに毎回の整形へ数千トークンを載せ続ける
/// 事故は止める幅にしてある。**超えたら切り詰めずに保存を断る** —
/// 指示の後半を黙って落とすと、利用者は効かない理由を追えない。
pub const MAX_FORMAT_RULES_CHARS: usize = 8_000;

/// 整形モデルの提供元。モデル名の検査規則が提供元ごとに違う
/// ([`validate_model_name`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatProvider {
    /// 主。モデル名は URL のパスへ入る。
    Gemini,
    /// 控え。モデル名は JSON 本文へ入る。
    Groq,
}

impl FormatProvider {
    /// 利用者向けの項目名 (エラー文に使う)。
    pub fn field_label(self) -> &'static str {
        match self {
            FormatProvider::Gemini => "主の整形モデル",
            FormatProvider::Groq => "控えの整形モデル",
        }
    }

    /// 空欄のときに使う既定のモデル。
    pub fn default_model(self) -> &'static str {
        match self {
            FormatProvider::Gemini => DEFAULT_FORMAT_MODEL,
            FormatProvider::Groq => DEFAULT_GROQ_FORMAT_MODEL,
        }
    }
}

/// モデル名の表記を揃える (検査はしない)。
///
/// Gemini は ListModels が `models/gemini-...` の形で返すので、そこから
/// 貼り付けた名前の先頭の `models/` を落とす。落とさないと
/// `.../models/models/gemini-...:generateContent` になって 404 が返り、
/// 利用者からは「名前は合っているのに動かない」としか見えない。
pub fn canonical_model_name(provider: FormatProvider, raw: &str) -> String {
    let trimmed = raw.trim();
    match provider {
        FormatProvider::Gemini => trimmed
            .strip_prefix("models/")
            .unwrap_or(trimmed)
            .trim()
            .to_string(),
        FormatProvider::Groq => trimmed.to_string(),
    }
}

/// モデル名を検査し、揃えた名前を返す (空なら空のまま返す = 既定を使う)。
///
/// # 提供元ごとに規則が違う理由
///
/// - **Gemini**: 名前は `{endpoint}/{model}:generateContent` の**URL のパス**へ
///   そのまま入る ([`Config::gemini_url_for`])。`/` `?` `#` `%` や空白が入ると
///   別のパスやクエリになり、黙って別物を叩く。実在するモデル名は英数字と
///   `.` `_` `-` だけでできているので、**それ以外を全部断る** (許可リスト)
/// - **Groq**: 名前は JSON 本文の `model` に入るだけで、`openai/gpt-oss-20b`
///   のように `/` を正当に含む。断るのは空白と制御文字だけ
///
/// 保存 ([`ConfigPatch::validate`]) と「試す」(`test_format_model`) が
/// **同じこの関数**を通る。別々に書くと、試せたのに保存できない
/// (またはその逆の) 名前ができる。
pub fn validate_model_name(provider: FormatProvider, raw: &str) -> Result<String, String> {
    let name = canonical_model_name(provider, raw);
    let label = provider.field_label();
    match provider {
        FormatProvider::Gemini => {
            if let Some(c) = name
                .chars()
                .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
            {
                return Err(format!(
                    "{label}「{name}」に使えない文字「{}」が含まれています。\
                     Gemini のモデル名は英数字と . _ - だけでできています (例: {DEFAULT_FORMAT_MODEL})",
                    describe_char(c)
                ));
            }
        }
        FormatProvider::Groq => {
            if let Some(c) = name.chars().find(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!(
                    "{label}「{name}」に使えない文字「{}」が含まれています。\
                     モデル名に空白や制御文字は入りません (例: {DEFAULT_GROQ_FORMAT_MODEL})",
                    describe_char(c)
                ));
            }
        }
    }
    Ok(name)
}

/// エラー文に出す 1 文字。空白や制御文字はそのまま出すと見えない。
fn describe_char(c: char) -> String {
    if c == ' ' || c == '\u{3000}' {
        "空白".to_string()
    } else if c.is_whitespace() || c.is_control() {
        format!("U+{:04X}", c as u32)
    } else {
        c.to_string()
    }
}

/// 整形の共通ルールを保存する形へ揃える (検査はしない)。
///
/// 改行を `\n` に揃えて前後の空白を落とし、**同梱の既定と同じなら空にする**。
/// 空 = 既定 ([`crate::format::DEFAULT_FORMAT_RULES`]) という約束で、
/// 既定の全文を設定ファイルへ焼き込まない。焼き込むと、アプリ更新で既定を
/// 直しても既存ユーザーに届かない ([`crate::style`] のモジュール doc にある
/// 既定プロファイルの教訓)。画面は空のとき既定文を表示するので、
/// 「既定に戻す」→ 保存 で空に戻る。
pub fn canonical_format_rules(raw: &str) -> String {
    let unified = raw.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = unified.trim();
    if trimmed == crate::format::DEFAULT_FORMAT_RULES.trim() {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// 整形の共通ルールを検査し、保存する形にして返す。
///
/// 上限 ([`MAX_FORMAT_RULES_CHARS`]) を超えたら**切り詰めずに断る**。
pub fn validate_format_rules(raw: &str) -> Result<String, String> {
    let rules = canonical_format_rules(raw);
    let count = rules.chars().count();
    if count > MAX_FORMAT_RULES_CHARS {
        return Err(format!(
            "整形の共通ルールが長すぎます ({count} 文字 / 上限 {MAX_FORMAT_RULES_CHARS} 文字)。\
             短くしてから保存してください"
        ));
    }
    Ok(rules)
}

/// 履歴の既定保持日数。
pub const DEFAULT_HISTORY_RETENTION_DAYS: u32 = 30;
/// 保持日数の上限 (約 10 年)。0 は「無制限」を意味するので別扱い。
pub const MAX_HISTORY_RETENTION_DAYS: u32 = 3_650;

/// 既定のタイピング速度 (文字/分)。ダッシュボードの「節約時間」計算に使う。
pub const DEFAULT_TYPING_SPEED_CHARS_PER_MIN: u32 = 35;
/// タイピング速度の下限 (文字/分)。
pub const MIN_TYPING_SPEED_CHARS_PER_MIN: u32 = 10;
/// タイピング速度の上限 (文字/分)。
pub const MAX_TYPING_SPEED_CHARS_PER_MIN: u32 = 300;

/// 復元待ちの下限。0 だと貼付が消費される前に戻してしまう。
pub const MIN_RESTORE_DELAY_MS: u64 = 50;
/// 復元待ちの上限。長いほどユーザーの次のコピーを壊す窓が広がる。
pub const MAX_RESTORE_DELAY_MS: u64 = 5_000;

const ENV_GROQ_KEY: &str = "GROQ_API_KEY";
const ENV_GEMINI_KEY: &str = "GEMINI_API_KEY";

/// キーを書く `.env` のファイル名。`config.json` と同じフォルダに置く。
pub const DOTENV_FILE_NAME: &str = ".env";

/// 本番で読む `.env` の場所。**[`set_dotenv_path`] が呼ばれるまでは何も読まない。**
///
/// 起動時 (lib.rs の setup) にだけ設定する。単体テストからは設定しないので、
/// 開発機に本物の `.env` があってもテストの結果は変わらない。
static DOTENV_PATH: OnceLock<PathBuf> = OnceLock::new();

/// `.env` の場所を決める。2 回目以降の呼び出しは無視する (起動時に 1 回だけ)。
pub fn set_dotenv_path(path: PathBuf) {
    let _ = DOTENV_PATH.set(path);
}

/// 設定済みの `.env` の場所 (未設定なら `None`)。
pub fn dotenv_path() -> Option<&'static Path> {
    DOTENV_PATH.get().map(PathBuf::as_path)
}

/// 秘密文字列。表示系はすべて伏字になる。
///
/// `Serialize` / `Deserialize` は素の文字列として振る舞う (設定ファイルに
/// 保存する必要があるため)。**表示に使ってよいのは [`Secret::is_empty`] と
/// [`Secret::preview`] だけ**で、実体は [`Secret::expose`] でしか取り出せない。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    /// 実体を取り出す。**ログ・エラー文字列・IPC 応答に載せないこと。**
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// UI 用の伏字表示 (末尾 4 文字のみ)。キーの同一性確認に使う。
    pub fn preview(&self) -> String {
        let trimmed = self.0.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let tail: String = trimmed
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_empty() {
            "Secret(<empty>)"
        } else {
            "Secret(<redacted>)"
        })
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_empty() {
            "<empty>"
        } else {
            "<redacted>"
        })
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Secret)
    }
}

/// 永続化される設定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub groq_api_key: Secret,
    pub gemini_api_key: Secret,
    /// STT の言語ヒント (ISO-639-1)。空なら自動判定に任せる。
    pub language: String,
    /// 用語リスト (固有名詞・専門用語の表記ゆれ対策)。
    ///
    /// **旧形式 (`["nox-voice", "塩谷,しおや"]` の文字列配列) も読める。**
    /// 移行は [`DictionaryEntry`] の `Deserialize` が引き受ける
    /// (専用の移行処理を書かないので、設定ファイル・IPC パッチ・テストの
    /// どこから来ても同じ形になる)。フィールド単位の `#[serde(default)]`
    /// は他と同じ理由 — コンテナ側の default は [`Config::default()`] の
    /// 値を拾うため、項目が無い設定ファイルの扱いが変わりうる。
    #[serde(default)]
    pub dictionary: Vec<DictionaryEntry>,
    /// LLM 整形を行うか。false なら生転写をそのまま採用する。
    pub formatting_enabled: bool,
    /// 結果を前景アプリへ自動で貼り付けるか。
    /// false なら画面に表示するだけ (手動コピー)。
    pub injection_enabled: bool,
    /// ローカル STT の使い方。
    pub local_stt_mode: LocalSttMode,
    /// ダウンロード済みモデルの SHA-256。
    ///
    /// 初回ダウンロードの成功時に記録し、以後の再ダウンロードで照合する
    /// (TOFU: 最初に取得したものを正とする)。空なら未固定。
    pub local_model_sha256: String,
    /// 起動時にメインウィンドウを出さない (トレイ常駐で始める)。
    ///
    /// 常駐アプリなので既定は「出さない」。ただし**初回起動だけは出す** —
    /// API キーを設定しないと何もできず、窓が出ないと設定画面へ辿り着けない。
    pub start_hidden: bool,
    /// PC 起動時に自動で立ち上げるか (Windows なら `HKCU\...\Run` への登録)。
    ///
    /// **既定は false。** 常駐アプリだからといって、入れた瞬間に勝手に
    /// スタートアップへ入るのは利用者の環境を変える行為で、PRODUCT.md の
    /// 「ユーザーの環境を壊さない」に反する。明示的に選ばれたときだけ登録する。
    ///
    /// **この値は「利用者の意思」であって実際の状態ではない。** 正典は
    /// レジストリ側で、利用者はタスクマネージャーの「スタートアップ アプリ」から
    /// いつでも無効にできるし、設定ファイルを別マシンへ持ち込むこともある。
    /// 食い違いを見つけたら実際の状態へこちらを合わせる (`lib.rs` の setup)。
    ///
    /// [`Self::start_hidden`] とは直交する。こちらは「いつ起動するか」、
    /// あちらは「起動したとき窓を出すか」を決める。
    ///
    /// フィールド単位の `#[serde(default)]` は旧い設定ファイル対策
    /// ([`Self::hotkey_mods`] の doc に理由)。
    #[serde(default)]
    pub autostart: bool,
    /// PTT のトリガー仮想キーコード。既定は Space ([`crate::hotkey::DEFAULT_HOTKEY_VK`])。
    ///
    /// [`Self::hotkey_mods`] が空のときは単独キーとして扱う (旧形式の設定ファイル)。
    pub hotkey_vk: u32,
    /// トリガーと一緒に押す修飾キー (VK コード、表示順に正規化される)。
    ///
    /// 既定は左 Ctrl 1 個 (「左 Ctrl + Space」)。空なら単独キーホットキーで、
    /// このときトリガーは旧基準 ([`crate::hotkey::is_allowed_hotkey`]) で弾かれる。
    /// フィールド単位の `#[serde(default)]` が重要: コンテナ側の default は
    /// [`Config::default()`] の値を使うため、**旧形式の設定ファイル (この項目が
    /// 無い) まで既定の左 Ctrl を拾ってしまい、ホットキーが黙って変わる**。
    /// 空ベクタで受ければ、旧設定は単独キーのまま保たれる。
    #[serde(default)]
    pub hotkey_mods: Vec<u32>,
    /// 録音を破棄するキャンセルキー。既定は Esc。**0 で無効化**。
    ///
    /// ホットキーとの重複は不可 ([`Config::normalize`] が解消する)。
    pub cancel_vk: u32,
    /// キャンセルキーと一緒に押す修飾キー (VK コード、表示順に正規化される)。
    ///
    /// 空なら単独キー。旧形式の設定ファイルには無い項目なので、
    /// フィールド単位の `#[serde(default)]` で空として受ける。
    #[serde(default)]
    pub cancel_mods: Vec<u32>,
    /// 「クリップボードへ入れるだけ」モードのトリガーキー。**0 で無効**。
    ///
    /// 貼り付けモード ([`Self::hotkey_vk`]) とは別のキーを割り当てる。
    /// 既定は未設定 — 勝手にキーを 1 つ占有すると、その組み合わせを
    /// 使っている他アプリの操作を黙って奪うため。
    #[serde(default)]
    pub clipboard_hotkey_vk: u32,
    /// クリップボードのみモードの修飾キー。
    #[serde(default)]
    pub clipboard_hotkey_mods: Vec<u32>,
    /// 画面質問モードを使うか。
    ///
    /// **既定は無効。** 有効にすると、質問したときにモニタ 1 枚分の
    /// 画面 (ウィンドウのテキストと、必要ならスクリーンショット) が
    /// Gemini へ送られる。deep context より踏み込んだ行為なので、
    /// UI に明示すること (design.md「画面質問モード」)。
    ///
    /// **このフラグだけでは動かない。** [`Self::screen_ask_hotkey_vk`] に
    /// 専用キーを割り当てて初めて発火する。ON/OFF ひとつで既存の録音キーに
    /// 相乗りさせると、普通の音声入力のたびに画面が送られてしまう。
    #[serde(default)]
    pub screen_ask_enabled: bool,
    /// 画面質問モードのトリガーキー。**0 で未設定**。
    #[serde(default)]
    pub screen_ask_hotkey_vk: u32,
    /// 画面質問モードの修飾キー。
    #[serde(default)]
    pub screen_ask_hotkey_mods: Vec<u32>,
    /// 通知音を鳴らすか。
    ///
    /// 旧い設定ファイル (この項目が無い) でも**有効**にする。ペダル運用で
    /// 「踏んだのに録音が始まっていない」に気づけないのが、この機能の
    /// そもそもの動機なので、既定を無音にすると誰も気づけない。
    #[serde(default = "default_true")]
    pub sound_enabled: bool,
    /// 通知音の音量 (0〜100)。0 は無音。
    #[serde(default = "default_sound_volume")]
    pub sound_volume: u8,
    /// 録音開始時に鳴らす音。
    #[serde(default)]
    pub start_sound: SoundPreset,
    /// `start_sound == Custom` のときに鳴らす WAV のパス。
    #[serde(default)]
    pub start_sound_path: String,
    /// キャンセル / エラー時に鳴らす音。
    #[serde(default = "default_cancel_sound")]
    pub cancel_sound: SoundPreset,
    /// `cancel_sound == Custom` のときに鳴らす WAV のパス。
    #[serde(default)]
    pub cancel_sound_path: String,
    /// 録音中・処理中の小窓を出すか。
    pub overlay_enabled: bool,
    /// 画面のテキストを読んで文脈として使うか (deep context)。
    ///
    /// **既定は無効。** 有効にすると、挿入先の画面に表示されている文章が
    /// STT / 整形の API へ送られる (design.md R1)。UI でその旨を明示すること。
    pub deep_context: bool,
    /// 貼り付けた後の手直しから辞書を自動学習するか ([`crate::learn`])。
    ///
    /// **既定は有効。** [`Self::deep_context`] が既定無効なのは
    /// 「画面のテキストをクラウドへ送る」からで、こちらは送らない —
    /// 読むのは**自分がたったいま貼り付けた欄だけ**、読んだ本文は
    /// ローカルの差分計算にしか使わず、クラウドへ行くのは抽出された語が
    /// 辞書に載ってからの話 (手動で登録した語と同じ扱い)。
    /// 判断の根拠は design.md 2026-08-30。
    pub auto_learn_dictionary: bool,
    /// 挿入先アプリごとの文体プロファイル。
    pub style_profiles: Vec<StyleProfile>,
    /// 取り込み済みの同梱既定の版 ([`crate::style::STYLE_DEFAULTS_VERSION`])。
    ///
    /// **フィールド単位の `#[serde(default)]` であることが要**。コンテナ側の
    /// default だと [`Config::default()`] の現行版を拾ってしまい、旧い設定
    /// ファイルが「もう最新を取り込み済み」に化けて、拡充した既定が
    /// 永久に届かなくなる (hotkey_mods と同じ罠)。0 = 版管理より前の設定。
    #[serde(default)]
    pub style_defaults_version: u32,
    /// ユーザーが削除した既定プロファイルの id。
    ///
    /// 「まだ取り込んでいない」と「ユーザーが消した」を区別するためだけに
    /// 存在する。これが無いと、アプリ更新のたびに消したはずの既定が
    /// 生き返る。**空の Vec で始まってよい** — 削除は保存時に記録される。
    #[serde(default)]
    pub style_removed_default_ids: Vec<String>,
    /// 履歴を保存するか。
    ///
    /// 履歴には発話の全文が入る。R1 の観点で「残さない」選択肢を用意する。
    /// OFF でも失敗 WAV の退避は続ける (音声を失わせないため)。
    pub history_enabled: bool,
    /// 履歴の保持日数。0 は無制限。
    pub history_retention_days: u32,
    /// 貼付から元クリップボードの復元までの待ち時間 (ms)。
    ///
    /// 短すぎると貼付が消費される前に戻して旧内容が貼られ、
    /// 長すぎるとユーザーの次のコピーを壊しうる (design.md R3-b)。
    pub restore_delay_ms: u64,
    /// 録音結果を貼付後もクリップボードに残すか (Typeless 互換)。
    ///
    /// **既定は残す。** R7 のフォーカス照合は「前景ウィンドウが同じか」しか
    /// 見られないので、同じウィンドウでキャレットが入力欄に無い / 相手が
    /// Ctrl+V を無視した / UIPI で弾かれた、といった失敗はすり抜ける。
    /// そこで復元すると発話が消えて言い直しになるが、残しておけば
    /// Ctrl+V でやり直せる。代償は元のクリップボード内容が上書きされること。
    /// 有効な間 [`restore_delay_ms`](Self::restore_delay_ms) は使われない。
    pub keep_transcript_in_clipboard: bool,
    /// タイピング速度 (文字/分)。ダッシュボードの「節約時間」計算に使う。
    pub typing_speed_chars_per_min: u32,
    pub groq_endpoint: String,
    pub gemini_endpoint: String,
    pub stt_model: String,
    pub format_model: String,
    /// 主 (Gemini) が落ちたとき、控え (Groq) で整形をやり直すか。
    ///
    /// **既定は有効。** 2026-09-05 に Gemini の 503 が続き、その日の録音
    /// 7 件すべてが整形されず生転写のままになった (design.md R2)。
    /// 無効にすると従来どおり 1 段のまま、失敗は生転写へ落ちる。
    ///
    /// ⚠️ 有効な間は、整形プロンプト一式 (生転写・辞書・文体指示、および
    /// deep context が ON なら**画面のテキスト**) が Groq へも行く。
    /// 音声は既に Groq が受け取っているが、画面テキストは新規の送信先になる。
    ///
    /// フィールド単位の `#[serde(default = "...")]` が要る: 旧い設定ファイルに
    /// この項目が無く、コンテナ側の default では拾えないため。
    #[serde(default = "default_true")]
    pub format_fallback_enabled: bool,
    /// 控えの整形モデル ([`DEFAULT_GROQ_FORMAT_MODEL`])。
    #[serde(default = "default_groq_format_model")]
    pub groq_format_model: String,
    /// 控えの chat/completions エンドポイント (**完全な URL**)。
    #[serde(default = "default_groq_chat_endpoint")]
    pub groq_chat_endpoint: String,
    /// 整形の共通ルール (設定画面で編集できる部分)。**空 = 同梱の既定**
    /// ([`crate::format::DEFAULT_FORMAT_RULES`])。
    ///
    /// 既定と同じ文は空で保存する ([`canonical_format_rules`])。既定の全文を
    /// ここへ焼き込むと、アプリ更新で既定を直しても届かなくなる。
    /// 注入への防御と出力形式の決まりはここに含まれず、整形のたびに
    /// [`crate::format::system_prompt`] が必ず付け足す (消せない)。
    ///
    /// フィールド単位の `#[serde(default)]` は他と同じ理由 — 旧い設定ファイルに
    /// この項目が無いとき、コンテナ側の default ではなく「空 = 既定」に倒すため。
    #[serde(default)]
    pub format_rules: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            groq_api_key: Secret::default(),
            gemini_api_key: Secret::default(),
            language: DEFAULT_LANGUAGE.to_string(),
            dictionary: Vec::new(),
            formatting_enabled: true,
            injection_enabled: true,
            local_stt_mode: LocalSttMode::Fallback,
            local_model_sha256: String::new(),
            start_hidden: true,
            // 既定 false。入れただけで勝手にスタートアップへ入らない
            // (フィールドの doc に理由)。
            autostart: false,
            hotkey_vk: crate::hotkey::DEFAULT_HOTKEY_VK,
            hotkey_mods: crate::hotkey::DEFAULT_HOTKEY_MODS.to_vec(),
            cancel_vk: crate::hotkey::DEFAULT_CANCEL_VK,
            cancel_mods: Vec::new(),
            // 既定は未設定。ユーザーが設定 UI で割り当てて初めて効く。
            clipboard_hotkey_vk: 0,
            clipboard_hotkey_mods: Vec::new(),
            // 画面をまるごとクラウドへ送る機能なので、
            // 「有効化」と「キーの割り当て」の 2 つを踏ませる。
            screen_ask_enabled: false,
            screen_ask_hotkey_vk: 0,
            screen_ask_hotkey_mods: Vec::new(),
            sound_enabled: true,
            sound_volume: crate::sound::DEFAULT_VOLUME,
            start_sound: SoundPreset::SoftPop,
            start_sound_path: String::new(),
            cancel_sound: SoundPreset::Fall,
            cancel_sound_path: String::new(),
            overlay_enabled: true,
            // 画面テキストをクラウドへ送るので、明示的に有効化させる。
            deep_context: false,
            // こちらは送らない (フィールドの doc)。既定で効かせないと
            // 「直したのに毎回同じ誤変換が出る」が続くだけなので有効。
            auto_learn_dictionary: true,
            style_profiles: crate::style::default_profiles(),
            // 既定値から作った設定は「現行版を取り込み済み」。ここを 0 に
            // すると、新規ユーザーの初回起動が旧形式移行の経路へ入る。
            style_defaults_version: crate::style::STYLE_DEFAULTS_VERSION,
            style_removed_default_ids: Vec::new(),
            history_enabled: true,
            history_retention_days: DEFAULT_HISTORY_RETENTION_DAYS,
            restore_delay_ms: crate::inject::DEFAULT_RESTORE_DELAY_MS,
            // 貼付に失敗しても言い直さずに済むほうを既定にする。
            keep_transcript_in_clipboard: true,
            typing_speed_chars_per_min: DEFAULT_TYPING_SPEED_CHARS_PER_MIN,
            groq_endpoint: DEFAULT_GROQ_ENDPOINT.to_string(),
            gemini_endpoint: DEFAULT_GEMINI_ENDPOINT.to_string(),
            stt_model: DEFAULT_STT_MODEL.to_string(),
            format_model: DEFAULT_FORMAT_MODEL.to_string(),
            format_fallback_enabled: true,
            groq_format_model: DEFAULT_GROQ_FORMAT_MODEL.to_string(),
            groq_chat_endpoint: DEFAULT_GROQ_CHAT_ENDPOINT.to_string(),
            // 空 = 同梱の既定 (フィールドの doc)。
            format_rules: String::new(),
        }
    }
}

/// ローカル STT (whisper.cpp) の使い方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LocalSttMode {
    /// 使わない (常に Groq)。
    Off,
    /// Groq が失敗したときだけ使う。
    #[default]
    Fallback,
    /// 常にローカルで認識する (クラウドへ音声を送らない)。
    Only,
}

impl LocalSttMode {
    pub fn uses_cloud(self) -> bool {
        !matches!(self, LocalSttMode::Only)
    }
    pub fn allows_local(self) -> bool {
        !matches!(self, LocalSttMode::Off)
    }
}

/// キーの取得元。UI に「どこから来たキーか」を出すために持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeySource {
    /// `config.json` と同じフォルダの `.env` から。**いちばん優先される。**
    ///
    /// 同じ名前の環境変数 (`GROQ_API_KEY` など) を他のアプリも使っていて、
    /// nox-voice だけ別のキーにしたい場合のための層。
    DotEnv,
    /// 環境変数から。設定ファイルより優先される。
    Env,
    /// 設定ファイルから。
    Config,
    /// 未設定。
    None,
}

/// 解決済みのキー (`.env` > 環境変数 > 設定ファイル)。
#[derive(Debug, Clone)]
pub struct ResolvedKey {
    pub secret: Option<Secret>,
    pub source: KeySource,
}

impl ResolvedKey {
    pub fn is_set(&self) -> bool {
        self.secret.is_some()
    }
}

impl Config {
    /// Groq の API キーを解決する (`.env` > 環境変数 > 設定ファイル)。
    ///
    /// `.env` は呼ぶたびに読み直す。書き換えればアプリを再起動しなくても
    /// 次の録音から効く (環境変数は起動時の値のままなので再起動が要る)。
    pub fn groq_key(&self) -> ResolvedKey {
        resolve_key_layers(
            dotenv_value(ENV_GROQ_KEY),
            env_value(ENV_GROQ_KEY),
            &self.groq_api_key,
        )
    }

    /// Gemini の API キーを解決する (`.env` > 環境変数 > 設定ファイル)。
    pub fn gemini_key(&self) -> ResolvedKey {
        resolve_key_layers(
            dotenv_value(ENV_GEMINI_KEY),
            env_value(ENV_GEMINI_KEY),
            &self.gemini_api_key,
        )
    }

    /// 辞書の全語。**正規化済み** ([`Config::normalize`] が空の表記を落とす)。
    pub fn dictionary_entries(&self) -> Vec<DictionaryEntry> {
        self.dictionary.clone()
    }

    /// 「いま何語が Whisper へ渡っているか」。設定画面へ出すために持つ。
    ///
    /// 判定は [`crate::dictionary::select_dictionary_terms`] に一本化する。
    /// UI 側で数え直すと、**画面の表示と実際に送るものがずれる** —
    /// それは今回直した「無言で捨てる」の別の顔にすぎない。
    pub fn dictionary_status(&self) -> DictionaryStatus {
        DictionaryStatus::of(&self.dictionary)
    }

    /// `{gemini_endpoint}/{model}:generateContent` を組み立てる。
    pub fn gemini_url(&self) -> String {
        self.gemini_url_for(&self.format_model)
    }

    /// 任意のモデル名で URL を組み立てる (設定画面の「試す」用)。
    ///
    /// [`Self::gemini_url`] もこれを通す。URL の組み立てを 2 か所に書くと、
    /// 「試すと通るのに保存すると通らない」が生まれる。`model` は
    /// [`validate_model_name`] を通したものを渡すこと。
    pub fn gemini_url_for(&self, model: &str) -> String {
        format!(
            "{}/{}:generateContent",
            self.gemini_endpoint.trim_end_matches('/'),
            model
        )
    }

    /// 整形プロンプトへ渡す共通ルール。空なら `None` (= 同梱の既定)。
    ///
    /// 整形器を組み立てる経路 (新規録音・再転写) はすべてこれを通す。
    pub fn format_rules_for_prompt(&self) -> Option<&str> {
        Some(self.format_rules.trim()).filter(|r| !r.is_empty())
    }

    /// ホットキーの組み合わせを返す。
    ///
    /// [`Self::normalize`] 済みの設定なら必ず `Some` (不正値は既定へ倒済み)。
    /// 念のため不正値が残っていても既定に落として返す。
    pub fn hotkey_combo(&self) -> crate::hotkey::HotkeyCombo {
        crate::hotkey::HotkeyCombo::from_parts(&self.hotkey_mods, self.hotkey_vk)
            .unwrap_or_default()
    }

    /// 「クリップボードのみ」モードの組み合わせ。未設定なら `None`。
    ///
    /// こちらは `unwrap_or_default()` してはいけない。既定へ倒すと
    /// **設定していないのに左 Ctrl + Space が 2 つの用途に割り当たる**。
    pub fn clipboard_hotkey_combo(&self) -> Option<crate::hotkey::HotkeyCombo> {
        if self.clipboard_hotkey_vk == 0 {
            return None;
        }
        crate::hotkey::HotkeyCombo::from_parts(
            &self.clipboard_hotkey_mods,
            self.clipboard_hotkey_vk,
        )
    }

    /// 画面質問モードの組み合わせ。**無効か未設定なら `None`**。
    ///
    /// 有効化フラグをここで見るのが要点。フラグを外したのにキーだけ残って
    /// いると、設定画面では「オフ」なのにキーを押すと画面が送られる、
    /// という一番まずい食い違いが起きる。判定を 1 か所に閉じ込めておけば、
    /// 呼び出し側 (`apply_hotkeys`) が両方を見忘れることはない。
    pub fn screen_ask_hotkey_combo(&self) -> Option<crate::hotkey::HotkeyCombo> {
        if !self.screen_ask_enabled || self.screen_ask_hotkey_vk == 0 {
            return None;
        }
        crate::hotkey::HotkeyCombo::from_parts(
            &self.screen_ask_hotkey_mods,
            self.screen_ask_hotkey_vk,
        )
    }

    /// 録音を破棄するキーの組み合わせ。無効なら `None`。
    pub fn cancel_combo(&self) -> Option<crate::hotkey::HotkeyCombo> {
        crate::hotkey::sanitize_cancel_combo(&self.cancel_mods, self.cancel_vk)
    }

    /// `combo` が割り当て済みのホットキーのどれかと衝突するか
    /// ([`crate::hotkey::cancel_clashes_with`])。
    ///
    /// 画面質問は有効化フラグを見ない。機能をオンにした瞬間に衝突が生まれて
    /// 取り消しキーが黙って変わるのを防ぐため、キーがあれば数える。
    pub fn cancel_clashes(&self, combo: &crate::hotkey::HotkeyCombo) -> bool {
        let screen_ask = crate::hotkey::HotkeyCombo::from_parts(
            &self.screen_ask_hotkey_mods,
            self.screen_ask_hotkey_vk,
        );
        [
            Some(self.hotkey_combo()),
            self.clipboard_hotkey_combo(),
            screen_ask,
        ]
        .iter()
        .flatten()
        .any(|hotkey| crate::hotkey::cancel_clashes_with(combo, hotkey))
    }

    /// 録音開始時に鳴らす音。
    pub fn start_sound_choice(&self) -> SoundChoice {
        SoundChoice::new(self.start_sound, &self.start_sound_path)
    }

    /// キャンセル / エラー時に鳴らす音。
    pub fn cancel_sound_choice(&self) -> SoundChoice {
        SoundChoice::new(self.cancel_sound, &self.cancel_sound_path)
    }

    /// 通知音の音量。無効なら 0 (= 鳴らさない)。
    pub fn effective_sound_volume(&self) -> u8 {
        if self.sound_enabled {
            self.sound_volume.min(100)
        } else {
            0
        }
    }
}

/// serde のフィールド既定値 (旧い設定ファイルの補完用)。
fn default_true() -> bool {
    true
}

fn default_sound_volume() -> u8 {
    crate::sound::DEFAULT_VOLUME
}

fn default_groq_format_model() -> String {
    DEFAULT_GROQ_FORMAT_MODEL.to_string()
}

fn default_groq_chat_endpoint() -> String {
    DEFAULT_GROQ_CHAT_ENDPOINT.to_string()
}

/// キャンセル音の既定。開始音と**別の音**にする (聞き分けが要る)。
fn default_cancel_sound() -> SoundPreset {
    SoundPreset::Fall
}

/// 現在時刻 (UNIX epoch ミリ秒)。時計が 1970 より前を指していれば 1。
///
/// 0 を返さないのが要点。0 は辞書で「日時不明」の予約値なので、
/// 返してしまうと [`Config::normalize_dictionary`] が毎回入れ直しに来る。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(1)
        .max(1)
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// 設定済みの `.env` から `name` の値を読む。ファイルが無ければ `None`。
///
/// **プロセスの環境変数には書き込まない** (`dotenv()` 系の読み込みはしない)。
/// 書き込むと子プロセスにキーが渡り、他の箇所の `std::env::var` の結果も変わる。
fn dotenv_value(name: &str) -> Option<String> {
    let path = dotenv_path()?;
    match fs::read(path) {
        Ok(bytes) => parse_dotenv_value(&decode_dotenv(&bytes), name),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            // 中身はキーなので出さない。場所と原因だけ書く。
            log::warn!(".env を読めません ({}): {e}", path.display());
            None
        }
    }
}

/// `.env` のバイト列を文字列にする。
///
/// Windows で作られうる形をどれも受ける: UTF-8 (BOM 付き = メモ帳の旧既定) と、
/// UTF-16 LE (BOM 付き = Windows PowerShell 5.1 の `>` / `Out-File` の既定)。
/// それ以外は UTF-8 として読み、壊れたバイトは置き換える (キーは ASCII なので
/// 他の行に変な文字があっても目的の行は読める)。
fn decode_dotenv(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(rest).into_owned()
}

/// `.env` の本文から `name` の値を取り出す。空の値は「書いていない」と同じ扱い。
///
/// 書式: `NAME=値` を 1 行に 1 つ。`#` で始まる行は注釈。行頭の `export` (後ろに
/// 空白) は無視する。値は前後の空白を落とし、引用符 (`"` か `'`) で始まれば
/// 閉じる引用符までを値にする (後ろの注釈は捨てる)。囲まれていない値は、
/// **空白の直後の `#`** から先を注釈として捨てる (`K= # 後で入れる` は空値)。
/// 同じ名前が複数あれば**後の行が勝つ** (シェルで `source` したときと同じ)。
fn parse_dotenv_value(text: &str, name: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .strip_prefix("export")
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != name {
            continue;
        }
        // trim する前の値を渡す。`=` の直後の空白も「注釈の前の空白」に数えたい。
        found = Some(unquote_dotenv_value(value));
    }
    found.filter(|v| !v.is_empty())
}

fn unquote_dotenv_value(raw: &str) -> String {
    let value = raw.trim();
    if let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        // 引用符は 1 バイトなので、[1..] と [1..1 + end] は常に文字境界。
        if let Some(end) = value[1..].find(quote) {
            return value[1..1 + end].trim().to_string();
        }
    }
    // 空白 (スペース・タブ) の直後の `#` から先は注釈。`a#b` の `#` は値の一部。
    let comment = raw
        .char_indices()
        .find(|&(i, c)| c == '#' && raw[..i].ends_with(char::is_whitespace))
        .map(|(i, _)| i);
    match comment {
        Some(i) => raw[..i].trim().to_string(),
        None => value.to_string(),
    }
}

/// `.env` > 環境変数 > 設定ファイル > 未設定 の優先順で解決する。
fn resolve_key_layers(
    from_dotenv: Option<String>,
    from_env: Option<String>,
    configured: &Secret,
) -> ResolvedKey {
    if let Some(v) = from_dotenv {
        return ResolvedKey {
            secret: Some(Secret::new(v.trim())),
            source: KeySource::DotEnv,
        };
    }
    resolve_key(from_env, configured)
}

/// 環境変数 > 設定ファイル > 未設定 の優先順で解決する。
fn resolve_key(from_env: Option<String>, configured: &Secret) -> ResolvedKey {
    if let Some(v) = from_env {
        return ResolvedKey {
            secret: Some(Secret::new(v.trim())),
            source: KeySource::Env,
        };
    }
    if !configured.is_empty() {
        return ResolvedKey {
            secret: Some(Secret::new(configured.expose().trim())),
            source: KeySource::Config,
        };
    }
    ResolvedKey {
        secret: None,
        source: KeySource::None,
    }
}

/// フロントへ返す設定ビュー。**キーの実体は決して含めない。**
#[derive(Debug, Clone, Serialize)]
pub struct ConfigView {
    pub groq_key_set: bool,
    pub groq_key_source: KeySource,
    pub groq_key_preview: String,
    pub gemini_key_set: bool,
    pub gemini_key_source: KeySource,
    pub gemini_key_preview: String,
    /// キーを書ける `.env` の場所 (設定画面の案内に出す)。未設定なら空。
    pub dotenv_path: String,
    pub language: String,
    pub dictionary: Vec<DictionaryEntry>,
    /// 辞書の何語が実際に `prompt` へ載っているか (行と 1 対 1 に対応)。
    ///
    /// **溢れた語を黙って捨てないための項目**。ここが無いと、ユーザーには
    /// 「登録したのに効かない」としか見えない。
    pub dictionary_status: DictionaryStatus,
    pub formatting_enabled: bool,
    pub injection_enabled: bool,
    pub deep_context: bool,
    /// 貼付後の手直しから辞書を自動学習するか。**既定は有効**
    /// ([`Config::auto_learn_dictionary`] の doc に理由)。
    pub auto_learn_dictionary: bool,
    pub style_profiles: Vec<StyleProfile>,
    pub local_stt_mode: LocalSttMode,
    pub start_hidden: bool,
    /// 設定ファイルに記録された「自動起動の意思」。
    ///
    /// **UI のトグルはこれを見てはいけない。** 実際に登録されているかは
    /// `get_autostart` (レジストリ) で問い合わせる。ここに出すのは、
    /// 意思と実際の食い違いを診断したい場合のためだけ
    /// ([`Config::autostart`] の doc)。
    pub autostart: bool,
    pub hotkey_vk: u32,
    /// ホットキーの修飾キー (空なら単独キー)。
    pub hotkey_mods: Vec<u32>,
    /// 表示用のキー名 (「左 Ctrl + Space」など)。
    pub hotkey_label: String,
    /// 録音キャンセルキーのトリガー。0 は無効。
    pub cancel_vk: u32,
    pub cancel_mods: Vec<u32>,
    /// 録音キャンセルキーの表示名 (「Esc」など)。無効なら空。
    pub cancel_label: String,
    /// クリップボードのみモードのトリガー (0 = 未設定)。
    pub clipboard_hotkey_vk: u32,
    pub clipboard_hotkey_mods: Vec<u32>,
    /// クリップボードのみモードの表示用ラベル (未設定なら空文字)。
    pub clipboard_hotkey_label: String,
    /// 画面質問モードが有効か (**キーの割り当てとは別**)。
    pub screen_ask_enabled: bool,
    /// 画面質問モードのトリガー (0 = 未設定)。
    pub screen_ask_hotkey_vk: u32,
    pub screen_ask_hotkey_mods: Vec<u32>,
    /// 画面質問モードの表示用ラベル (未設定なら空文字)。
    ///
    /// **有効化フラグを見ない**。無効のときも「割り当ててあるキー」は
    /// 見せる — 有効化した瞬間に何のキーで動くのかが分からないと、
    /// トグルを押すのが怖い機能になる。実際に効くかどうかは
    /// [`Self::screen_ask_enabled`] で判断すること。
    pub screen_ask_hotkey_label: String,
    pub sound_enabled: bool,
    pub sound_volume: u8,
    pub start_sound: SoundPreset,
    pub start_sound_path: String,
    pub cancel_sound: SoundPreset,
    pub cancel_sound_path: String,
    pub overlay_enabled: bool,
    pub history_enabled: bool,
    pub history_retention_days: u32,
    pub restore_delay_ms: u64,
    pub keep_transcript_in_clipboard: bool,
    pub typing_speed_chars_per_min: u32,
    pub stt_model: String,
    pub format_model: String,
    /// 主が落ちたときに控え (Groq) で整形し直すか。
    pub format_fallback_enabled: bool,
    pub groq_format_model: String,
    /// 保存されている共通ルール。**空なら既定を使っている**。
    pub format_rules: String,
    /// 同梱の既定ルール。画面の「既定に戻す」と、空のときの表示に使う。
    /// フロントに写しを置かない (写しは更新から取り残される)。
    pub default_format_rules: &'static str,
    /// 共通ルールの上限 (文字数)。入力欄の `maxlength` に使う。
    pub format_rules_max_chars: usize,
    /// 主 / 控えの既定モデル (入力欄の placeholder と説明に使う)。
    pub default_format_model: &'static str,
    pub default_groq_format_model: &'static str,
    /// 候補リスト (実測で良好だったもの)。
    pub format_model_candidates: &'static [ModelCandidate],
    pub groq_format_model_candidates: &'static [ModelCandidate],
}

impl ConfigView {
    /// 解決済みキーを明示して組み立てる。
    ///
    /// 環境変数を読む [`From<&Config>`] と分けてあるのは、テストを
    /// 実行環境の環境変数に依存させないため (開発機に本物のキーが
    /// 入っていると、素朴なテストは通ったり落ちたりする)。
    pub fn build(c: &Config, groq: ResolvedKey, gemini: ResolvedKey) -> Self {
        Self {
            groq_key_set: groq.is_set(),
            groq_key_source: groq.source,
            groq_key_preview: groq
                .secret
                .as_ref()
                .map(Secret::preview)
                .unwrap_or_default(),
            gemini_key_set: gemini.is_set(),
            gemini_key_source: gemini.source,
            gemini_key_preview: gemini
                .secret
                .as_ref()
                .map(Secret::preview)
                .unwrap_or_default(),
            dotenv_path: dotenv_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            language: c.language.clone(),
            dictionary: c.dictionary.clone(),
            dictionary_status: c.dictionary_status(),
            formatting_enabled: c.formatting_enabled,
            injection_enabled: c.injection_enabled,
            deep_context: c.deep_context,
            auto_learn_dictionary: c.auto_learn_dictionary,
            style_profiles: c.style_profiles.clone(),
            local_stt_mode: c.local_stt_mode,
            start_hidden: c.start_hidden,
            autostart: c.autostart,
            hotkey_vk: c.hotkey_vk,
            hotkey_mods: c.hotkey_combo().mods_vec(),
            hotkey_label: c.hotkey_combo().label(),
            cancel_vk: c.cancel_vk,
            cancel_mods: c.cancel_mods.clone(),
            cancel_label: c
                .cancel_combo()
                .map(|combo| combo.label())
                .unwrap_or_default(),
            clipboard_hotkey_vk: c.clipboard_hotkey_vk,
            clipboard_hotkey_mods: c
                .clipboard_hotkey_combo()
                .map(|combo| combo.mods_vec())
                .unwrap_or_default(),
            clipboard_hotkey_label: c
                .clipboard_hotkey_combo()
                .map(|combo| combo.label())
                .unwrap_or_default(),
            screen_ask_enabled: c.screen_ask_enabled,
            screen_ask_hotkey_vk: c.screen_ask_hotkey_vk,
            screen_ask_hotkey_mods: c.screen_ask_hotkey_mods.clone(),
            // 有効化フラグを通さない accessor をわざと使わない:
            // 無効でも割り当て済みのキーは見せる (フィールドの doc 参照)。
            screen_ask_hotkey_label: crate::hotkey::HotkeyCombo::from_parts(
                &c.screen_ask_hotkey_mods,
                c.screen_ask_hotkey_vk,
            )
            .map(|combo| combo.label())
            .unwrap_or_default(),
            sound_enabled: c.sound_enabled,
            sound_volume: c.sound_volume,
            start_sound: c.start_sound,
            start_sound_path: c.start_sound_path.clone(),
            cancel_sound: c.cancel_sound,
            cancel_sound_path: c.cancel_sound_path.clone(),
            overlay_enabled: c.overlay_enabled,
            history_enabled: c.history_enabled,
            history_retention_days: c.history_retention_days,
            restore_delay_ms: c.restore_delay_ms,
            keep_transcript_in_clipboard: c.keep_transcript_in_clipboard,
            typing_speed_chars_per_min: c.typing_speed_chars_per_min,
            stt_model: c.stt_model.clone(),
            format_model: c.format_model.clone(),
            format_fallback_enabled: c.format_fallback_enabled,
            groq_format_model: c.groq_format_model.clone(),
            format_rules: c.format_rules.clone(),
            default_format_rules: crate::format::DEFAULT_FORMAT_RULES,
            format_rules_max_chars: MAX_FORMAT_RULES_CHARS,
            default_format_model: DEFAULT_FORMAT_MODEL,
            default_groq_format_model: DEFAULT_GROQ_FORMAT_MODEL,
            format_model_candidates: GEMINI_FORMAT_MODEL_CANDIDATES,
            groq_format_model_candidates: GROQ_FORMAT_MODEL_CANDIDATES,
        }
    }
}

impl From<&Config> for ConfigView {
    fn from(c: &Config) -> Self {
        ConfigView::build(c, c.groq_key(), c.gemini_key())
    }
}

/// フロントからの部分更新。`None` のフィールドは変更しない。
///
/// キーは「空文字を送ると消去、未指定なら据え置き」。誤って空で
/// 上書きしないよう、明示的に `Some("")` のときだけ消す。
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct ConfigPatch {
    // 生の String なので、Debug を derive すると `{:?}` でキーが漏れる。
    // 手書きの Debug で伏せる (下の impl を参照)。
    pub groq_api_key: Option<String>,
    pub gemini_api_key: Option<String>,
    pub language: Option<String>,
    /// 辞書の全置換。**旧形式の文字列配列も受ける** (フィールドの doc)。
    pub dictionary: Option<Vec<DictionaryEntry>>,
    pub formatting_enabled: Option<bool>,
    pub injection_enabled: Option<bool>,
    pub deep_context: Option<bool>,
    pub auto_learn_dictionary: Option<bool>,
    pub style_profiles: Option<Vec<StyleProfile>>,
    pub local_stt_mode: Option<LocalSttMode>,
    pub local_model_sha256: Option<String>,
    pub start_hidden: Option<bool>,
    /// 自動起動の意思。**フロントの設定フォームからは送らない** —
    /// 登録はレジストリへの操作を伴うので `set_autostart` が唯一の入口で、
    /// ここへ直接書けると「設定ファイルだけ true、実体は未登録」を作れてしまう。
    pub autostart: Option<bool>,
    pub hotkey_vk: Option<u32>,
    pub hotkey_mods: Option<Vec<u32>>,
    pub cancel_vk: Option<u32>,
    pub cancel_mods: Option<Vec<u32>>,
    pub clipboard_hotkey_vk: Option<u32>,
    pub clipboard_hotkey_mods: Option<Vec<u32>>,
    pub screen_ask_enabled: Option<bool>,
    pub screen_ask_hotkey_vk: Option<u32>,
    pub screen_ask_hotkey_mods: Option<Vec<u32>>,
    pub sound_enabled: Option<bool>,
    pub sound_volume: Option<u8>,
    pub start_sound: Option<SoundPreset>,
    pub start_sound_path: Option<String>,
    pub cancel_sound: Option<SoundPreset>,
    pub cancel_sound_path: Option<String>,
    pub overlay_enabled: Option<bool>,
    pub history_enabled: Option<bool>,
    pub history_retention_days: Option<u32>,
    pub restore_delay_ms: Option<u64>,
    pub keep_transcript_in_clipboard: Option<bool>,
    pub typing_speed_chars_per_min: Option<u32>,
    pub stt_model: Option<String>,
    pub format_model: Option<String>,
    pub groq_endpoint: Option<String>,
    pub gemini_endpoint: Option<String>,
    pub format_fallback_enabled: Option<bool>,
    pub groq_format_model: Option<String>,
    pub groq_chat_endpoint: Option<String>,
    /// 整形の共通ルール。空文字・既定と同じ文は「既定を使う」(空) として保存する。
    pub format_rules: Option<String>,
}

impl fmt::Debug for ConfigPatch {
    /// キーは「指定の有無」だけ見せる。値は絶対に出さない。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn presence(v: &Option<String>) -> &'static str {
            match v {
                None => "None",
                Some(s) if s.trim().is_empty() => "Some(<empty>)",
                Some(_) => "Some(<redacted>)",
            }
        }
        f.debug_struct("ConfigPatch")
            .field(
                "groq_api_key",
                &format_args!("{}", presence(&self.groq_api_key)),
            )
            .field(
                "gemini_api_key",
                &format_args!("{}", presence(&self.gemini_api_key)),
            )
            .field("language", &self.language)
            .field("dictionary", &self.dictionary.as_ref().map(Vec::len))
            .field("formatting_enabled", &self.formatting_enabled)
            .field("injection_enabled", &self.injection_enabled)
            .field("deep_context", &self.deep_context)
            .field("auto_learn_dictionary", &self.auto_learn_dictionary)
            .field("local_stt_mode", &self.local_stt_mode)
            .field("start_hidden", &self.start_hidden)
            .field("autostart", &self.autostart)
            .field("hotkey_vk", &self.hotkey_vk)
            .field("hotkey_mods", &self.hotkey_mods.as_ref().map(Vec::len))
            .field("cancel_vk", &self.cancel_vk)
            .field("cancel_mods", &self.cancel_mods.as_ref().map(Vec::len))
            .field("clipboard_hotkey_vk", &self.clipboard_hotkey_vk)
            .field(
                "clipboard_hotkey_mods",
                &self.clipboard_hotkey_mods.as_ref().map(Vec::len),
            )
            .field("screen_ask_enabled", &self.screen_ask_enabled)
            .field("screen_ask_hotkey_vk", &self.screen_ask_hotkey_vk)
            .field(
                "screen_ask_hotkey_mods",
                &self.screen_ask_hotkey_mods.as_ref().map(Vec::len),
            )
            .field("sound_enabled", &self.sound_enabled)
            .field("sound_volume", &self.sound_volume)
            .field("start_sound", &self.start_sound)
            .field("cancel_sound", &self.cancel_sound)
            .field("overlay_enabled", &self.overlay_enabled)
            .field(
                "style_profiles",
                &self.style_profiles.as_ref().map(Vec::len),
            )
            .field("history_enabled", &self.history_enabled)
            .field("history_retention_days", &self.history_retention_days)
            .field("restore_delay_ms", &self.restore_delay_ms)
            .field(
                "keep_transcript_in_clipboard",
                &self.keep_transcript_in_clipboard,
            )
            .field(
                "typing_speed_chars_per_min",
                &self.typing_speed_chars_per_min,
            )
            .field("stt_model", &self.stt_model)
            .field("format_model", &self.format_model)
            .field("groq_endpoint", &self.groq_endpoint)
            .field("gemini_endpoint", &self.gemini_endpoint)
            .field("format_fallback_enabled", &self.format_fallback_enabled)
            .field("groq_format_model", &self.groq_format_model)
            .field("groq_chat_endpoint", &self.groq_chat_endpoint)
            // 本文は長くなりうるので文字数だけ。
            .field(
                "format_rules",
                &self.format_rules.as_ref().map(|r| r.chars().count()),
            )
            .finish()
    }
}

impl ConfigPatch {
    /// 保存してよい値か検査する。**1 つでも駄目なら何も変更しない**。
    ///
    /// [`Config::apply`] は失敗しない作りなので (読み込み時の [`Config::normalize`]
    /// と共有している)、断る判断はここで先に済ませる。[`ConfigStore::update`] は
    /// これを通ってから適用するので、断られた保存は設定を半端に書き換えない。
    ///
    /// 断るとき、エラー文は**どの項目の何が駄目か**を名指しする。保存は
    /// 全項目まとめて断られるので、名指しが無いと何を直せばよいか分からない。
    pub fn validate(&self) -> Result<(), String> {
        if let Some(model) = &self.format_model {
            validate_model_name(FormatProvider::Gemini, model)?;
        }
        if let Some(model) = &self.groq_format_model {
            validate_model_name(FormatProvider::Groq, model)?;
        }
        if let Some(rules) = &self.format_rules {
            validate_format_rules(rules)?;
        }
        Ok(())
    }
}

impl Config {
    /// 範囲外の値を安全な範囲へ丸める。
    ///
    /// パッチ適用時だけでなく**読み込み時にも**通すこと。設定ファイルは
    /// 手で編集されうるので、UI を通らない値が入ってくる。
    pub fn normalize(&mut self) {
        self.normalize_dictionary();
        // 捕獲 UI と同じ不変条件をここでも守る。設定ファイルは手で編集できるので、
        // UI を通らない値 (文字キー・Enter・マウス・範囲外) が入りうる。
        // 文字キーが入ると、押している間ずっと入力先へ流れ続ける。
        match crate::hotkey::sanitize_combo(&self.hotkey_mods, self.hotkey_vk) {
            Some(combo) => {
                self.hotkey_mods = combo.mods_vec();
                self.hotkey_vk = combo.vk;
            }
            None => {
                log::warn!(
                    "ホットキーに使えない組み合わせが設定されていたので既定へ戻します \
                     (トリガー VK 0x{:02X} / 修飾子 {:?})",
                    self.hotkey_vk,
                    self.hotkey_mods
                );
                self.hotkey_mods = crate::hotkey::DEFAULT_HOTKEY_MODS.to_vec();
                self.hotkey_vk = crate::hotkey::DEFAULT_HOTKEY_VK;
            }
        }
        // 「クリップボードのみ」モードの組み合わせ。
        //
        // 未設定 (vk == 0) は正しい値なのでそのまま通す。設定されている場合は
        // 貼り付け用と同じ要件で検査し、**同じ組み合わせなら後勝ちにせず
        // クリップボード側を無効化する**。同じキーに 2 つの用途を割り当てると、
        // どちらが動いたのかユーザーには区別が付かない。
        if self.clipboard_hotkey_vk != 0 {
            match crate::hotkey::sanitize_combo(
                &self.clipboard_hotkey_mods,
                self.clipboard_hotkey_vk,
            ) {
                Some(combo) if combo == self.hotkey_combo() => {
                    log::warn!(
                        "クリップボードのみモードのホットキーが貼り付け用 ({}) と同じなので無効にします",
                        combo.label()
                    );
                    self.clipboard_hotkey_vk = 0;
                    self.clipboard_hotkey_mods.clear();
                }
                Some(combo) => {
                    self.clipboard_hotkey_mods = combo.mods_vec();
                    self.clipboard_hotkey_vk = combo.vk;
                }
                None => {
                    log::warn!(
                        "クリップボードのみモードに使えない組み合わせが設定されていたので無効にします                          (トリガー VK 0x{:02X} / 修飾子 {:?})",
                        self.clipboard_hotkey_vk,
                        self.clipboard_hotkey_mods
                    );
                    self.clipboard_hotkey_vk = 0;
                    self.clipboard_hotkey_mods.clear();
                }
            }
        } else {
            // トリガーが無いのに修飾子だけ残っていると、UI の表示が嘘になる。
            self.clipboard_hotkey_mods.clear();
        }
        // 画面質問モードの組み合わせ。要件は上の 2 つと同じで、
        // **どちらとも重複してはいけない**。同じキーに 2 つの用途が乗ると、
        // どちらが動いたかユーザーに区別が付かない — しかもこの用途は
        // 「画面を送る」なので、取り違えの代償が他と違う。
        // 重複時は後から足したこちらを無効化する (既存の割り当てを壊さない)。
        if self.screen_ask_hotkey_vk != 0 {
            match crate::hotkey::sanitize_combo(
                &self.screen_ask_hotkey_mods,
                self.screen_ask_hotkey_vk,
            ) {
                Some(combo)
                    if combo == self.hotkey_combo()
                        || Some(combo) == self.clipboard_hotkey_combo() =>
                {
                    log::warn!(
                        "画面質問モードのホットキーが他の用途 ({}) と同じなので無効にします",
                        combo.label()
                    );
                    self.screen_ask_hotkey_vk = 0;
                    self.screen_ask_hotkey_mods.clear();
                }
                Some(combo) => {
                    self.screen_ask_hotkey_mods = combo.mods_vec();
                    self.screen_ask_hotkey_vk = combo.vk;
                }
                None => {
                    log::warn!(
                        "画面質問モードに使えない組み合わせが設定されていたので無効にします                          (トリガー VK 0x{:02X} / 修飾子 {:?})",
                        self.screen_ask_hotkey_vk,
                        self.screen_ask_hotkey_mods
                    );
                    self.screen_ask_hotkey_vk = 0;
                    self.screen_ask_hotkey_mods.clear();
                }
            }
        } else {
            self.screen_ask_hotkey_mods.clear();
        }
        // キャンセルキーはホットキーと要件が違う (録音中の 1 回押しなので Esc や
        // 文字キーも可。hotkey.rs の is_allowed_cancel_vk を参照)。マウスや
        // VK が揺れる IME 系だけ弾く。ただし **0 は「無効化」という意味の
        // 正しい値**なので、そのまま通す。
        if self.cancel_vk == 0 {
            // トリガーが無いのに修飾子だけ残っていると、UI の表示が嘘になる。
            self.cancel_mods.clear();
        } else if let Some(combo) = self.cancel_combo() {
            self.cancel_mods = combo.mods_vec();
        } else {
            log::warn!(
                "キャンセルキーに使えない値 (VK 0x{:02X}) が設定されていたので既定へ戻します",
                self.cancel_vk
            );
            self.cancel_vk = crate::hotkey::DEFAULT_CANCEL_VK;
            self.cancel_mods.clear();
        }
        // ホットキーとの重複は禁止。トリガーと同じキーや、組み合わせの修飾子と
        // 同じキーをキャンセルにすると、押すたびに録音操作とキャンセルが同時に
        // 起こってしまう。キャンセル側を既定 (Esc) へ戻して解消する — ホットキーは
        // Esc を選べないので、これで必ず外れる。
        //
        // 設定 UI はここへ来る前に断る (`lib.rs` の `cancel_clash_message`)。
        // ここは手編集された設定ファイルの受け皿。
        if self
            .cancel_combo()
            .is_some_and(|combo| self.cancel_clashes(&combo))
        {
            log::warn!(
                "キャンセルキーがホットキー ({}) と重複していたため、既定の {} へ戻します",
                self.hotkey_combo().label(),
                crate::hotkey::key_label(crate::hotkey::DEFAULT_CANCEL_VK)
            );
            self.cancel_vk = crate::hotkey::DEFAULT_CANCEL_VK;
            self.cancel_mods.clear();
        }
        self.restore_delay_ms = self
            .restore_delay_ms
            .clamp(MIN_RESTORE_DELAY_MS, MAX_RESTORE_DELAY_MS);
        // 0 は「無制限」という意味を持つので潰さない。
        if self.history_retention_days != 0 {
            self.history_retention_days =
                self.history_retention_days.min(MAX_HISTORY_RETENTION_DAYS);
        }
        // 通知音。音量は 0..=100、カスタム音はパスが無ければ鳴らしようがない。
        self.sound_volume = self.sound_volume.min(100);
        self.start_sound_path = self.start_sound_path.trim().to_string();
        self.cancel_sound_path = self.cancel_sound_path.trim().to_string();
        for (preset, path, what) in [
            (&mut self.start_sound, &self.start_sound_path, "録音開始音"),
            (&mut self.cancel_sound, &self.cancel_sound_path, "キャンセル音"),
        ] {
            if *preset == SoundPreset::Custom && path.is_empty() {
                log::warn!("{what}にファイルが指定されていないので無音にします");
                *preset = SoundPreset::Silent;
            }
        }
        // タイピング速度を 10..=300 文字/分にクランプする。
        // 0 は「節約時間を計算しない」という意味の正しい値なので潰さない
        // (get_dashboard_stats が 0 を特別扱いする)。
        if self.typing_speed_chars_per_min != 0 {
            self.typing_speed_chars_per_min = self.typing_speed_chars_per_min.clamp(
                MIN_TYPING_SPEED_CHARS_PER_MIN,
                MAX_TYPING_SPEED_CHARS_PER_MIN,
            );
        }
        // 整形モデル。保存の経路は ConfigPatch::validate で断っているので、
        // ここへ不正な名前が来るのは設定ファイルを手で書いたときだけ。
        // ホットキーと同じく、警告を残して既定へ戻す (URL を壊す名前のまま
        // 走らせると、毎回の整形が理由の読みにくい 404 で落ち続ける)。
        self.format_model = normalized_model(FormatProvider::Gemini, &self.format_model);
        self.groq_format_model =
            normalized_model(FormatProvider::Groq, &self.groq_format_model);
        // 共通ルール。改行の統一と「既定と同じなら空」はここで揃える。
        // **上限超えは読み込み時には断らない** — 手で書いた長文を黙って
        // 捨てる (または切り詰める) より、そのまま使い、画面から保存する
        // ときに理由つきで断る方が利用者に見える。
        self.format_rules = canonical_format_rules(&self.format_rules);
    }

    /// 辞書の不変条件を整える。
    ///
    /// 1. 表記と読みの前後空白を落とし、**表記が空の語は捨てる**。
    ///    空の表記は `prompt` に入れようがないうえ、[`ConfigView`] の
    ///    行と [`DictionaryStatus`] の判定を 1 対 1 に保てなくなる。
    /// 2. 読みが空文字なら `None` に倒す (「読み無し」と同義にする)。
    /// 3. **追加日時が 0 (不明) の語に現在時刻を入れる。** 旧形式から
    ///    移行した語はここで横並びになり、同着は登録順で解ける
    ///    ([`crate::dictionary::select_dictionary_terms`])。0 のまま
    ///    残すと、後から足した語だけが常に勝ち続けて古い語が二度と
    ///    載らなくなる。
    fn normalize_dictionary(&mut self) {
        let now = now_ms();
        self.dictionary.retain_mut(|entry| {
            entry.written = entry.written.trim().to_string();
            entry.reading = entry
                .reading
                .take()
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty());
            if entry.added_at_ms <= 0 {
                entry.added_at_ms = now;
            }
            !entry.written.is_empty()
        });
    }

    /// 貼付後にクリップボードをどう扱うか。
    ///
    /// 「残すなら復元ディレイは使わない」という関係は、bool と Duration を
    /// 別々に持ち回すと呼び出し側ごとに書き直すことになる。ここで一度だけ
    /// 型へ畳んでおく (design.md「同じ判断を 2 か所に書かない」)。
    pub fn clipboard_policy(&self) -> crate::inject::ClipboardPolicy {
        if self.keep_transcript_in_clipboard {
            crate::inject::ClipboardPolicy::Keep
        } else {
            crate::inject::ClipboardPolicy::Restore {
                delay: std::time::Duration::from_millis(self.restore_delay_ms),
            }
        }
    }

    /// 文体プロファイルの一覧を UI から来たもので置き換える。
    ///
    /// ここが版管理の**書き込み側**。UI は「プロセス名 / タイトル条件 /
    /// 指示 / id」しか送ってこないので、`user_edited` と削除済み集合は
    /// サーバ側 (ここ) で保存前の状態と突き合わせて維持する。フロントに
    /// 持たせると、リロードや実装の取り違えで印が消えた瞬間に、
    /// ユーザーの編集がアプリ更新で上書きされる。
    ///
    /// 規則:
    /// 1. 条件か指示が空の行は落とす (空欄は全発話に効いてしまう)。
    /// 2. 既知の id が消えていれば「ユーザーが削除した」と記録する。
    /// 3. 既知の id で中身が変わっていれば `user_edited` を立てる。
    /// 4. **知らない id は空へ倒す** (= ユーザー作成扱い)。UI が既定の id を
    ///    名乗れると、他人の項目の更新権を横取りできてしまう。
    ///    削除済み集合には**触らない**。
    fn replace_style_profiles(&mut self, incoming: Vec<StyleProfile>) {
        let previous = std::mem::take(&mut self.style_profiles);
        let mut next: Vec<StyleProfile> = Vec::with_capacity(incoming.len());

        for mut profile in incoming {
            profile.process = profile.process.trim().to_string();
            profile.instruction = profile.instruction.trim().to_string();
            profile.title_contains = profile
                .title_contains
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
            if profile.process.is_empty() || profile.instruction.is_empty() {
                continue;
            }
            let id = profile.id.trim().to_string();
            profile.id = id.clone();
            // 同じ id を 2 行が名乗っていたら、2 行目以降は空へ倒す。
            // 重複した id はマージ側で「最初に見つかった 1 行」しか更新
            // されず、残りが更新されない幽霊として残り続ける。UI の正常系
            // では起きないが、防ぐのは 1 行で済む。
            if !id.is_empty() && next.iter().any(|p| p.id == id) {
                profile.id = String::new();
                profile.user_edited = false;
                next.push(profile);
                continue;
            }
            match previous.iter().find(|p| !p.id.is_empty() && p.id == id) {
                Some(stored) => {
                    // 一度でも書き換えられたら印は落とさない。戻しても
                    // 「触った項目」であることに変わりはなく、アプリ側の
                    // 改訂で黙って上書きされない方が驚きが少ない。
                    profile.user_edited = stored.user_edited
                        || stored.process != profile.process
                        || stored.title_contains != profile.title_contains
                        || stored.instruction != profile.instruction;
                }
                None => {
                    // 保存前の一覧に無い id を名乗ってきた行。**id を空へ倒す。**
                    //
                    // 現行 UI では起きない (行を作り直しても id は常に空)。
                    // つまりここに来るのは、フロントの取り違えで行と
                    // dataset.id の対応がずれたときだけ。そのとき id を
                    // 信じると、(i) 実体の無い「既定 (編集済み)」行が生まれ、
                    // (ii) 削除の記録が黙って取り消され、(iii) 同じ id の
                    // 将来の既定配信がその行に塞がれる。どれも無言で起きて
                    // 直しようがないので、名乗りは受け付けない。
                    //
                    // 「消した既定を戻したい」導線は、`style_removed_default_ids`
                    // から外す UI で正面から作るべきもので、**未知 id の受理で
                    // 賄ってはいけない** (副作用が上の 3 つと同じになる)。
                    profile.id = String::new();
                    profile.user_edited = false;
                }
            }
            next.push(profile);
        }

        // 消えた既定を記録する。ユーザー作成 (id 空) は記録しない —
        // 復活させる仕組みが無いので、覚えておく意味が無い。
        for stored in &previous {
            if stored.is_user_made() {
                continue;
            }
            let gone = !next.iter().any(|p| p.id == stored.id);
            if gone && !self.style_removed_default_ids.contains(&stored.id) {
                self.style_removed_default_ids.push(stored.id.clone());
            }
        }

        self.style_profiles = next;
    }

    /// 同梱の既定を差分だけ取り込み、取り込み済みの版を進める。
    ///
    /// 戻り値が「何か変わったか」。呼び出し側 ([`ConfigStore::load`]) は
    /// 変わったときだけ保存する。毎回書くと、起動のたびに設定ファイルの
    /// mtime が動いてバックアップ差分が無意味に膨らむ。
    fn merge_style_defaults(&mut self) -> crate::style::MergeReport {
        let catalog = crate::style::default_profiles();
        let report = crate::style::merge_default_profiles(
            &mut self.style_profiles,
            &mut self.style_removed_default_ids,
            self.style_defaults_version,
            &catalog,
        );
        self.style_defaults_version = crate::style::STYLE_DEFAULTS_VERSION;
        report
    }

    /// パッチを適用する。空文字が来たフィールドは既定値へ戻す。
    pub fn apply(&mut self, patch: ConfigPatch) {
        if let Some(v) = patch.groq_api_key {
            self.groq_api_key = Secret::new(v.trim());
        }
        if let Some(v) = patch.gemini_api_key {
            self.gemini_api_key = Secret::new(v.trim());
        }
        if let Some(v) = patch.language {
            self.language = v.trim().to_string();
        }
        if let Some(v) = patch.dictionary {
            // 掃除 (trim / 空行落とし / 追加日時の補完) は normalize に集める。
            // 設定ファイルは手で編集できるので、UI 経由の道だけを掃除しても
            // 不変条件は守れない (末尾の self.normalize() が両方を通す)。
            self.dictionary = v;
        }
        if let Some(v) = patch.formatting_enabled {
            self.formatting_enabled = v;
        }
        if let Some(v) = patch.injection_enabled {
            self.injection_enabled = v;
        }
        if let Some(v) = patch.deep_context {
            self.deep_context = v;
        }
        if let Some(v) = patch.auto_learn_dictionary {
            self.auto_learn_dictionary = v;
        }
        if let Some(v) = patch.local_stt_mode {
            self.local_stt_mode = v;
        }
        if let Some(v) = patch.local_model_sha256 {
            self.local_model_sha256 = v.trim().to_string();
        }
        if let Some(v) = patch.start_hidden {
            self.start_hidden = v;
        }
        if let Some(v) = patch.autostart {
            self.autostart = v;
        }
        if let Some(v) = patch.hotkey_vk {
            self.hotkey_vk = v;
        }
        if let Some(v) = patch.hotkey_mods {
            self.hotkey_mods = v;
        }
        if let Some(v) = patch.cancel_vk {
            self.cancel_vk = v;
        }
        if let Some(v) = patch.cancel_mods {
            self.cancel_mods = v;
        }
        if let Some(v) = patch.clipboard_hotkey_vk {
            self.clipboard_hotkey_vk = v;
        }
        if let Some(v) = patch.clipboard_hotkey_mods {
            self.clipboard_hotkey_mods = v;
        }
        if let Some(v) = patch.screen_ask_enabled {
            self.screen_ask_enabled = v;
        }
        if let Some(v) = patch.screen_ask_hotkey_vk {
            self.screen_ask_hotkey_vk = v;
        }
        if let Some(v) = patch.screen_ask_hotkey_mods {
            self.screen_ask_hotkey_mods = v;
        }
        if let Some(v) = patch.sound_enabled {
            self.sound_enabled = v;
        }
        if let Some(v) = patch.sound_volume {
            self.sound_volume = v;
        }
        if let Some(v) = patch.start_sound {
            self.start_sound = v;
        }
        if let Some(v) = patch.start_sound_path {
            self.start_sound_path = v.trim().to_string();
        }
        if let Some(v) = patch.cancel_sound {
            self.cancel_sound = v;
        }
        if let Some(v) = patch.cancel_sound_path {
            self.cancel_sound_path = v.trim().to_string();
        }
        if let Some(v) = patch.overlay_enabled {
            self.overlay_enabled = v;
        }
        if let Some(v) = patch.style_profiles {
            self.replace_style_profiles(v);
        }
        if let Some(v) = patch.history_enabled {
            self.history_enabled = v;
        }
        if let Some(v) = patch.history_retention_days {
            self.history_retention_days = v;
        }
        if let Some(v) = patch.restore_delay_ms {
            // 極端な値は事故のもと。0 は即復元 = 旧内容が貼られる、
            // 長すぎるとユーザーの次のコピーを壊す (R3-b)。
            self.restore_delay_ms = v;
        }
        if let Some(v) = patch.keep_transcript_in_clipboard {
            self.keep_transcript_in_clipboard = v;
        }
        if let Some(v) = patch.stt_model {
            self.stt_model = non_empty_or(v, DEFAULT_STT_MODEL);
        }
        if let Some(v) = patch.format_model {
            self.format_model = non_empty_or(v, DEFAULT_FORMAT_MODEL);
        }
        if let Some(v) = patch.groq_endpoint {
            self.groq_endpoint = non_empty_or(v, DEFAULT_GROQ_ENDPOINT);
        }
        if let Some(v) = patch.gemini_endpoint {
            self.gemini_endpoint = non_empty_or(v, DEFAULT_GEMINI_ENDPOINT);
        }
        if let Some(v) = patch.format_fallback_enabled {
            self.format_fallback_enabled = v;
        }
        if let Some(v) = patch.groq_format_model {
            self.groq_format_model = non_empty_or(v, DEFAULT_GROQ_FORMAT_MODEL);
        }
        if let Some(v) = patch.groq_chat_endpoint {
            self.groq_chat_endpoint = non_empty_or(v, DEFAULT_GROQ_CHAT_ENDPOINT);
        }
        if let Some(v) = patch.typing_speed_chars_per_min {
            self.typing_speed_chars_per_min = v;
        }
        if let Some(v) = patch.format_rules {
            // 改行の統一・trim・「既定と同じなら空」は normalize に集める
            // (設定ファイルから読んだ値も同じ形にするため)。
            self.format_rules = v;
        }
        self.normalize();
    }
}

/// 読み込み・適用後のモデル名を揃える。空や不正なら既定へ倒す (警告つき)。
fn normalized_model(provider: FormatProvider, raw: &str) -> String {
    match validate_model_name(provider, raw) {
        Ok(name) if name.is_empty() => provider.default_model().to_string(),
        Ok(name) => name,
        Err(e) => {
            log::warn!("{e} — 既定の {} へ戻します", provider.default_model());
            provider.default_model().to_string()
        }
    }
}

fn non_empty_or(value: String, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// 設定の保持と永続化。
pub struct ConfigStore {
    path: PathBuf,
    config: Mutex<Config>,
    /// 起動時に設定ファイルが存在したか (初回起動の判定に使う)。
    existed: bool,
}

impl ConfigStore {
    /// ファイルから読み込む。壊れていても既定値で起動する (落とさない)。
    pub fn load(path: PathBuf) -> Self {
        let existed = path.exists();
        let config = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Config>(&text) {
                Ok(c) => {
                    log::info!("設定を読み込みました: {}", path.display());
                    c
                }
                Err(e) => {
                    // 中身はキーを含みうるのでログに出さない。原因だけ書く。
                    log::error!("設定ファイルを解釈できません ({}): {e}", path.display());
                    // 既定値で起動すると、次の保存でこのファイルが黙って
                    // 上書きされ、手で直せば救えたはずのキーが消える。
                    // 退避してから既定値へ倒す。
                    backup_broken_config(&path);
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!(
                    "設定ファイルがありません。既定値を使います: {}",
                    path.display()
                );
                Config::default()
            }
            Err(e) => {
                log::error!("設定ファイルを読めません ({}): {e}", path.display());
                Config::default()
            }
        };
        // 手で編集された設定ファイルにも範囲外の値が入りうる。
        // 0 だと貼付が消費される前に復元して旧内容が貼られ (R3-b)、
        // 巨大値だとワーカーがその間ずっと塞がる。読み込み時にも正す。
        let mut config = config;
        config.normalize();

        // 同梱の既定を差分だけ取り込む。**アプリ更新で既定を増やしても、
        // 既存ユーザーの config.json には何も届かない**のがこの仕組みの
        // 動機 (style.rs のモジュール doc)。
        let version_before = config.style_defaults_version;
        let removed_before = config.style_removed_default_ids.len();
        let report = config.merge_style_defaults();
        let changed = !report.is_empty()
            || version_before != crate::style::STYLE_DEFAULTS_VERSION
            || removed_before != config.style_removed_default_ids.len();
        if changed {
            log::info!(
                "既定の文体プロファイルを取り込みました (版 {version_before} → {}): 追加 {} / 更新 {} / 旧形式の引き継ぎ {} 件",
                crate::style::STYLE_DEFAULTS_VERSION,
                report.added.len(),
                report.updated.len(),
                report.adopted,
            );
            // 保存できなくても起動は続ける。次回また同じ差分を取り込む
            // だけで、ユーザーの編集や削除が壊れることはない (冪等)。
            if existed {
                if let Err(e) = save(&path, &config) {
                    log::warn!("既定プロファイルの取り込みを保存できません: {e}");
                }
            }
        }

        Self {
            path,
            config: Mutex::new(config),
            existed,
        }
    }

    /// 初回起動か (設定ファイルが無かったか)。
    pub fn is_first_run(&self) -> bool {
        !self.existed
    }

    /// 現在の設定のコピーを返す。
    pub fn snapshot(&self) -> Config {
        self.config
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// 自動学習で得た語を辞書へ足して保存する。**実際に足した語**を返す。
    ///
    /// # なぜ `update(ConfigPatch { dictionary: ... })` を使わないか
    ///
    /// パッチ経由だと「スナップショットを取る → 語を足す → 全置換で書く」に
    /// なる。学習は裏のスレッドから来るので、その間に設定画面が保存すると
    /// **どちらかの変更が丸ごと消える**。ここはロックを 1 回だけ取って
    /// 読み書きを閉じ込める (読んでから書くまでの隙間を作らない)。
    ///
    /// 追加そのものの判断 (重複・上限・古い語の退避) は
    /// [`crate::dictionary::merge_auto_entries`] に集めてある。
    pub fn add_auto_dictionary_entries(
        &self,
        terms: &[(String, Option<String>)],
    ) -> Result<Vec<DictionaryEntry>, String> {
        let (added, updated) = {
            let mut guard = self
                .config
                .lock()
                .map_err(|_| "設定のロックが毒化しました".to_string())?;
            let now = now_ms();
            let added = crate::dictionary::merge_auto_entries(&mut guard.dictionary, terms, now);
            if added.is_empty() {
                // 1 語も増えないなら書き込みもしない。**保存のたびに
                // ファイルを触ると、設定画面の「保存しました」と競合する。**
                return Ok(Vec::new());
            }
            // 追加日時の補完や trim をここで書き直さない (不変条件は 1 か所)。
            guard.normalize();
            (added, guard.clone())
        };
        save(&self.path, &updated)?;
        Ok(added)
    }

    /// パッチを適用して保存する。
    pub fn update(&self, patch: ConfigPatch) -> Result<Config, String> {
        // 断るなら、ロックを取る前・何も書き換えないうちに断る。
        patch.validate()?;
        let updated = {
            let mut guard = self
                .config
                .lock()
                .map_err(|_| "設定のロックが毒化しました".to_string())?;
            guard.apply(patch);
            guard.clone()
        };
        save(&self.path, &updated)?;
        Ok(updated)
    }
}

/// 壊れた設定ファイルを `.bak` へ退避する。
///
/// 既存の `.bak` は潰さない (最初に壊れたときの原本の方が価値がある)。
fn backup_broken_config(path: &Path) {
    let backup = path.with_extension("json.bak");
    if backup.exists() {
        log::warn!(
            "壊れた設定を退避できません (退避先が既にあります): {}",
            backup.display()
        );
        return;
    }
    match fs::rename(path, &backup) {
        Ok(()) => log::warn!(
            "壊れた設定を {} へ退避しました。API キーはこのファイルから回収できます",
            backup.display()
        ),
        Err(e) => log::error!("壊れた設定を退避できません ({}): {e}", backup.display()),
    }
}

/// アトミックに書き出す (書きかけのファイルを残さない)。
fn save(path: &Path, config: &Config) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "設定ディレクトリを作成できません ({}): {e}",
                parent.display()
            )
        })?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("設定をシリアライズできません: {e}"))?;

    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json.as_bytes())
        .map_err(|e| format!("設定を書き出せません ({}): {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        // 失敗時に中途半端な tmp を残さない。
        let _ = fs::remove_file(&tmp);
        format!("設定を保存できません ({}): {e}", path.display())
    })?;
    log::info!("設定を保存しました: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_shows_its_value() {
        let s = Secret::new("gsk_supersecret_value");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
        // 構造体ごと {:?} しても漏れない。
        let cfg = Config {
            groq_api_key: s.clone(),
            gemini_api_key: Secret::new("AIzaSy_secret"),
            ..Config::default()
        };
        let dumped = format!("{cfg:?}");
        assert!(!dumped.contains("supersecret"), "{dumped}");
        assert!(!dumped.contains("AIzaSy"), "{dumped}");
    }

    #[test]
    fn empty_secret_is_distinguishable() {
        assert_eq!(format!("{:?}", Secret::default()), "Secret(<empty>)");
        assert!(Secret::new("   ").is_empty());
    }

    #[test]
    fn preview_shows_only_the_tail() {
        assert_eq!(Secret::new("gsk_abcdefgh1234").preview(), "…1234");
        assert_eq!(Secret::new("").preview(), "");
        // 4 文字未満でも panic しない。
        assert_eq!(Secret::new("ab").preview(), "…ab");
    }

    #[test]
    fn the_clipboard_hotkey_is_unset_by_default() {
        let cfg = Config::default();
        assert_eq!(cfg.clipboard_hotkey_vk, 0);
        assert!(cfg.clipboard_hotkey_combo().is_none());
    }

    /// 同じ組み合わせを 2 用途に割り当てると、どちらが動いたか分からなくなる。
    #[test]
    fn a_duplicate_clipboard_hotkey_is_disabled_not_preferred() {
        let mut cfg = Config::default();
        cfg.clipboard_hotkey_vk = cfg.hotkey_vk;
        cfg.clipboard_hotkey_mods = cfg.hotkey_mods.clone();
        cfg.normalize();
        assert_eq!(cfg.clipboard_hotkey_vk, 0, "重複が残っている");
        assert!(cfg.clipboard_hotkey_mods.is_empty());
        // 貼り付け側は無傷。
        assert_eq!(cfg.hotkey_vk, Config::default().hotkey_vk);
    }

    #[test]
    fn an_unusable_clipboard_hotkey_is_disabled_not_defaulted() {
        // 文字キー単独は押している間ずっと入力先へ流れるので選べない。
        let mut cfg = Config {
            clipboard_hotkey_vk: 0x41, // A
            clipboard_hotkey_mods: Vec::new(),
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.clipboard_hotkey_vk, 0, "既定へ倒れている");
    }

    #[test]
    fn dangling_modifiers_are_dropped_when_the_trigger_is_unset() {
        let mut cfg = Config {
            clipboard_hotkey_vk: 0,
            clipboard_hotkey_mods: vec![0xA2],
            ..Config::default()
        };
        cfg.normalize();
        assert!(cfg.clipboard_hotkey_mods.is_empty());
    }

    /// キャンセルキーはどちらのホットキーとも重複してはいけない。
    #[test]
    fn the_cancel_key_may_not_collide_with_the_clipboard_hotkey() {
        let mut cfg = Config {
            clipboard_hotkey_vk: 0x7C, // F13
            cancel_vk: 0x7C,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.cancel_vk, crate::hotkey::DEFAULT_CANCEL_VK);
        assert_eq!(cfg.clipboard_hotkey_vk, 0x7C, "先に設定した方を残す");
    }

    // --- 画面質問モード ---

    #[test]
    fn screen_ask_is_off_and_unbound_by_default() {
        // 画面をまるごとクラウドへ送る機能なので、既定は二重に閉じている。
        let cfg = Config::default();
        assert!(!cfg.screen_ask_enabled);
        assert_eq!(cfg.screen_ask_hotkey_vk, 0);
        assert!(cfg.screen_ask_hotkey_combo().is_none());
    }

    #[test]
    fn a_bound_key_does_nothing_while_the_feature_is_off() {
        // 「オフなのに押すと画面が送られる」を作らない。
        let mut cfg = Config {
            screen_ask_enabled: false,
            screen_ask_hotkey_vk: 0x7C, // F13
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.screen_ask_hotkey_vk, 0x7C, "キーの割り当ては保たれる");
        assert!(
            cfg.screen_ask_hotkey_combo().is_none(),
            "無効なのにフックへ渡る組み合わせが返っている"
        );

        cfg.screen_ask_enabled = true;
        assert!(cfg.screen_ask_hotkey_combo().is_some(), "有効化しても効かない");
    }

    #[test]
    fn enabling_the_feature_without_a_key_still_does_nothing() {
        // トグルだけでは動かない。既存の録音キーに相乗りさせない。
        let cfg = Config {
            screen_ask_enabled: true,
            screen_ask_hotkey_vk: 0,
            ..Config::default()
        };
        assert!(cfg.screen_ask_hotkey_combo().is_none());
    }

    #[test]
    fn a_screen_ask_key_that_duplicates_another_mode_is_disabled() {
        // 取り違えの代償が「画面が送られる」なので、後勝ちにしない。
        let mut cfg = Config {
            screen_ask_enabled: true,
            ..Config::default()
        };
        cfg.screen_ask_hotkey_vk = cfg.hotkey_vk;
        cfg.screen_ask_hotkey_mods = cfg.hotkey_mods.clone();
        cfg.normalize();
        assert_eq!(cfg.screen_ask_hotkey_vk, 0, "録音キーとの重複が残っている");
        assert!(cfg.screen_ask_hotkey_mods.is_empty());

        let mut cfg = Config {
            screen_ask_enabled: true,
            clipboard_hotkey_vk: 0x7C, // F13
            screen_ask_hotkey_vk: 0x7C,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.clipboard_hotkey_vk, 0x7C, "先に設定した方を残す");
        assert_eq!(cfg.screen_ask_hotkey_vk, 0, "クリップボードとの重複が残っている");
    }

    #[test]
    fn an_unusable_screen_ask_key_is_disabled_not_defaulted() {
        // 既定へ倒すと、設定していないのに録音キーが画面質問にも割り当たる。
        let mut cfg = Config {
            screen_ask_enabled: true,
            screen_ask_hotkey_vk: 0x41, // A
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.screen_ask_hotkey_vk, 0);
        assert!(cfg.screen_ask_hotkey_combo().is_none());
    }

    #[test]
    fn a_screen_ask_key_of_zero_drops_its_leftover_modifiers() {
        let mut cfg = Config {
            screen_ask_hotkey_vk: 0,
            screen_ask_hotkey_mods: vec![0xA2],
            ..Config::default()
        };
        cfg.normalize();
        assert!(cfg.screen_ask_hotkey_mods.is_empty(), "UI の表示が嘘になる");
    }

    #[test]
    fn the_cancel_key_may_not_collide_with_the_screen_ask_hotkey() {
        let mut cfg = Config {
            screen_ask_enabled: true,
            screen_ask_hotkey_vk: 0x7C, // F13
            cancel_vk: 0x7C,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.cancel_vk, crate::hotkey::DEFAULT_CANCEL_VK);
        assert_eq!(cfg.screen_ask_hotkey_vk, 0x7C, "先に設定した方を残す");
    }

    #[test]
    fn the_view_shows_the_bound_key_even_while_the_feature_is_off() {
        // 有効化した瞬間に何のキーで動くのか分からないと、
        // トグルを押すのが怖い機能になる。
        let cfg = Config {
            screen_ask_enabled: false,
            screen_ask_hotkey_vk: 0x7C, // F13
            ..Config::default()
        };
        // 環境変数に依存させないため、解決済みキーを明示して組み立てる。
        let view = ConfigView::build(
            &cfg,
            resolve_key(None, &cfg.groq_api_key),
            resolve_key(None, &cfg.gemini_api_key),
        );
        assert!(!view.screen_ask_enabled);
        assert_eq!(view.screen_ask_hotkey_vk, 0x7C);
        assert!(
            !view.screen_ask_hotkey_label.is_empty(),
            "割り当て済みのキーが表示されない"
        );
    }

    #[test]
    fn a_screen_ask_patch_survives_a_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("nox-screen-ask-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                screen_ask_enabled: Some(true),
                screen_ask_hotkey_vk: Some(0x7C),
                screen_ask_hotkey_mods: Some(vec![0xA2]),
                ..Default::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path).snapshot();
        assert!(reloaded.screen_ask_enabled);
        assert_eq!(reloaded.screen_ask_hotkey_vk, 0x7C);
        assert_eq!(reloaded.screen_ask_hotkey_mods, vec![0xA2]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_config_file_without_screen_ask_stays_off() {
        // 旧い設定ファイルにこの項目は無い。フィールド単位の
        // `#[serde(default)]` が効いていないと、既定値の解釈がずれる
        // (hotkey_mods で踏んだのと同じ罠)。
        let json = r#"{
            "groq_api_key": "",
            "gemini_api_key": "",
            "language": "ja",
            "dictionary": [],
            "formatting_enabled": true,
            "injection_enabled": true,
            "local_stt_mode": "fallback",
            "local_model_sha256": "",
            "start_hidden": true,
            "hotkey_vk": 32,
            "cancel_vk": 27,
            "overlay_enabled": true,
            "deep_context": false,
            "style_profiles": [],
            "history_enabled": true,
            "history_retention_days": 30,
            "restore_delay_ms": 120,
            "keep_transcript_in_clipboard": true,
            "typing_speed_chars_per_min": 300,
            "groq_endpoint": "https://example.invalid",
            "gemini_endpoint": "https://example.invalid",
            "stt_model": "m",
            "format_model": "m"
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("旧形式を読める");
        assert!(!cfg.screen_ask_enabled);
        assert_eq!(cfg.screen_ask_hotkey_vk, 0);
        assert!(cfg.screen_ask_hotkey_mods.is_empty());
    }

    #[test]
    fn sound_settings_are_clamped_and_trimmed() {
        let mut cfg = Config {
            sound_volume: 250,
            start_sound: SoundPreset::Custom,
            start_sound_path: "  ".to_string(),
            cancel_sound: SoundPreset::Custom,
            cancel_sound_path: "  C:/beep.wav  ".to_string(),
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.sound_volume, 100);
        // パス未指定のカスタム音は鳴らしようがないので無音へ倒す。
        assert_eq!(cfg.start_sound, SoundPreset::Silent);
        // 指定があるものは残す (前後の空白だけ落とす)。
        assert_eq!(cfg.cancel_sound, SoundPreset::Custom);
        assert_eq!(cfg.cancel_sound_path, "C:/beep.wav");
    }

    #[test]
    fn disabling_sound_silences_every_event() {
        let cfg = Config {
            sound_enabled: false,
            sound_volume: 80,
            ..Config::default()
        };
        assert_eq!(cfg.effective_sound_volume(), 0);
    }

    /// 旧い設定ファイル (音の項目が無い) を読んでも通知音は有効になる。
    #[test]
    fn old_config_files_get_sound_enabled() {
        let json = r#"{
            "groq_api_key": "",
            "gemini_api_key": "",
            "language": "ja",
            "dictionary": [],
            "formatting_enabled": true,
            "injection_enabled": true,
            "local_stt_mode": "fallback",
            "local_model_sha256": "",
            "start_hidden": true,
            "hotkey_vk": 32,
            "cancel_vk": 27,
            "overlay_enabled": true,
            "deep_context": false,
            "style_profiles": [],
            "history_enabled": true,
            "history_retention_days": 30,
            "restore_delay_ms": 180,
            "keep_transcript_in_clipboard": true,
            "typing_speed_chars_per_min": 60,
            "groq_endpoint": "",
            "gemini_endpoint": "",
            "stt_model": "",
            "format_model": ""
        }"#;
        let cfg: Config = serde_json::from_str(json).expect("旧形式を読める");
        assert!(cfg.sound_enabled, "旧設定で通知音が無効になっている");
        assert_eq!(cfg.sound_volume, crate::sound::DEFAULT_VOLUME);
        assert_eq!(cfg.start_sound, SoundPreset::SoftPop);
        assert_eq!(cfg.cancel_sound, SoundPreset::Fall);
        // 旧設定にクリップボード用ホットキーは無いので未設定のまま。
        assert_eq!(cfg.clipboard_hotkey_vk, 0);
    }

    #[test]
    fn env_var_takes_precedence_over_config() {
        let configured = Secret::new("from-config");
        let resolved = resolve_key(Some("from-env".to_string()), &configured);
        assert_eq!(resolved.source, KeySource::Env);
        assert_eq!(resolved.secret.expect("鍵がある").expose(), "from-env");
    }

    #[test]
    fn config_key_is_used_when_env_absent() {
        let resolved = resolve_key(None, &Secret::new("from-config"));
        assert_eq!(resolved.source, KeySource::Config);
        assert_eq!(resolved.secret.expect("鍵がある").expose(), "from-config");
    }

    #[test]
    fn blank_env_var_does_not_shadow_config() {
        // env_value 側で空白は弾かれる想定なので None が渡る。
        let resolved = resolve_key(None, &Secret::new("from-config"));
        assert_eq!(resolved.source, KeySource::Config);
    }

    #[test]
    fn dotenv_takes_precedence_over_env_var_and_config() {
        // 他のアプリと共有している環境変数より、nox-voice 専用の .env が勝つ。
        let resolved = resolve_key_layers(
            Some("from-dotenv".to_string()),
            Some("from-env".to_string()),
            &Secret::new("from-config"),
        );
        assert_eq!(resolved.source, KeySource::DotEnv);
        assert_eq!(resolved.secret.expect("鍵がある").expose(), "from-dotenv");
    }

    #[test]
    fn missing_dotenv_falls_back_to_env_then_config() {
        let resolved = resolve_key_layers(
            None,
            Some("from-env".to_string()),
            &Secret::new("from-config"),
        );
        assert_eq!(resolved.source, KeySource::Env);
        let resolved = resolve_key_layers(None, None, &Secret::new("from-config"));
        assert_eq!(resolved.source, KeySource::Config);
    }

    #[test]
    fn dotenv_parses_plain_quoted_exported_and_commented_lines() {
        let text = "# nox-voice 専用\r\n\
                    GROQ_API_KEY=gsk_plain\r\n\
                    export GEMINI_API_KEY = \"AIza quoted\" \r\n\
                    OTHER=1 # 無関係\r\n";
        assert_eq!(
            parse_dotenv_value(text, "GROQ_API_KEY").as_deref(),
            Some("gsk_plain")
        );
        assert_eq!(
            parse_dotenv_value(text, "GEMINI_API_KEY").as_deref(),
            Some("AIza quoted")
        );
        assert_eq!(parse_dotenv_value("K='single'", "K").as_deref(), Some("single"));
        assert_eq!(parse_dotenv_value("K=abc # 注釈", "K").as_deref(), Some("abc"));
        assert_eq!(parse_dotenv_value("K=a#b", "K").as_deref(), Some("a#b"));
        assert_eq!(parse_dotenv_value("#K=commented", "K"), None);
        assert_eq!(parse_dotenv_value("NOT_K=x", "K"), None);
    }

    #[test]
    fn dotenv_comment_only_value_is_blank_and_quoted_value_drops_trailing_comment() {
        // 注釈だけの値が「キー」として最上位の層に居座らないこと (レビュー指摘)。
        assert_eq!(parse_dotenv_value("GROQ_API_KEY= # 後で入れる", "GROQ_API_KEY"), None);
        assert_eq!(parse_dotenv_value("K=\t# tab", "K"), None);
        assert_eq!(parse_dotenv_value("K=abc\t# tab", "K").as_deref(), Some("abc"));
        assert_eq!(parse_dotenv_value("K=#abc", "K").as_deref(), Some("#abc"));
        assert_eq!(parse_dotenv_value("K=\"abc\" # c", "K").as_deref(), Some("abc"));
        assert_eq!(parse_dotenv_value("K='a #b' #c", "K").as_deref(), Some("a #b"));
        assert_eq!(parse_dotenv_value("K=a=b==", "K").as_deref(), Some("a=b=="));
        // 閉じない引用符はそのまま値 (panic しない)。
        assert_eq!(parse_dotenv_value("K=\"", "K").as_deref(), Some("\""));
        assert_eq!(parse_dotenv_value("K=\"あ", "K").as_deref(), Some("\"あ"));
        // `export` の後はタブでもよい。`exportK` は別の名前。
        assert_eq!(parse_dotenv_value("export\tK=v", "K").as_deref(), Some("v"));
        assert_eq!(parse_dotenv_value("exportK=v", "K"), None);
    }

    #[test]
    fn dotenv_later_line_wins_and_blank_value_is_absent() {
        assert_eq!(parse_dotenv_value("K=old\nK=new", "K").as_deref(), Some("new"));
        // 空の値は「書いていない」扱い。環境変数へ素通しする。
        assert_eq!(parse_dotenv_value("K=", "K"), None);
        assert_eq!(parse_dotenv_value("K=\"\"", "K"), None);
        assert_eq!(parse_dotenv_value("K=   ", "K"), None);
    }

    #[test]
    fn dotenv_decodes_utf8_bom_and_utf16le() {
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend_from_slice(b"GROQ_API_KEY=gsk_bom\r\n");
        assert_eq!(
            parse_dotenv_value(&decode_dotenv(&utf8), "GROQ_API_KEY").as_deref(),
            Some("gsk_bom")
        );
        // Windows PowerShell 5.1 の `"..." > .env` は UTF-16 LE (BOM 付き) で書く。
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "GROQ_API_KEY=gsk_utf16\r\n".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            parse_dotenv_value(&decode_dotenv(&utf16), "GROQ_API_KEY").as_deref(),
            Some("gsk_utf16")
        );
    }

    #[test]
    fn dotenv_source_serializes_as_dotenv() {
        let json = serde_json::to_string(&KeySource::DotEnv).expect("シリアライズできる");
        assert_eq!(json, "\"dotenv\"");
    }

    #[test]
    fn missing_key_everywhere_is_none() {
        let resolved = resolve_key(None, &Secret::default());
        assert_eq!(resolved.source, KeySource::None);
        assert!(!resolved.is_set());
    }

    #[test]
    fn config_view_omits_secrets_entirely() {
        let cfg = Config {
            groq_api_key: Secret::new("gsk_abcdefgh1234"),
            gemini_api_key: Secret::default(),
            ..Config::default()
        };
        // 環境変数に依存させないため、解決済みキーを明示して組み立てる。
        let view = ConfigView::build(
            &cfg,
            resolve_key(None, &cfg.groq_api_key),
            resolve_key(None, &cfg.gemini_api_key),
        );
        let json = serde_json::to_string(&view).expect("シリアライズできる");
        assert!(!json.contains("gsk_abcdefgh"), "{json}");
        assert!(json.contains("…1234"), "伏字プレビューが無い: {json}");
        assert!(view.groq_key_set);
        assert!(!view.gemini_key_set);
        assert_eq!(view.gemini_key_source, KeySource::None);
    }

    #[test]
    fn config_view_reports_env_sourced_keys() {
        let cfg = Config::default();
        let view = ConfigView::build(
            &cfg,
            resolve_key(Some("gsk_from_env_9876".to_string()), &cfg.groq_api_key),
            resolve_key(None, &cfg.gemini_api_key),
        );
        assert!(view.groq_key_set);
        assert_eq!(view.groq_key_source, KeySource::Env);
        let json = serde_json::to_string(&view).expect("シリアライズできる");
        assert!(!json.contains("gsk_from_env"), "{json}");
    }

    #[test]
    fn patch_only_touches_specified_fields() {
        let mut cfg = Config {
            language: "ja".to_string(),
            formatting_enabled: true,
            ..Config::default()
        };
        cfg.apply(ConfigPatch {
            formatting_enabled: Some(false),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.language, "ja", "指定していない項目が変わった");
        assert!(!cfg.formatting_enabled);
    }

    #[test]
    fn patch_with_empty_model_restores_default() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            stt_model: Some("  ".to_string()),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.stt_model, DEFAULT_STT_MODEL);
    }

    #[test]
    fn patch_drops_blank_dictionary_entries() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            dictionary: Some(vec![
                DictionaryEntry::new(" nox-voice "),
                DictionaryEntry::new(""),
                DictionaryEntry::new("  "),
                DictionaryEntry::new("Tauri"),
            ]),
            ..ConfigPatch::default()
        });
        let written: Vec<&str> = cfg.dictionary.iter().map(|e| e.written.as_str()).collect();
        assert_eq!(written, vec!["nox-voice", "Tauri"]);
    }

    #[test]
    fn saved_entries_get_an_added_at_timestamp() {
        // 0 のままだと選抜で「不明」として横並びになり、後から足した語
        // だけが常に勝ち続ける。保存の瞬間に埋める。
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            dictionary: Some(vec![DictionaryEntry::new("nox-voice")]),
            ..ConfigPatch::default()
        });
        assert!(cfg.dictionary[0].added_at_ms > 0);
        // 既に日時を持つ語は書き換えない (登録の古さが消えてしまう)。
        let stamped = DictionaryEntry {
            added_at_ms: 1_700_000_000_000,
            ..DictionaryEntry::new("Tauri")
        };
        cfg.apply(ConfigPatch {
            dictionary: Some(vec![stamped.clone()]),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.dictionary[0].added_at_ms, 1_700_000_000_000);
    }

    #[test]
    fn an_old_string_dictionary_migrates_on_load() {
        // 旧 config.json は `"dictionary": ["nox-voice", "塩谷,しおや"]`。
        // **移行で読みが失われないこと**が要点 (読みは整形側の指示になる)。
        let json = r#"{"dictionary":["nox-voice","塩谷,しおや","  "]}"#;
        let mut cfg: Config = serde_json::from_str(json).expect("読める");
        cfg.normalize();
        assert_eq!(cfg.dictionary.len(), 2, "空行が残った: {:?}", cfg.dictionary);
        assert_eq!(cfg.dictionary[0].written, "nox-voice");
        assert_eq!(cfg.dictionary[1].written, "塩谷");
        assert_eq!(cfg.dictionary[1].reading.as_deref(), Some("しおや"));
        // 移行分は手動・未★で始まり、日時は読み込み時に埋まる。
        assert!(cfg.dictionary.iter().all(|e| !e.pinned));
        assert!(cfg.dictionary.iter().all(|e| e.added_at_ms > 0));
        // 移行しても他の設定は既定のまま (コンテナ default の罠を踏まない)。
        assert_eq!(cfg.language, DEFAULT_LANGUAGE);
    }

    #[test]
    fn a_migrated_dictionary_survives_a_save_and_reload() {
        // 移行 → 保存 → 再読込で形が安定すること。ここが崩れると、
        // 起動のたびに読みや★が落ちる。
        let mut cfg: Config = serde_json::from_str(r#"{"dictionary":["塩谷,しおや"]}"#)
            .expect("読める");
        cfg.normalize();
        cfg.dictionary[0].pinned = true;
        let saved = serde_json::to_string(&cfg).expect("書ける");
        let mut back: Config = serde_json::from_str(&saved).expect("読める");
        back.normalize();
        assert_eq!(back.dictionary, cfg.dictionary);
    }

    #[test]
    fn a_patch_of_old_string_lines_is_still_accepted() {
        // フロントが旧形式を送ってきても (古い WebView が残っている等)
        // 弾かない。serde の入口 1 か所で吸収する設計の確認。
        let patch: ConfigPatch =
            serde_json::from_str(r#"{"dictionary":["nox-voice","塩谷,しおや"]}"#).expect("読める");
        let mut cfg = Config::default();
        cfg.apply(patch);
        assert_eq!(cfg.dictionary.len(), 2);
        assert_eq!(cfg.dictionary[1].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn the_view_reports_which_terms_reach_whisper() {
        // 「登録したのに効かない」を画面で説明できること。
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            dictionary: Some(
                (0..60)
                    .map(|i| DictionaryEntry::new(format!("用語{i:03}")))
                    .collect(),
            ),
            ..ConfigPatch::default()
        });
        // 環境変数に依存させないため、解決済みキーを明示して組み立てる。
        let view = ConfigView::build(
            &cfg,
            resolve_key(None, &cfg.groq_api_key),
            resolve_key(None, &cfg.gemini_api_key),
        );
        assert_eq!(view.dictionary.len(), 60);
        assert_eq!(view.dictionary_status.in_prompt.len(), 60);
        assert!(view.dictionary_status.used_terms < 60, "溢れが出ていない");
        assert!(view.dictionary_status.used_chars <= view.dictionary_status.max_chars);
        assert_eq!(
            view.dictionary_status.max_terms,
            crate::dictionary::DICTIONARY_MAX_TERMS
        );
    }

    #[test]
    fn the_model_hash_starts_unpinned_and_can_be_recorded() {
        // TOFU: 初回に取得したものを正とし、以後はそれと照合する。
        let mut cfg = Config::default();
        assert!(cfg.local_model_sha256.is_empty(), "既定で固定されている");
        cfg.apply(ConfigPatch {
            local_model_sha256: Some("  ABC123  ".to_string()),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.local_model_sha256, "ABC123", "前後の空白が残っている");
    }

    #[test]
    fn local_stt_defaults_to_fallback() {
        let cfg = Config::default();
        assert_eq!(cfg.local_stt_mode, LocalSttMode::Fallback);
        assert!(cfg.local_stt_mode.uses_cloud());
        assert!(cfg.local_stt_mode.allows_local());
    }

    #[test]
    fn local_only_mode_never_uses_the_cloud() {
        // 音声を外へ出したくない人向け。R1 の観点で意味がある。
        assert!(!LocalSttMode::Only.uses_cloud());
        assert!(LocalSttMode::Only.allows_local());
    }

    #[test]
    fn local_off_mode_never_uses_local() {
        assert!(LocalSttMode::Off.uses_cloud());
        assert!(!LocalSttMode::Off.allows_local());
    }

    #[test]
    fn auto_learning_is_on_by_default_unlike_deep_context() {
        // 対になる 2 つ。**違いはクラウドへ送るかどうか**で、
        // 自動学習は送らないので既定で効かせる (design.md 2026-08-30)。
        let cfg = Config::default();
        assert!(cfg.auto_learn_dictionary);
        assert!(!cfg.deep_context);
    }

    #[test]
    fn an_older_config_file_gets_auto_learning_enabled() {
        // 項目が無い設定ファイル (この機能より前に書かれたもの) は
        // コンテナ既定を拾う。ここが false に化けると、既存ユーザーだけ
        // 永久に機能が届かない。
        let cfg: Config = serde_json::from_str(r#"{"language":"ja"}"#).expect("読める");
        assert!(cfg.auto_learn_dictionary);
    }

    #[test]
    fn the_format_fallback_is_on_by_default_with_the_measured_model() {
        let cfg = Config::default();
        assert!(cfg.format_fallback_enabled, "控えが既定で無効になっている");
        assert_eq!(cfg.groq_format_model, "openai/gpt-oss-20b");
        assert_eq!(
            cfg.groq_chat_endpoint,
            "https://api.groq.com/openai/v1/chat/completions"
        );
        // 転写のエンドポイントから導出していないこと (あちらは完全な URL)。
        assert_ne!(cfg.groq_chat_endpoint, cfg.groq_endpoint);
    }

    #[test]
    fn an_older_config_file_gets_the_format_fallback() {
        // 項目が無い設定ファイル (この機能より前に書かれたもの) でも
        // 既定が届くこと。ここが false / 空に化けると、既存ユーザーだけ
        // 2026-09-05 と同じ全件生転写に戻る。
        let cfg: Config = serde_json::from_str(r#"{"language":"ja"}"#).expect("読める");
        assert!(cfg.format_fallback_enabled);
        assert_eq!(cfg.groq_format_model, DEFAULT_GROQ_FORMAT_MODEL);
        assert_eq!(cfg.groq_chat_endpoint, DEFAULT_GROQ_CHAT_ENDPOINT);
    }

    #[test]
    fn the_fallback_model_and_endpoint_fall_back_to_defaults_when_blanked() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            format_fallback_enabled: Some(false),
            groq_format_model: Some("  ".to_string()),
            groq_chat_endpoint: Some(String::new()),
            ..ConfigPatch::default()
        });
        assert!(!cfg.format_fallback_enabled);
        assert_eq!(cfg.groq_format_model, DEFAULT_GROQ_FORMAT_MODEL);
        assert_eq!(cfg.groq_chat_endpoint, DEFAULT_GROQ_CHAT_ENDPOINT);
    }

    // --- 整形の共通ルール (2026-10-10) ---

    #[test]
    fn an_older_config_file_uses_the_default_format_rules() {
        // 項目が無い設定ファイル (この機能より前に書かれたもの) は空 = 既定。
        // 既定の全文で埋めてしまうと、アプリ更新で既定を直しても届かない。
        let cfg: Config = serde_json::from_str(r#"{"language":"ja"}"#).expect("読める");
        assert_eq!(cfg.format_rules, "");
        assert_eq!(cfg.format_rules_for_prompt(), None);
        assert_eq!(Config::default().format_rules, "");
    }

    #[test]
    fn saving_the_default_rules_stores_empty() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            format_rules: Some(crate::format::DEFAULT_FORMAT_RULES.to_string()),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.format_rules, "", "既定の全文が焼き込まれた");
        // 前後の空白が付いていても既定と同じ。
        cfg.apply(ConfigPatch {
            format_rules: Some(format!("\n  {}  \n\n", crate::format::DEFAULT_FORMAT_RULES)),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.format_rules, "");
    }

    #[test]
    fn a_crlf_copy_of_the_default_rules_stores_empty() {
        // textarea の値やエディタ経由の貼り付けは CRLF になりうる。
        let crlf = crate::format::DEFAULT_FORMAT_RULES.replace('\n', "\r\n");
        assert_ne!(crlf, crate::format::DEFAULT_FORMAT_RULES);
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            format_rules: Some(crlf),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.format_rules, "");
    }

    #[test]
    fn whitespace_only_rules_store_empty() {
        let mut cfg = Config {
            format_rules: "自作のルール".to_string(),
            ..Config::default()
        };
        cfg.apply(ConfigPatch {
            format_rules: Some(" \r\n\t ".to_string()),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.format_rules, "");
    }

    #[test]
    fn custom_rules_survive_a_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("nox-format-rules-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                format_rules: Some("  句読点だけ打つ。\r\n言い換えない。\r\n".to_string()),
                ..Default::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path).snapshot();
        // 改行は \n に揃い、前後の空白は落ちる。中身はそのまま。
        assert_eq!(reloaded.format_rules, "句読点だけ打つ。\n言い換えない。");
        assert_eq!(
            reloaded.format_rules_for_prompt(),
            Some("句読点だけ打つ。\n言い換えない。")
        );
        let view = ConfigView::build(
            &reloaded,
            resolve_key(None, &reloaded.groq_api_key),
            resolve_key(None, &reloaded.gemini_api_key),
        );
        assert_eq!(view.format_rules, "句読点だけ打つ。\n言い換えない。");
        assert_eq!(view.default_format_rules, crate::format::DEFAULT_FORMAT_RULES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlong_rules_are_rejected_without_touching_the_config() {
        let dir = std::env::temp_dir().join(format!("nox-format-rules-long-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let store = ConfigStore::load(path.clone());
        let long = "あ".repeat(MAX_FORMAT_RULES_CHARS + 1);
        let err = store
            .update(ConfigPatch {
                format_rules: Some(long),
                // 同じパッチの他の項目も書き換わらないこと (半端な保存をしない)。
                language: Some("en".to_string()),
                ..Default::default()
            })
            .expect_err("上限超えは断る");
        assert!(err.contains("整形の共通ルール"), "項目を名指ししていない: {err}");
        assert!(err.contains(&MAX_FORMAT_RULES_CHARS.to_string()), "{err}");
        assert_eq!(store.snapshot().language, DEFAULT_LANGUAGE);
        assert_eq!(store.snapshot().format_rules, "");
        // ちょうど上限は通る。
        assert!(store
            .update(ConfigPatch {
                format_rules: Some("い".repeat(MAX_FORMAT_RULES_CHARS)),
                ..Default::default()
            })
            .is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    // --- 整形モデルの名前 (2026-10-10) ---

    #[test]
    fn a_pasted_models_prefix_is_stripped_from_the_gemini_model() {
        // ListModels は `models/gemini-...` で返す。そのまま貼られても通す。
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            format_model: Some("  models/gemini-3.5-flash-lite ".to_string()),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.format_model, "gemini-3.5-flash-lite");
        // `models/` だけなら空欄と同じ = 既定。
        cfg.apply(ConfigPatch {
            format_model: Some("models/".to_string()),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.format_model, DEFAULT_FORMAT_MODEL);
    }

    #[test]
    fn url_breaking_gemini_model_names_are_rejected() {
        for bad in [
            "gemini/../evil",
            "gemini?key=x",
            "gemini#frag",
            "gemini%2Fx",
            "gemini flash",
            "gemini\tflash",
            "gemini-フラッシュ",
            "gemini:streamGenerateContent",
        ] {
            let err = validate_model_name(FormatProvider::Gemini, bad)
                .expect_err(&format!("{bad:?} を通した"));
            assert!(err.contains("主の整形モデル"), "項目を名指ししていない: {err}");
        }
        for good in [
            "gemini-flash-lite-latest",
            "gemini-3.5-flash-lite",
            "gemini-1.5-pro_002",
            "",
        ] {
            assert!(validate_model_name(FormatProvider::Gemini, good).is_ok(), "{good}");
        }
    }

    #[test]
    fn groq_model_names_may_contain_slashes_but_not_whitespace() {
        for good in ["openai/gpt-oss-20b", "qwen/qwen3.8-27b", "meta-llama/llama-4:free"] {
            assert_eq!(
                validate_model_name(FormatProvider::Groq, good).as_deref(),
                Ok(good)
            );
        }
        // Groq では `models/` を剥がさない (Gemini 固有の書式なので)。
        assert_eq!(
            validate_model_name(FormatProvider::Groq, "models/x").as_deref(),
            Ok("models/x")
        );
        for bad in ["openai/gpt oss", "openai/gpt-oss\n20b", "a\u{7}b"] {
            let err = validate_model_name(FormatProvider::Groq, bad)
                .expect_err(&format!("{bad:?} を通した"));
            assert!(err.contains("控えの整形モデル"), "{err}");
        }
    }

    #[test]
    fn a_bad_model_name_is_rejected_on_save_without_touching_the_config() {
        let dir = std::env::temp_dir().join(format!("nox-format-model-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        let store = ConfigStore::load(path.clone());
        let err = store
            .update(ConfigPatch {
                format_model: Some("gemini?x=1".to_string()),
                groq_format_model: Some("qwen/qwen3.8-27b".to_string()),
                ..Default::default()
            })
            .expect_err("URL を壊す名前は断る");
        assert!(err.contains("「?」"), "どの文字が駄目か書いていない: {err}");
        assert_eq!(store.snapshot().format_model, DEFAULT_FORMAT_MODEL);
        assert_eq!(store.snapshot().groq_format_model, DEFAULT_GROQ_FORMAT_MODEL);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hand_edited_bad_model_name_falls_back_to_the_default_on_load() {
        // 保存の経路は断るので、ここへ来るのは設定ファイルを手で書いたときだけ。
        let mut cfg: Config = serde_json::from_str(
            r#"{"format_model":"gemini/../x?y","groq_format_model":"a b"}"#,
        )
        .expect("読める");
        cfg.normalize();
        assert_eq!(cfg.format_model, DEFAULT_FORMAT_MODEL);
        assert_eq!(cfg.groq_format_model, DEFAULT_GROQ_FORMAT_MODEL);
    }

    #[test]
    fn the_candidates_lead_with_the_defaults_and_pass_validation() {
        // 画面の候補は実測の表から選んだもの。先頭は既定で、
        // どれも保存で断られない名前であること (断られる候補は罠になる)。
        assert_eq!(GEMINI_FORMAT_MODEL_CANDIDATES[0].name, DEFAULT_FORMAT_MODEL);
        assert_eq!(GROQ_FORMAT_MODEL_CANDIDATES[0].name, DEFAULT_GROQ_FORMAT_MODEL);
        for (provider, list) in [
            (FormatProvider::Gemini, GEMINI_FORMAT_MODEL_CANDIDATES),
            (FormatProvider::Groq, GROQ_FORMAT_MODEL_CANDIDATES),
        ] {
            for c in list {
                assert_eq!(
                    validate_model_name(provider, c.name).as_deref(),
                    Ok(c.name),
                    "{provider:?}"
                );
                assert!(!c.note.is_empty());
            }
        }
        let cfg = Config::default();
        let view = ConfigView::build(
            &cfg,
            resolve_key(None, &cfg.groq_api_key),
            resolve_key(None, &cfg.gemini_api_key),
        );
        assert_eq!(view.format_model_candidates.len(), 3);
        assert_eq!(view.groq_format_model_candidates.len(), 3);
        let json = serde_json::to_value(&view).expect("JSON にできる");
        assert_eq!(
            json["format_model_candidates"][0]["name"],
            DEFAULT_FORMAT_MODEL
        );
        assert!(json["format_model_candidates"][0]["note"].is_string());
        assert_eq!(json["format_rules_max_chars"], MAX_FORMAT_RULES_CHARS);
    }

    #[test]
    fn auto_learning_can_be_turned_off_through_a_patch() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            auto_learn_dictionary: Some(false),
            ..ConfigPatch::default()
        });
        assert!(!cfg.auto_learn_dictionary);
    }

    #[test]
    fn learned_terms_are_stored_and_survive_a_reload() {
        let dir = std::env::temp_dir().join(format!("nox-config-learn-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);
        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                dictionary: Some(vec![DictionaryEntry::new("手で登録した語")]),
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let added = store
            .add_auto_dictionary_entries(&[("塩谷".to_string(), Some("しおや".to_string()))])
            .expect("保存できる");
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].origin, crate::dictionary::DictionaryOrigin::Auto);
        // 追加日時が入っていないと、選抜で永久に最下位のままになる。
        assert!(added[0].added_at_ms > 0);

        let reloaded = ConfigStore::load(path.clone()).snapshot();
        let written: Vec<&str> = reloaded.dictionary.iter().map(|e| e.written.as_str()).collect();
        assert_eq!(written, vec!["手で登録した語", "塩谷"]);
        assert_eq!(reloaded.dictionary[1].reading.as_deref(), Some("しおや"));

        // 2 度目は何も増えない = 何度直しても行が増えない。
        let again = store
            .add_auto_dictionary_entries(&[("塩谷".to_string(), None)])
            .expect("成功する");
        assert!(again.is_empty());
        assert_eq!(ConfigStore::load(path).snapshot().dictionary.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_stt_mode_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("nox-config-local-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);
        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                local_stt_mode: Some(LocalSttMode::Only),
                ..ConfigPatch::default()
            })
            .expect("保存できる");
        assert_eq!(
            ConfigStore::load(path).snapshot().local_stt_mode,
            LocalSttMode::Only
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_install_is_detected_as_the_first_run() {
        let dir = std::env::temp_dir().join(format!("nox-config-first-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);

        let store = ConfigStore::load(path.clone());
        assert!(store.is_first_run(), "設定が無いのに初回と判定されない");
        store.update(ConfigPatch::default()).expect("保存できる");

        // 2 回目以降は初回ではない。
        assert!(!ConfigStore::load(path).is_first_run());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_app_starts_hidden_by_default() {
        // トレイ常駐が本来の姿。起動のたびに窓が出て前景を奪うのは邪魔。
        assert!(Config::default().start_hidden);
    }

    #[test]
    fn autostart_is_off_until_the_user_asks_for_it() {
        // 入れただけでスタートアップへ入るのは利用者の環境を変える行為。
        // PRODUCT.md「ユーザーの環境を壊さない」。既定は必ず false。
        assert!(!Config::default().autostart);
    }

    #[test]
    fn an_old_config_file_without_autostart_still_loads() {
        // 旧い設定ファイルにはこの項目が無い。フィールド単位の
        // `#[serde(default)]` が効いていないと、ここで読み込みごと落ちる。
        let cfg: Config = serde_json::from_str(r#"{"language":"ja"}"#).expect("旧形式でも読める");
        assert!(!cfg.autostart);
    }

    #[test]
    fn the_default_hotkey_is_left_ctrl_plus_space() {
        let cfg = Config::default();
        assert_eq!(cfg.hotkey_vk, crate::hotkey::DEFAULT_HOTKEY_VK);
        assert_eq!(cfg.hotkey_mods, crate::hotkey::DEFAULT_HOTKEY_MODS.to_vec());
        let view = ConfigView::from(&cfg);
        assert_eq!(view.hotkey_label, "左 Ctrl + Space");
        assert_eq!(view.hotkey_mods, vec![0xA2]);
    }

    /// 旧形式 (hotkey_mods を持たない) の設定ファイルは単独キーを保つ。
    ///
    /// 既存ユーザーのホットキーが黙って「左 Ctrl + Space」に変わらないことが
    /// 大事。serde(default) で mods が空になり、トリガーは旧基準で検証される。
    #[test]
    fn a_legacy_config_without_mods_keeps_its_single_key() {
        let dir = std::env::temp_dir().join(format!("nox-config-legacy-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("テスト用ディレクトリ");

        // 旧既定の右 Ctrl 単独。
        fs::write(&path, r#"{"hotkey_vk": 163}"#).expect("書ける"); // 0xA3
        let cfg = ConfigStore::load(path.clone()).snapshot();
        assert_eq!(cfg.hotkey_vk, 0xA3);
        assert!(cfg.hotkey_mods.is_empty(), "mods が勝手に付いた");
        assert_eq!(ConfigView::from(&cfg).hotkey_label, "右 Ctrl");

        // F1 単独も同じ。
        fs::write(&path, r#"{"hotkey_vk": 112}"#).expect("書ける"); // 0x70
        let cfg = ConfigStore::load(path).snapshot();
        assert_eq!(cfg.hotkey_vk, 0x70);
        assert_eq!(ConfigView::from(&cfg).hotkey_label, "F1");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_overlay_is_on_by_default() {
        assert!(Config::default().overlay_enabled);
    }

    #[test]
    fn the_transcript_stays_in_the_clipboard_by_default() {
        // 貼付が不達でも言い直さずに済むほうを既定にする (ユーザー要求)。
        let cfg = Config::default();
        assert!(cfg.keep_transcript_in_clipboard);
        assert!(ConfigView::from(&cfg).keep_transcript_in_clipboard);
        assert_eq!(cfg.clipboard_policy(), crate::inject::ClipboardPolicy::Keep);
    }

    #[test]
    fn turning_the_setting_off_restores_with_the_configured_delay() {
        // 陰性コントロール: 「常に Keep」で通ってしまわないための対。
        let cfg = Config {
            keep_transcript_in_clipboard: false,
            restore_delay_ms: 250,
            ..Config::default()
        };
        assert_eq!(
            cfg.clipboard_policy(),
            crate::inject::ClipboardPolicy::Restore {
                delay: std::time::Duration::from_millis(250),
            }
        );
    }

    #[test]
    fn the_clipboard_setting_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("nox-config-keep-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);

        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                keep_transcript_in_clipboard: Some(false),
                ..ConfigPatch::default()
            })
            .expect("保存できる");
        let reloaded = ConfigStore::load(path.clone()).snapshot();
        assert!(
            !reloaded.keep_transcript_in_clipboard,
            "false が保存されていない"
        );

        // 既存の設定ファイル (このキーを持たない) は既定の true になる。
        fs::write(&path, r#"{"restore_delay_ms": 400}"#).expect("書ける");
        assert!(
            ConfigStore::load(path)
                .snapshot()
                .keep_transcript_in_clipboard,
            "古い設定ファイルを読むと既定値が効かない"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_hotkey_falls_back_to_the_default() {
        // 手編集で入りうる危険な値を、捕獲 UI と同じ基準で弾く。
        // 単独キー (mods 空) は旧基準 is_allowed_hotkey で判定される。
        let rejected_singles = [
            0u32,  // キー無し
            0x100, // 範囲外
            9_999, // 範囲外
            0x1B,  // Esc (取り消し用なので選べない)
            0x01,  // マウス左
            0x02,  // マウス右
            0x04,  // マウス中
            0x05,  // マウス X1
            0x06,  // マウス X2
            0x41,  // A (押しっぱなしで文字が入り続ける)
            0x0D,  // Enter (送信連発)
            0x20,  // Space (単独では空白が入り続ける)
        ];
        for broken in rejected_singles {
            let mut cfg = Config {
                hotkey_vk: broken,
                hotkey_mods: Vec::new(),
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(
                cfg.hotkey_vk,
                crate::hotkey::DEFAULT_HOTKEY_VK,
                "VK 0x{broken:02X} を受け入れてしまった"
            );
            assert!(!cfg.hotkey_mods.is_empty(), "既定の組み合わせが壊れた");
        }

        // 組み合わせ (mods 非空) のトリガーは緩和基準だが、危険キーは不可。
        for (mods, vk) in [
            (vec![0xA2u32], 0x41u32), // Ctrl + A (文字が入り続ける)
            (vec![0xA2], 0x0D),       // Ctrl + Enter (送信連発)
        ] {
            let mut cfg = Config {
                hotkey_vk: vk,
                hotkey_mods: mods,
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(
                cfg.hotkey_vk,
                crate::hotkey::DEFAULT_HOTKEY_VK,
                "VK 0x{vk:02X} の組み合わせを受け入れてしまった"
            );
        }

        // mods に混ざった非修飾キー・重複は落とされて正規化される。
        // F5 単独として成立するので、トリガーごと既定へは倒さない。
        let mut cfg = Config {
            hotkey_vk: 0x74,
            hotkey_mods: vec![0x41, 0xA2, 0xA2],
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.hotkey_vk, 0x74);
        assert_eq!(cfg.hotkey_mods, vec![0xA2], "非修飾子と重複が残った");
    }

    #[test]
    fn a_valid_hotkey_survives_normalize() {
        // 単独キー (mods 空) はそのまま残る。
        for ok in [0xA3u32, 0xA5, 0x14, 0x70, 0x87, 0x5B] {
            let mut cfg = Config {
                hotkey_vk: ok,
                hotkey_mods: Vec::new(),
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(cfg.hotkey_vk, ok, "VK 0x{ok:02X} が消された");
            assert!(cfg.hotkey_mods.is_empty(), "VK 0x{ok:02X} に mods が付いた");
        }

        // 組み合わせも正規化を通って残る。
        let mut cfg = Config {
            hotkey_vk: 0x20,
            hotkey_mods: vec![0xA2],
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.hotkey_vk, 0x20);
        assert_eq!(cfg.hotkey_mods, vec![0xA2]);
        assert_eq!(ConfigView::from(&cfg).hotkey_label, "左 Ctrl + Space");
    }

    #[test]
    fn the_default_cancel_key_is_escape() {
        let cfg = Config::default();
        assert_eq!(cfg.cancel_vk, crate::hotkey::DEFAULT_CANCEL_VK);
        assert_eq!(cfg.cancel_vk, 0x1B);
        assert_eq!(ConfigView::from(&cfg).cancel_label, "Esc");
    }

    #[test]
    fn an_invalid_cancel_key_falls_back_to_the_default() {
        // 手編集で入りうる危険な値を弾く。基準はホットキーとは違う
        // (is_allowed_cancel_vk を参照): 1 回押しなので Esc / 文字 / Enter は可。
        let rejected = [
            0x01u32, // マウス左 (キーボードフックに来ない)
            0xF4,    // 半角/全角 (押すたびに VK が揺れる)
            0x15,    // かな
            0x100,   // 範囲外
        ];
        for broken in rejected {
            let mut cfg = Config {
                cancel_vk: broken,
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(
                cfg.cancel_vk,
                crate::hotkey::DEFAULT_CANCEL_VK,
                "VK 0x{broken:02X} を受け入れてしまった"
            );
        }
        // 文字キーや Enter、そして既定の Esc は 1 回押しなら実害がないので通す。
        let accepted = [0x41u32, 0x0D, crate::hotkey::DEFAULT_CANCEL_VK];
        for ok in accepted {
            let mut cfg = Config {
                cancel_vk: ok,
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(cfg.cancel_vk, ok, "VK 0x{ok:02X} を潰してしまった");
        }
        // Space も 1 回押しなら可。ただし既定ホットキー (左 Ctrl + Space) の
        // トリガーと重複するため、単独キーホットキーとの組で確かめる。
        let mut cfg = Config {
            hotkey_vk: 0x70,
            hotkey_mods: Vec::new(),
            cancel_vk: 0x20,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.cancel_vk, 0x20, "Space を潰してしまった");
        // 0 は「無効化」という意味の正しい値なのでそのまま通す。
        let mut disabled = Config {
            cancel_vk: 0,
            ..Config::default()
        };
        disabled.normalize();
        assert_eq!(disabled.cancel_vk, 0, "無効化の 0 を潰した");
    }

    #[test]
    fn a_cancel_key_clashing_with_the_hotkey_falls_back_to_the_default() {
        // 押すたびに録音とキャンセルが同時に起こるので、衝突は必ず解消する。
        let mut cfg = Config {
            hotkey_vk: 0x70,
            hotkey_mods: Vec::new(),
            cancel_vk: 0x70,
            ..Config::default()
        };
        cfg.normalize();
        assert_ne!(
            cfg.cancel_vk, cfg.hotkey_vk,
            "ホットキーとの衝突が残っている"
        );
        assert_eq!(cfg.cancel_vk, crate::hotkey::DEFAULT_CANCEL_VK);

        // 組み合わせのトリガーや修飾子との衝突も解消する。
        // (Ctrl + Space の最中に Ctrl を押す = キャンセル、では意味が壊れる)
        for mods in [vec![0xA2u32], vec![0xA2, 0xA0]] {
            let mut cfg = Config {
                hotkey_vk: 0x20,
                hotkey_mods: mods.clone(),
                cancel_vk: 0x20,
                ..Config::default()
            };
            cfg.normalize();
            assert_ne!(
                cfg.cancel_vk, 0x20,
                "トリガーとの衝突が残っている (mods={mods:?})"
            );

            for mod_vk in &mods {
                let mut cfg = Config {
                    hotkey_vk: 0x20,
                    hotkey_mods: mods.clone(),
                    cancel_vk: *mod_vk,
                    ..Config::default()
                };
                cfg.normalize();
                assert_eq!(
                    cfg.cancel_vk,
                    crate::hotkey::DEFAULT_CANCEL_VK,
                    "修飾子 {mod_vk:#x} との衝突が残った"
                );
            }
        }

        // 無効化 (0) との比較は衝突にならない。
        let mut disabled = Config {
            hotkey_vk: 0x70,
            hotkey_mods: Vec::new(),
            cancel_vk: 0,
            ..Config::default()
        };
        disabled.normalize();
        assert_eq!(disabled.cancel_vk, 0);
    }

    #[test]
    fn a_cancel_chord_is_normalized_and_labelled() {
        let mut cfg = Config {
            cancel_vk: 0x5A,
            // 非修飾キーと重複は落ち、表示順 (Ctrl → Shift) に並ぶ。
            cancel_mods: vec![0xA0, 0x41, 0xA2, 0xA0],
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.cancel_mods, vec![0xA2, 0xA0]);
        assert_eq!(
            ConfigView::from(&cfg).cancel_label,
            "左 Ctrl + 左 Shift + Z"
        );
    }

    #[test]
    fn a_disabled_cancel_key_has_no_label_or_mods() {
        // 旧実装は key_label(0) をそのまま出し、画面に「VK 0x00」と表示された。
        let mut cfg = Config {
            cancel_vk: 0,
            cancel_mods: vec![0xA2],
            ..Config::default()
        };
        cfg.normalize();
        assert!(cfg.cancel_mods.is_empty());
        assert_eq!(ConfigView::from(&cfg).cancel_label, "");
    }

    #[test]
    fn a_cancel_chord_may_share_a_modifier_with_the_hotkey() {
        // 左 Ctrl + Space の録音中に左 Ctrl + Z で取り消す、は両立する。
        let mut cfg = Config {
            cancel_vk: 0x5A,
            cancel_mods: vec![0xA2],
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.cancel_vk, 0x5A);
        assert_eq!(cfg.cancel_mods, vec![0xA2]);

        // ホットキーのトリガーを修飾キーに含むのは不可。修飾子も一緒に既定へ戻る。
        let mut clash = Config {
            hotkey_vk: 0xA2,
            hotkey_mods: Vec::new(),
            cancel_vk: 0x5A,
            cancel_mods: vec![0xA2],
            ..Config::default()
        };
        clash.normalize();
        assert_eq!(clash.cancel_vk, crate::hotkey::DEFAULT_CANCEL_VK);
        assert!(clash.cancel_mods.is_empty());
    }

    #[test]
    fn a_config_without_cancel_mods_loads_as_a_single_key() {
        // 旧形式の設定ファイルには cancel_mods が無い。
        let mut value = serde_json::to_value(Config::default()).expect("直列化できる");
        let object = value.as_object_mut().expect("オブジェクト");
        object.remove("cancel_mods");
        object.insert("cancel_vk".to_string(), serde_json::json!(0x91));
        let cfg: Config = serde_json::from_value(value).expect("読める");
        assert_eq!(cfg.cancel_vk, 0x91);
        assert!(cfg.cancel_mods.is_empty());
    }

    #[test]
    fn the_cancel_key_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("nox-config-cancel-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);

        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                cancel_vk: Some(0x91), // ScrollLock
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path).snapshot();
        assert_eq!(reloaded.cancel_vk, 0x91);
        assert_eq!(ConfigView::from(&reloaded).cancel_label, "ScrollLock");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hotkey_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("nox-config-hotkey-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);

        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                hotkey_vk: Some(0x20),               // Space
                hotkey_mods: Some(vec![0xA0, 0xA2]), // 左 Shift + 左 Ctrl
                overlay_enabled: Some(false),
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path).snapshot();
        assert_eq!(reloaded.hotkey_vk, 0x20);
        assert_eq!(
            reloaded.hotkey_mods,
            vec![0xA2, 0xA0],
            "正規化順で保存される"
        );
        assert!(!reloaded.overlay_enabled);
        assert_eq!(
            ConfigView::from(&reloaded).hotkey_label,
            "左 Ctrl + 左 Shift + Space"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deep_context_is_off_by_default() {
        // 画面テキストをクラウドへ送る機能なので、黙って有効にしない。
        assert!(!Config::default().deep_context);
    }

    #[test]
    fn style_profiles_ship_with_defaults() {
        let cfg = Config::default();
        assert!(!cfg.style_profiles.is_empty());
        assert!(cfg
            .style_profiles
            .iter()
            .any(|p| p.process.contains("slack")));
        // 新規ユーザーは「現行版を取り込み済み」から始まる。
        assert_eq!(
            cfg.style_defaults_version,
            crate::style::STYLE_DEFAULTS_VERSION
        );
        assert!(cfg.style_removed_default_ids.is_empty());
    }

    // --- 既定プロファイルの版管理 ------------------------------------------
    //
    // ここで守っているのは「ユーザーが編集・削除したものを、アプリ更新で
    // 復活させない」という約束。壊れても例外は出ず、次のリリースで
    // 消したはずのプロファイルが静かに戻るだけなので、テストで固定する。

    /// 一時ディレクトリつきの設定ファイル。
    fn temp_config(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nox-style-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("作れる");
        dir.join("config.json")
    }

    #[test]
    fn deleting_a_default_profile_survives_a_restart() {
        // シナリオ: 既定を消したユーザー。**この機能で一番壊れやすい約束**。
        let path = temp_config("deleted");
        let store = ConfigStore::load(path.clone());
        let kept: Vec<StyleProfile> = store
            .snapshot()
            .style_profiles
            .into_iter()
            .filter(|p| p.id != "chat.slack")
            .collect();
        store
            .update(ConfigPatch {
                style_profiles: Some(kept.clone()),
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path.clone()).snapshot();
        assert!(
            !reloaded.style_profiles.iter().any(|p| p.id == "chat.slack"),
            "消した既定が起動で戻った"
        );
        assert!(reloaded
            .style_removed_default_ids
            .contains(&"chat.slack".to_string()));
        // 巻き添えで他が消えていないこと。
        assert_eq!(reloaded.style_profiles.len(), kept.len());
        let _ = fs::remove_dir_all(path_parent(&path));
    }

    #[test]
    fn editing_a_default_profile_marks_it_and_keeps_it() {
        // シナリオ: 既定を編集したユーザー。
        let path = temp_config("edited");
        let store = ConfigStore::load(path.clone());
        let mut profiles = store.snapshot().style_profiles;
        let slack = profiles
            .iter_mut()
            .find(|p| p.id == "chat.slack")
            .expect("既定にある");
        slack.instruction = "自分で書いた指示".to_string();
        store
            .update(ConfigPatch {
                style_profiles: Some(profiles),
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path.clone()).snapshot();
        let slack = reloaded
            .style_profiles
            .iter()
            .find(|p| p.id == "chat.slack")
            .expect("残っている");
        assert_eq!(slack.instruction, "自分で書いた指示");
        assert!(slack.user_edited, "編集の印が立っていない");
        let _ = fs::remove_dir_all(path_parent(&path));
    }

    #[test]
    fn a_user_made_profile_never_claims_a_default_id() {
        // UI が既定の id を名乗ってきても、保存前の一覧に無い id は空へ倒す。
        // ここが緩いと、フロントの取り違えで (a) 実体の無い「既定」行が
        // 生まれ、(b) 削除の記録が黙って取り消され、(c) その id の将来の
        // 既定配信が塞がれる。3 つとも無言で起きるのでテストで固定する。
        let mut cfg = Config {
            style_profiles: Vec::new(),
            // 「ユーザーが chat.slack を消した」状態から始める。
            style_removed_default_ids: vec!["chat.slack".to_string()],
            ..Config::default()
        };
        cfg.apply(ConfigPatch {
            style_profiles: Some(vec![StyleProfile {
                process: "myapp.exe".into(),
                title_contains: None,
                instruction: "自作".into(),
                id: "chat.slack".into(),
                user_edited: false,
            }]),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.style_profiles.len(), 1);
        assert_eq!(cfg.style_profiles[0].id, "", "既定の id を名乗り通り受けた");
        assert!(!cfg.style_profiles[0].user_edited);
        assert_eq!(
            cfg.style_removed_default_ids,
            vec!["chat.slack".to_string()],
            "未知 id の受理で削除の記録が取り消された"
        );

        // 削除の記録が生きているので、次のマージでも復活しない。
        cfg.merge_style_defaults();
        assert!(
            !cfg.style_profiles.iter().any(|p| p.id == "chat.slack"),
            "消した既定が戻った"
        );
    }

    #[test]
    fn two_rows_cannot_share_one_default_id() {
        // 重複した id はマージ側で最初の 1 行しか更新されず、残りが
        // 更新されない幽霊として残る。2 行目以降はユーザー作成へ倒す。
        let mut cfg = Config::default();
        let slack = cfg
            .style_profiles
            .iter()
            .find(|p| p.id == "chat.slack")
            .cloned()
            .expect("既定にある");
        let mut twin = slack.clone();
        twin.instruction = "二重の行".to_string();
        cfg.apply(ConfigPatch {
            style_profiles: Some(vec![slack, twin]),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.style_profiles.len(), 2);
        assert_eq!(cfg.style_profiles[0].id, "chat.slack");
        assert_eq!(cfg.style_profiles[1].id, "", "2 行目まで既定を名乗った");
    }

    #[test]
    fn an_old_config_file_receives_the_new_defaults_without_duplicates() {
        // シナリオ: 版管理より前の設定ファイル。旧既定 7 件は id を
        // 引き継ぎ、新しく増えた既定だけが届く。
        let path = temp_config("legacy");
        let legacy = r#"{
            "groq_api_key": "",
            "gemini_api_key": "",
            "language": "ja",
            "dictionary": [],
            "formatting_enabled": true,
            "injection_enabled": true,
            "local_stt_mode": "fallback",
            "local_model_sha256": "",
            "start_hidden": true,
            "hotkey_vk": 32,
            "cancel_vk": 27,
            "overlay_enabled": true,
            "deep_context": false,
            "style_profiles": [
                {"process": "slack.exe", "instruction": "チャットの発言。簡潔な口語で、丁寧すぎない自然な調子にする。挨拶や定型の前置きは付けない"},
                {"process": "outlook.exe", "instruction": "私が書き換えたメールの指示"},
                {"process": "myapp.exe", "instruction": "自作"}
            ],
            "history_enabled": true,
            "history_retention_days": 30,
            "restore_delay_ms": 180,
            "keep_transcript_in_clipboard": true,
            "typing_speed_chars_per_min": 60,
            "groq_endpoint": "",
            "gemini_endpoint": "",
            "stt_model": "",
            "format_model": ""
        }"#;
        fs::write(&path, legacy).expect("書ける");

        let cfg = ConfigStore::load(path.clone()).snapshot();
        assert_eq!(
            cfg.style_defaults_version,
            crate::style::STYLE_DEFAULTS_VERSION
        );
        // 旧既定が二重に生えていない。
        let slack: Vec<_> = cfg
            .style_profiles
            .iter()
            .filter(|p| p.process == "slack.exe")
            .collect();
        assert_eq!(slack.len(), 1, "旧既定と新既定が二重になった");
        assert_eq!(slack[0].id, "chat.slack");
        // 書き換えていた項目は書き換えたまま。
        let outlook = cfg
            .style_profiles
            .iter()
            .find(|p| p.id == "mail.outlook")
            .expect("引き継がれる");
        assert_eq!(outlook.instruction, "私が書き換えたメールの指示");
        assert!(outlook.user_edited);
        // 旧設定に無かった既定は「消した」扱いで戻らない。
        assert!(!cfg.style_profiles.iter().any(|p| p.id == "chat.discord"));
        // 新カタログ分 (ブラウザ) は届く。これがこの仕組みの目的。
        assert!(cfg.style_profiles.iter().any(|p| p.id == "web.generic"));
        // ユーザー作成は素通し。
        let mine = cfg
            .style_profiles
            .iter()
            .find(|p| p.process == "myapp.exe")
            .expect("残る");
        assert!(mine.is_user_made());

        // 取り込みは保存され、次の起動で同じ結果になる (冪等)。
        let again = ConfigStore::load(path.clone()).snapshot();
        assert_eq!(again.style_profiles, cfg.style_profiles);
        let _ = fs::remove_dir_all(path_parent(&path));
    }

    fn path_parent(path: &Path) -> PathBuf {
        path.parent().unwrap_or(Path::new(".")).to_path_buf()
    }

    #[test]
    fn patching_style_profiles_drops_incomplete_rows() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            style_profiles: Some(vec![
                StyleProfile {
                    process: "slack.exe".into(),
                    title_contains: None,
                    instruction: "カジュアル".into(),
                    id: String::new(),
                    user_edited: false,
                },
                // 書きかけ: プロセス名が空 = 全発話に効いてしまう。
                StyleProfile {
                    process: "  ".into(),
                    title_contains: None,
                    instruction: "壊れた".into(),
                    id: String::new(),
                    user_edited: false,
                },
                // 指示が空 = 意味がない。
                StyleProfile {
                    process: "code.exe".into(),
                    title_contains: None,
                    instruction: "".into(),
                    id: String::new(),
                    user_edited: false,
                },
            ]),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.style_profiles.len(), 1);
        assert_eq!(cfg.style_profiles[0].process, "slack.exe");
    }

    #[test]
    fn dictionary_entries_keep_readings() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            dictionary: Some(vec![
                DictionaryEntry::new("nox-voice"),
                DictionaryEntry {
                    reading: Some("  しおや  ".into()),
                    ..DictionaryEntry::new("塩谷")
                },
            ]),
            ..ConfigPatch::default()
        });
        let entries = cfg.dictionary_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].written, "塩谷");
        // 読みも trim される (前後の空白は「読み無し」との差にならない)。
        assert_eq!(entries[1].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn config_round_trips_the_new_fields() {
        let dir = std::env::temp_dir().join(format!("nox-config-q1-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);

        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                deep_context: Some(true),
                style_profiles: Some(vec![StyleProfile {
                    process: "myapp.exe".into(),
                    title_contains: Some("編集".into()),
                    instruction: "箇条書きにする".into(),
                    id: String::new(),
                    user_edited: false,
                }]),
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path).snapshot();
        assert!(reloaded.deep_context);
        assert_eq!(reloaded.style_profiles.len(), 1);
        assert_eq!(
            reloaded.style_profiles[0].title_contains.as_deref(),
            Some("編集")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gemini_url_is_built_from_endpoint_and_model() {
        let cfg = Config {
            gemini_endpoint: "https://example.test/v1beta/models/".to_string(),
            format_model: "gemini-2.5-flash".to_string(),
            ..Config::default()
        };
        assert_eq!(
            cfg.gemini_url(),
            "https://example.test/v1beta/models/gemini-2.5-flash:generateContent"
        );
        // キーが URL に混ざらないこと (x-goog-api-key ヘッダで渡す方針)。
        assert!(!cfg.gemini_url().contains("key="));
        // 「試す」用の組み立ては同じ関数を通る (保存済みの名前で同じ URL になる)。
        assert_eq!(cfg.gemini_url_for(&cfg.format_model), cfg.gemini_url());
        assert_eq!(
            cfg.gemini_url_for("gemini-3.5-flash-lite"),
            "https://example.test/v1beta/models/gemini-3.5-flash-lite:generateContent"
        );
    }

    #[test]
    fn a_saved_gemini_model_always_builds_a_single_path_segment() {
        // 保存で通った名前は、URL のパスを 1 段だけ足す形にしかならない。
        let mut cfg = Config {
            gemini_endpoint: "https://example.test/v1beta/models".to_string(),
            ..Config::default()
        };
        cfg.apply(ConfigPatch {
            format_model: Some("models/gemini-flash-latest".to_string()),
            ..ConfigPatch::default()
        });
        let url = cfg.gemini_url();
        assert_eq!(
            url,
            "https://example.test/v1beta/models/gemini-flash-latest:generateContent"
        );
        let tail = url
            .strip_prefix("https://example.test/v1beta/models/")
            .expect("エンドポイントの下");
        assert!(!tail.contains('/') && !tail.contains('?') && !tail.contains('#'));
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("nox-config-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);

        let store = ConfigStore::load(path.clone());
        store
            .update(ConfigPatch {
                groq_api_key: Some("gsk_test".to_string()),
                language: Some("en".to_string()),
                formatting_enabled: Some(false),
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path).snapshot();
        assert_eq!(reloaded.groq_api_key.expose(), "gsk_test");
        assert_eq!(reloaded.language, "en");
        assert!(!reloaded.formatting_enabled);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_config_file_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("nox-config-broken-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("テスト用ディレクトリ");
        fs::write(&path, "{ not json").expect("書ける");

        let cfg = ConfigStore::load(path).snapshot();
        assert_eq!(cfg.language, DEFAULT_LANGUAGE);
        assert!(cfg.formatting_enabled);

        let _ = fs::remove_dir_all(&dir);
    }

    /// m-3 回帰: 壊れた設定は次の保存で無言消滅させず、.bak に残す。
    #[test]
    fn broken_config_file_is_backed_up_before_defaults_take_over() {
        let dir = std::env::temp_dir().join(format!("nox-config-bak-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("テスト用ディレクトリ");
        // 手で直せばキーを救えたはずの壊れ方。
        let original = r#"{ "groq_api_key": "gsk_recoverable", "language": "ja" "#;
        fs::write(&path, original).expect("書ける");

        let store = ConfigStore::load(path.clone());
        let backup = path.with_extension("json.bak");
        assert!(backup.exists(), "壊れた設定が退避されていない");
        assert_eq!(
            fs::read_to_string(&backup).expect("読める"),
            original,
            "退避内容が原本と違う"
        );

        // 既定値で保存しても .bak は残り続ける。
        store.update(ConfigPatch::default()).expect("保存できる");
        assert!(backup.exists(), "保存で退避が消えた");
        assert!(path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// m-3: 二度目に壊れても、最初の退避 (原本) を潰さない。
    #[test]
    fn existing_backup_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("nox-config-bak2-{}", std::process::id()));
        let path = dir.join("config.json");
        let backup = path.with_extension("json.bak");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("テスト用ディレクトリ");
        fs::write(&backup, "最初の原本").expect("書ける");
        fs::write(&path, "{ broken again").expect("書ける");

        ConfigStore::load(path);
        assert_eq!(
            fs::read_to_string(&backup).expect("読める"),
            "最初の原本",
            "既存の退避を潰した"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // --- m6 回帰: 復元待ち時間のクランプ ---

    #[test]
    fn typing_speed_chars_per_min_is_clamped_to_valid_range() {
        // Boundary test: values below minimum (10) should be clamped to 10
        let mut cfg = Config {
            typing_speed_chars_per_min: 9,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.typing_speed_chars_per_min, 10, "9 should clamp to 10");

        // Boundary test: values above maximum (300) should be clamped to 300
        let mut cfg = Config {
            typing_speed_chars_per_min: 301,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(
            cfg.typing_speed_chars_per_min, 300,
            "301 should clamp to 300"
        );

        // Zero should remain zero (special value meaning "don't calculate")
        let mut cfg = Config {
            typing_speed_chars_per_min: 0,
            ..Config::default()
        };
        cfg.normalize();
        assert_eq!(cfg.typing_speed_chars_per_min, 0, "0 should remain 0");
    }

    #[test]
    fn restore_delay_is_clamped_when_patched() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            restore_delay_ms: Some(0),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.restore_delay_ms, MIN_RESTORE_DELAY_MS);

        cfg.apply(ConfigPatch {
            restore_delay_ms: Some(u64::MAX),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.restore_delay_ms, MAX_RESTORE_DELAY_MS);
    }

    /// 設定ファイルを手で編集された場合も範囲内に正すこと。
    ///
    /// 0 のままだと貼付が消費される前に復元して旧内容が貼られ (R3-b)、
    /// 巨大値だと後処理ワーカーがその間ずっと塞がる。
    #[test]
    fn restore_delay_is_clamped_on_load() {
        let dir = std::env::temp_dir().join(format!("nox-config-clamp-{}", std::process::id()));
        let path = dir.join("config.json");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("テスト用ディレクトリ");

        fs::write(&path, r#"{"restore_delay_ms": 0}"#).expect("書ける");
        assert_eq!(
            ConfigStore::load(path.clone()).snapshot().restore_delay_ms,
            MIN_RESTORE_DELAY_MS,
            "0 がそのまま読み込まれている"
        );

        fs::write(&path, r#"{"restore_delay_ms": 999999999}"#).expect("書ける");
        assert_eq!(
            ConfigStore::load(path.clone()).snapshot().restore_delay_ms,
            MAX_RESTORE_DELAY_MS,
            "巨大値がそのまま読み込まれている"
        );

        // 範囲内の値はそのまま。
        fs::write(&path, r#"{"restore_delay_ms": 400}"#).expect("書ける");
        assert_eq!(ConfigStore::load(path).snapshot().restore_delay_ms, 400);

        let _ = fs::remove_dir_all(&dir);
    }

    /// m-5 回帰: ConfigPatch を `{:?}` してもキーが漏れない。
    #[test]
    fn config_patch_debug_redacts_keys() {
        let patch = ConfigPatch {
            groq_api_key: Some("gsk_must_not_leak".to_string()),
            gemini_api_key: Some("AIzaSy_must_not_leak".to_string()),
            language: Some("ja".to_string()),
            ..ConfigPatch::default()
        };
        let dumped = format!("{patch:?}");
        assert!(!dumped.contains("gsk_must_not_leak"), "{dumped}");
        assert!(!dumped.contains("AIzaSy_must_not_leak"), "{dumped}");
        assert!(dumped.contains("<redacted>"), "{dumped}");
        // キーでない項目は読めたままにする (デバッグの役に立つように)。
        assert!(dumped.contains("ja"), "{dumped}");
    }

    #[test]
    fn config_patch_debug_distinguishes_absent_from_cleared() {
        let cleared = ConfigPatch {
            groq_api_key: Some(String::new()),
            ..ConfigPatch::default()
        };
        assert!(format!("{cleared:?}").contains("groq_api_key: Some(<empty>)"));
        let absent = ConfigPatch::default();
        assert!(format!("{absent:?}").contains("groq_api_key: None"));
    }
}
