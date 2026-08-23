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
use std::sync::Mutex;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::dictionary::{self, DictionaryEntry};
use crate::style::StyleProfile;

/// Groq の既定エンドポイント (OpenAI 互換の transcriptions)。
pub const DEFAULT_GROQ_ENDPOINT: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
/// Gemini の既定エンドポイント。末尾に `/{model}:generateContent` が付く。
pub const DEFAULT_GEMINI_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/models";
pub const DEFAULT_STT_MODEL: &str = "whisper-large-v3";

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

/// 履歴の既定保持日数。
pub const DEFAULT_HISTORY_RETENTION_DAYS: u32 = 30;
/// 保持日数の上限 (約 10 年)。0 は「無制限」を意味するので別扱い。
pub const MAX_HISTORY_RETENTION_DAYS: u32 = 3_650;

/// 復元待ちの下限。0 だと貼付が消費される前に戻してしまう。
pub const MIN_RESTORE_DELAY_MS: u64 = 50;
/// 復元待ちの上限。長いほどユーザーの次のコピーを壊す窓が広がる。
pub const MAX_RESTORE_DELAY_MS: u64 = 5_000;

const ENV_GROQ_KEY: &str = "GROQ_API_KEY";
const ENV_GEMINI_KEY: &str = "GEMINI_API_KEY";

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
        let tail: String = trimmed.chars().rev().take(4).collect::<Vec<_>>()
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
    /// 整形プロンプトへ差し込む用語リスト (固有名詞・専門用語の表記ゆれ対策)。
    pub dictionary: Vec<String>,
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
    /// PTT に使う仮想キーコード。既定は右 Ctrl。
    pub hotkey_vk: u32,
    /// 録音を破棄するキャンセルキー。既定は Esc。**0 で無効化**。
    ///
    /// ホットキーとの重複は不可 ([`Config::normalize`] が解消する)。
    pub cancel_vk: u32,
    /// 録音中・処理中の小窓を出すか。
    pub overlay_enabled: bool,
    /// 画面のテキストを読んで文脈として使うか (deep context)。
    ///
    /// **既定は無効。** 有効にすると、挿入先の画面に表示されている文章が
    /// STT / 整形の API へ送られる (design.md R1)。UI でその旨を明示すること。
    pub deep_context: bool,
    /// 挿入先アプリごとの文体プロファイル。
    pub style_profiles: Vec<StyleProfile>,
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
    pub groq_endpoint: String,
    pub gemini_endpoint: String,
    pub stt_model: String,
    pub format_model: String,
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
            hotkey_vk: crate::hotkey::DEFAULT_HOTKEY_VK,
            cancel_vk: crate::hotkey::DEFAULT_CANCEL_VK,
            overlay_enabled: true,
            // 画面テキストをクラウドへ送るので、明示的に有効化させる。
            deep_context: false,
            style_profiles: crate::style::default_profiles(),
            history_enabled: true,
            history_retention_days: DEFAULT_HISTORY_RETENTION_DAYS,
            restore_delay_ms: crate::inject::DEFAULT_RESTORE_DELAY_MS,
            // 貼付に失敗しても言い直さずに済むほうを既定にする。
            keep_transcript_in_clipboard: true,
            groq_endpoint: DEFAULT_GROQ_ENDPOINT.to_string(),
            gemini_endpoint: DEFAULT_GEMINI_ENDPOINT.to_string(),
            stt_model: DEFAULT_STT_MODEL.to_string(),
            format_model: DEFAULT_FORMAT_MODEL.to_string(),
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
    /// 環境変数から。設定ファイルより優先される。
    Env,
    /// 設定ファイルから。
    Config,
    /// 未設定。
    None,
}

/// 解決済みのキー (環境変数 > 設定ファイル)。
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
    /// Groq の API キーを解決する (環境変数優先)。
    pub fn groq_key(&self) -> ResolvedKey {
        resolve_key(env_value(ENV_GROQ_KEY), &self.groq_api_key)
    }

    /// Gemini の API キーを解決する (環境変数優先)。
    pub fn gemini_key(&self) -> ResolvedKey {
        resolve_key(env_value(ENV_GEMINI_KEY), &self.gemini_api_key)
    }

    /// 辞書を構造化して返す (`表記,よみ` のパース済み)。
    pub fn dictionary_entries(&self) -> Vec<DictionaryEntry> {
        dictionary::parse_entries(&self.dictionary)
    }

    /// `{gemini_endpoint}/{model}:generateContent` を組み立てる。
    pub fn gemini_url(&self) -> String {
        format!(
            "{}/{}:generateContent",
            self.gemini_endpoint.trim_end_matches('/'),
            self.format_model
        )
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
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
    pub language: String,
    pub dictionary: Vec<String>,
    pub formatting_enabled: bool,
    pub injection_enabled: bool,
    pub deep_context: bool,
    pub style_profiles: Vec<StyleProfile>,
    pub local_stt_mode: LocalSttMode,
    pub start_hidden: bool,
    pub hotkey_vk: u32,
    /// 表示用のキー名 (「右 Ctrl」など)。
    pub hotkey_label: String,
    /// 録音キャンセルキーの表示名 (「Esc」など)。
    pub cancel_label: String,
    pub overlay_enabled: bool,
    pub history_enabled: bool,
    pub history_retention_days: u32,
    pub restore_delay_ms: u64,
    pub keep_transcript_in_clipboard: bool,
    pub stt_model: String,
    pub format_model: String,
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
            language: c.language.clone(),
            dictionary: c.dictionary.clone(),
            formatting_enabled: c.formatting_enabled,
            injection_enabled: c.injection_enabled,
            deep_context: c.deep_context,
            style_profiles: c.style_profiles.clone(),
            local_stt_mode: c.local_stt_mode,
            start_hidden: c.start_hidden,
            hotkey_vk: c.hotkey_vk,
            hotkey_label: crate::hotkey::key_label(c.hotkey_vk),
            cancel_label: crate::hotkey::key_label(c.cancel_vk),
            overlay_enabled: c.overlay_enabled,
            history_enabled: c.history_enabled,
            history_retention_days: c.history_retention_days,
            restore_delay_ms: c.restore_delay_ms,
            keep_transcript_in_clipboard: c.keep_transcript_in_clipboard,
            stt_model: c.stt_model.clone(),
            format_model: c.format_model.clone(),
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
    pub dictionary: Option<Vec<String>>,
    pub formatting_enabled: Option<bool>,
    pub injection_enabled: Option<bool>,
    pub deep_context: Option<bool>,
    pub style_profiles: Option<Vec<StyleProfile>>,
    pub local_stt_mode: Option<LocalSttMode>,
    pub local_model_sha256: Option<String>,
    pub start_hidden: Option<bool>,
    pub hotkey_vk: Option<u32>,
    pub cancel_vk: Option<u32>,
    pub overlay_enabled: Option<bool>,
    pub history_enabled: Option<bool>,
    pub history_retention_days: Option<u32>,
    pub restore_delay_ms: Option<u64>,
    pub keep_transcript_in_clipboard: Option<bool>,
    pub stt_model: Option<String>,
    pub format_model: Option<String>,
    pub groq_endpoint: Option<String>,
    pub gemini_endpoint: Option<String>,
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
            .field("groq_api_key", &format_args!("{}", presence(&self.groq_api_key)))
            .field(
                "gemini_api_key",
                &format_args!("{}", presence(&self.gemini_api_key)),
            )
            .field("language", &self.language)
            .field("dictionary", &self.dictionary)
            .field("formatting_enabled", &self.formatting_enabled)
            .field("injection_enabled", &self.injection_enabled)
            .field("deep_context", &self.deep_context)
            .field("local_stt_mode", &self.local_stt_mode)
            .field("start_hidden", &self.start_hidden)
            .field("hotkey_vk", &self.hotkey_vk)
            .field("cancel_vk", &self.cancel_vk)
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
            .field("stt_model", &self.stt_model)
            .field("format_model", &self.format_model)
            .field("groq_endpoint", &self.groq_endpoint)
            .field("gemini_endpoint", &self.gemini_endpoint)
            .finish()
    }
}

impl Config {
    /// 範囲外の値を安全な範囲へ丸める。
    ///
    /// パッチ適用時だけでなく**読み込み時にも**通すこと。設定ファイルは
    /// 手で編集されうるので、UI を通らない値が入ってくる。
    pub fn normalize(&mut self) {
        // 捕獲 UI と同じ不変条件をここでも守る。設定ファイルは手で編集できるので、
        // UI を通らない値 (文字キー・Enter・マウス・範囲外) が入りうる。
        // 文字キーが入ると、押している間ずっと入力先へ流れ続ける。
        if !crate::hotkey::is_allowed_hotkey(self.hotkey_vk) {
            log::warn!(
                "ホットキーに使えない値 (VK 0x{:02X}) が設定されていたので既定へ戻します",
                self.hotkey_vk
            );
            self.hotkey_vk = crate::hotkey::DEFAULT_HOTKEY_VK;
        }
        // キャンセルキーはホットキーと要件が違う (録音中の 1 回押しなので Esc や
        // 文字キーも可。hotkey.rs の is_allowed_cancel_vk を参照)。マウスや
        // VK が揺れる IME 系だけ弾く。ただし **0 は「無効化」という意味の
        // 正しい値**なので、そのまま通す。
        if self.cancel_vk != 0 && !crate::hotkey::is_allowed_cancel_vk(self.cancel_vk) {
            log::warn!(
                "キャンセルキーに使えない値 (VK 0x{:02X}) が設定されていたので既定へ戻します",
                self.cancel_vk
            );
            self.cancel_vk = crate::hotkey::DEFAULT_CANCEL_VK;
        }
        // ホットキーとの重複は禁止。押すたびに録音とキャンセルが同時に
        // 起こってしまう。キャンセル側を既定 (Esc) へ戻して解消する —
        // ホットキーは Esc を選べないので、これで必ず外れる。
        if self.cancel_vk != 0 && self.cancel_vk == self.hotkey_vk {
            log::warn!(
                "キャンセルキーがホットキー ({}) と重複していたため、既定の {} へ戻します",
                crate::hotkey::key_label(self.hotkey_vk),
                crate::hotkey::key_label(crate::hotkey::DEFAULT_CANCEL_VK)
            );
            self.cancel_vk = crate::hotkey::DEFAULT_CANCEL_VK;
        }
        self.restore_delay_ms = self
            .restore_delay_ms
            .clamp(MIN_RESTORE_DELAY_MS, MAX_RESTORE_DELAY_MS);
        // 0 は「無制限」という意味を持つので潰さない。
        if self.history_retention_days != 0 {
            self.history_retention_days =
                self.history_retention_days.min(MAX_HISTORY_RETENTION_DAYS);
        }
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
            self.dictionary = v
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
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
        if let Some(v) = patch.local_stt_mode {
            self.local_stt_mode = v;
        }
        if let Some(v) = patch.local_model_sha256 {
            self.local_model_sha256 = v.trim().to_string();
        }
        if let Some(v) = patch.start_hidden {
            self.start_hidden = v;
        }
        if let Some(v) = patch.hotkey_vk {
            self.hotkey_vk = v;
        }
        if let Some(v) = patch.cancel_vk {
            self.cancel_vk = v;
        }
        if let Some(v) = patch.overlay_enabled {
            self.overlay_enabled = v;
        }
        if let Some(v) = patch.style_profiles {
            // 条件が空のプロファイルは全発話に効いてしまうので落とす。
            self.style_profiles = v
                .into_iter()
                .filter(|p| !p.process.trim().is_empty() && !p.instruction.trim().is_empty())
                .collect();
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
        self.normalize();
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
                    log::error!(
                        "設定ファイルを解釈できません ({}): {e}",
                        path.display()
                    );
                    // 既定値で起動すると、次の保存でこのファイルが黙って
                    // 上書きされ、手で直せば救えたはずのキーが消える。
                    // 退避してから既定値へ倒す。
                    backup_broken_config(&path);
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("設定ファイルがありません。既定値を使います: {}", path.display());
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

    /// パッチを適用して保存する。
    pub fn update(&self, patch: ConfigPatch) -> Result<Config, String> {
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
        fs::create_dir_all(parent)
            .map_err(|e| format!("設定ディレクトリを作成できません ({}): {e}", parent.display()))?;
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
                " nox-voice ".to_string(),
                "".to_string(),
                "  ".to_string(),
                "Tauri".to_string(),
            ]),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.dictionary, vec!["nox-voice", "Tauri"]);
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
    fn the_default_hotkey_is_right_ctrl() {
        let cfg = Config::default();
        assert_eq!(cfg.hotkey_vk, crate::hotkey::DEFAULT_HOTKEY_VK);
        assert_eq!(ConfigView::from(&cfg).hotkey_label, "右 Ctrl");
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
        assert!(!reloaded.keep_transcript_in_clipboard, "false が保存されていない");

        // 既存の設定ファイル (このキーを持たない) は既定の true になる。
        fs::write(&path, r#"{"restore_delay_ms": 400}"#).expect("書ける");
        assert!(
            ConfigStore::load(path).snapshot().keep_transcript_in_clipboard,
            "古い設定ファイルを読むと既定値が効かない"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_invalid_hotkey_falls_back_to_the_default() {
        // 手編集で入りうる危険な値を、捕獲 UI と同じ基準で弾く。
        let rejected = [
            0u32,    // キー無し
            0x100,   // 範囲外
            9_999,   // 範囲外
            0x1B,    // Esc (取り消し用なので選べない)
            0x01,    // マウス左
            0x02,    // マウス右
            0x04,    // マウス中
            0x05,    // マウス X1
            0x06,    // マウス X2
            0x41,    // A (押しっぱなしで文字が入り続ける)
            0x0D,    // Enter (送信連発)
            0x20,    // Space
        ];
        for broken in rejected {
            let mut cfg = Config {
                hotkey_vk: broken,
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(
                cfg.hotkey_vk,
                crate::hotkey::DEFAULT_HOTKEY_VK,
                "VK 0x{broken:02X} を受け入れてしまった"
            );
        }
    }

    #[test]
    fn a_valid_hotkey_survives_normalize() {
        for ok in [0xA3u32, 0xA5, 0x14, 0x70, 0x87, 0x5B] {
            let mut cfg = Config {
                hotkey_vk: ok,
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(cfg.hotkey_vk, ok, "VK 0x{ok:02X} が消された");
        }
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
        let accepted = [0x41u32, 0x0D, 0x20, crate::hotkey::DEFAULT_CANCEL_VK];
        for ok in accepted {
            let mut cfg = Config {
                cancel_vk: ok,
                ..Config::default()
            };
            cfg.normalize();
            assert_eq!(cfg.cancel_vk, ok, "VK 0x{ok:02X} を潰してしまった");
        }
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
            cancel_vk: 0x70,
            ..Config::default()
        };
        cfg.normalize();
        assert_ne!(
            cfg.cancel_vk, cfg.hotkey_vk,
            "ホットキーとの衝突が残っている"
        );
        assert_eq!(cfg.cancel_vk, crate::hotkey::DEFAULT_CANCEL_VK);

        // 無効化 (0) との比較は衝突にならない。
        let mut disabled = Config {
            hotkey_vk: 0x70,
            cancel_vk: 0,
            ..Config::default()
        };
        disabled.normalize();
        assert_eq!(disabled.cancel_vk, 0);
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
                hotkey_vk: Some(0x70), // F1
                overlay_enabled: Some(false),
                ..ConfigPatch::default()
            })
            .expect("保存できる");

        let reloaded = ConfigStore::load(path).snapshot();
        assert_eq!(reloaded.hotkey_vk, 0x70);
        assert!(!reloaded.overlay_enabled);
        assert_eq!(ConfigView::from(&reloaded).hotkey_label, "F1");

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
                },
                // 書きかけ: プロセス名が空 = 全発話に効いてしまう。
                StyleProfile {
                    process: "  ".into(),
                    title_contains: None,
                    instruction: "壊れた".into(),
                },
                // 指示が空 = 意味がない。
                StyleProfile {
                    process: "code.exe".into(),
                    title_contains: None,
                    instruction: "".into(),
                },
            ]),
            ..ConfigPatch::default()
        });
        assert_eq!(cfg.style_profiles.len(), 1);
        assert_eq!(cfg.style_profiles[0].process, "slack.exe");
    }

    #[test]
    fn dictionary_entries_parse_readings() {
        let mut cfg = Config::default();
        cfg.apply(ConfigPatch {
            dictionary: Some(vec!["nox-voice".into(), "塩谷,しおや".into()]),
            ..ConfigPatch::default()
        });
        let entries = cfg.dictionary_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].written, "塩谷");
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
