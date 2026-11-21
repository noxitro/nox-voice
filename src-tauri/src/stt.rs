//! 音声認識 (STT) — Groq の OpenAI 互換 `audio/transcriptions`。
//!
//! 既定モデルは `whisper-large-v3` (design.md の決定事項)。
//! WAV を multipart で 1 発話ずつ送るバッチ方式。
//!
//! # 設計上の約束
//!
//! - API キーは `Authorization` ヘッダのみに載せる。**URL にもログにも出さない。**
//! - エラー本文はそのまま抱えず、先頭を切り詰めた抜粋だけを持つ
//!   ([`MAX_ERROR_BODY`])。ログに巨大な JSON を流さないため。
//! - 429 / 5xx は 1 回だけ短いバックオフで再試行する。それ以上粘らないのは、
//!   音声入力の体感速度が最優先で、失敗は上位で可視化されるため。

use std::fmt;
use std::time::{Duration, Instant};

use crate::config::Secret;

/// 接続確立のタイムアウト。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// リクエスト全体のタイムアウト。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// 再試行前の待ち時間。
const RETRY_BACKOFF: Duration = Duration::from_millis(600);
/// エラー本文をログ/UI へ載せる際の最大長。
const MAX_ERROR_BODY: usize = 400;

/// STT の失敗理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttError {
    /// API キーが未設定。
    MissingApiKey,
    /// 認証エラー (401 / 403)。キーが誤っている。
    Unauthorized(String),
    /// レート制限 (429)。再試行しても解消しなかった。
    RateLimited(String),
    /// サーバ側エラー (5xx)。
    Server { status: u16, body: String },
    /// その他の HTTP エラー (4xx)。
    Http { status: u16, body: String },
    /// タイムアウト。
    Timeout,
    /// 接続できない・切断された等。
    Network(String),
    /// 応答を解釈できない。
    Decode(String),
    /// 転写結果が空 (無音など)。
    Empty,
    /// 転写が同じ文字・語の繰り返しに陥った (Whisper の反復ループ)。
    ///
    /// 本文は持たない。中身は意味のない繰り返しで、プロンプト由来の
    /// 画面テキストが混じりうる (design.md「既知の限界: Whisperのプロンプト・エコー」)
    /// ので、ログや退避メタへ流さない。判定は [`find_repetition_loop`]。
    Repetitive,
}

impl fmt::Display for SttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SttError::MissingApiKey => write!(
                f,
                "Groq の API キーが設定されていません (.env・環境変数 GROQ_API_KEY・設定画面のいずれか)"
            ),
            SttError::Unauthorized(b) => {
                write!(f, "Groq の認証に失敗しました。API キーを確認してください: {b}")
            }
            SttError::RateLimited(b) => {
                write!(f, "Groq のレート制限に達しました。しばらく待って再試行してください: {b}")
            }
            SttError::Server { status, body } => {
                write!(f, "Groq のサーバエラー ({status}): {body}")
            }
            SttError::Http { status, body } => {
                write!(f, "Groq へのリクエストが失敗しました ({status}): {body}")
            }
            SttError::Timeout => write!(
                f,
                "Groq への転写要求がタイムアウトしました ({} 秒)",
                REQUEST_TIMEOUT.as_secs()
            ),
            SttError::Network(e) => write!(f, "Groq へ接続できません: {e}"),
            SttError::Decode(e) => write!(f, "Groq の応答を解釈できません: {e}"),
            SttError::Empty => write!(f, "転写結果が空でした (無音だった可能性があります)"),
            SttError::Repetitive => write!(
                f,
                "音声認識が同じ語の繰り返しに陥ったため、結果を捨てました (Whisper の誤認識)。同じ録音の再転写は同じようにループしやすいので、言い直すのが確実です"
            ),
        }
    }
}

impl std::error::Error for SttError {}

/// 転写結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// 生転写 (前後の空白を除去済み)。
    pub text: String,
}

/// 転写 1 回分の入力。
#[derive(Debug, Clone, Default)]
pub struct TranscribeRequest<'a> {
    pub wav: &'a [u8],
    /// 言語ヒント (ISO-639-1)。空なら自動判定に任せる。
    pub language: &'a str,
    /// 認識のバイアスに使うプロンプト。辞書と画面コンテキストから作る
    /// ([`crate::dictionary::build_whisper_prompt`])。
    pub prompt: Option<&'a str>,
}

impl<'a> TranscribeRequest<'a> {
    pub fn new(wav: &'a [u8], language: &'a str) -> Self {
        Self {
            wav,
            language,
            prompt: None,
        }
    }
}

/// STT のインターフェース。
///
/// トレイトにしてあるのは、上位のパイプラインを実 API なしでテストするため。
pub trait SpeechToText: Send + Sync {
    fn transcribe(&self, request: &TranscribeRequest<'_>) -> Result<Transcript, SttError>;
}

/// Groq 実装。
pub struct GroqStt {
    client: reqwest::blocking::Client,
    endpoint: String,
    model: String,
    api_key: Secret,
}

impl GroqStt {
    /// 呼び出し側で作った [`reqwest::blocking::Client`] を使う
    /// (接続プールを録音ごとに作り直さないため)。
    pub fn new(
        client: reqwest::blocking::Client,
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: Secret,
    ) -> Self {
        Self {
            client,
            endpoint: endpoint.into(),
            model: model.into(),
            api_key,
        }
    }

    fn send_once(&self, request: &TranscribeRequest<'_>) -> Result<String, SttError> {
        let part = reqwest::blocking::multipart::Part::bytes(request.wav.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| SttError::Decode(format!("multipart を組み立てられません: {e}")))?;

        let mut form = reqwest::blocking::multipart::Form::new()
            .text("model", self.model.clone())
            // verbose_json はセグメント情報まで返って重いので、
            // M2 の用途 (本文だけ) には json で十分。
            .text("response_format", "json")
            .part("file", part);
        if !request.language.trim().is_empty() {
            form = form.text("language", request.language.trim().to_string());
        }
        // 固有名詞や文脈を先に見せて、誤変換自体を減らす。
        if let Some(prompt) = request.prompt.map(str::trim).filter(|p| !p.is_empty()) {
            form = form.text("prompt", prompt.to_string());
        }

        let response = self
            .client
            .post(&self.endpoint)
            // キーはヘッダのみ。URL・ログには絶対に出さない。
            .bearer_auth(self.api_key.expose())
            .multipart(form)
            .send()
            .map_err(classify_transport_error)?;

        let status = response.status();
        let body = response.text().unwrap_or_default();

        if status.is_success() {
            return parse_transcription(&body);
        }
        Err(classify_status(status.as_u16(), &body))
    }
}

impl SpeechToText for GroqStt {
    fn transcribe(&self, request: &TranscribeRequest<'_>) -> Result<Transcript, SttError> {
        if self.api_key.is_empty() {
            return Err(SttError::MissingApiKey);
        }

        let started = Instant::now();
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send_once(request) {
                Ok(text) => {
                    log::info!(
                        "STT 完了: {} 文字 / {} ms / 試行 {attempt} 回",
                        text.chars().count(),
                        started.elapsed().as_millis()
                    );
                    return Ok(Transcript { text });
                }
                Err(e) if attempt == 1 && is_retryable(&e) => {
                    log::warn!("STT を再試行します ({e})");
                    std::thread::sleep(RETRY_BACKOFF);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// 一度だけ再試行する価値がある失敗か。
fn is_retryable(error: &SttError) -> bool {
    matches!(
        error,
        SttError::RateLimited(_) | SttError::Server { .. } | SttError::Timeout | SttError::Network(_)
    )
}

/// HTTP ステータスから失敗種別を決める。
fn classify_status(status: u16, body: &str) -> SttError {
    let body = truncate_body(body);
    match status {
        401 | 403 => SttError::Unauthorized(body),
        429 => SttError::RateLimited(body),
        500..=599 => SttError::Server { status, body },
        _ => SttError::Http { status, body },
    }
}

/// reqwest のエラーを分類する。
///
/// メッセージに URL が入りうるが、本アプリは URL にキーを載せないので
/// そのまま出しても漏れない (Gemini をクエリキー方式にしない理由でもある)。
fn classify_transport_error(error: reqwest::Error) -> SttError {
    if error.is_timeout() {
        SttError::Timeout
    } else {
        // 接続不可・切断・TLS 失敗などはまとめてネットワーク扱い。
        // 個別に分ける利点が (再試行方針が同じなので) 無い。
        SttError::Network(error.to_string())
    }
}

/// エラー本文を UTF-8 境界を壊さずに切り詰める。
fn truncate_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX_ERROR_BODY {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(MAX_ERROR_BODY).collect();
    format!("{cut}…")
}

/// 転写応答 (`{"text": "..."}`) から本文を取り出す。
///
/// `verbose_json` でも最上位の `text` は同じ形なので両対応できる。
/// HTTP に触れない純関数なので単体テストしやすい。
pub fn parse_transcription(body: &str) -> Result<String, SttError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SttError::Decode(format!("JSON として読めません: {e}")))?;

    // エラー応答が 200 で返ってくる実装もあるので先に見る。
    if let Some(message) = value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Err(SttError::Decode(format!(
            "API がエラーを返しました: {}",
            truncate_body(message)
        )));
    }

    let text = value
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| SttError::Decode("応答に text フィールドがありません".to_string()))?;

    let text = text.trim();
    if text.is_empty() {
        return Err(SttError::Empty);
    }
    Ok(text.to_string())
}

/// 反復ループとみなす繰り返し単位の最大長 (文字)。
///
/// Whisper のループは 1 文字 (「早早早…」) から 1 文 (「ご視聴ありがとう
/// ございました。」の連続) まであるので、文 1 つ分まで見る。
const MAX_LOOP_UNIT_CHARS: usize = 32;

/// 1 文字の単位が、これ以上連続したらループとみなす。
///
/// 日本語の口述で同じ字が 8 つ並ぶことはまず無い (笑いの「ははは」、
/// 伸ばしの「えーー」は数個で止まる)。数字の連続 (「100000000」) は
/// 単位が文字を含まないので、そもそも数えない。
const MIN_SINGLE_CHAR_REPEATS: usize = 8;
/// 2 文字以上の単位が、これ以上連続したらループとみなす。
const MIN_UNIT_REPEATS: usize = 4;
/// 2 文字以上の単位のループは、繰り返し部分がこの文字数以上あるときだけ数える。
///
/// 「ワンワンワンワン」(2×4=8) や「はい、はい、はい、はい、」(3×4=12) のような、
/// 実際に口にしうる短い繰り返しを誤って捨てないため。
const MIN_UNIT_LOOP_CHARS: usize = 16;

/// 転写が Whisper の反復ループに陥っているかを判定する (純関数)。
///
/// ループしていれば `(単位の文字数, 連続回数)` を返す (ログ用)。
/// 単位の中身は返さない — 画面テキスト由来の語でありうるので、ログに出さない。
///
/// # なぜ要るか
///
/// Whisper は短い・不明瞭な音声で、同じトークンを延々と出し続けることがある
/// (2026-11-21 に 4 秒の録音で、同じ漢字が 13 個続く転写が出た)。
/// API はこれを**成功として返す**ので、検出しないと整形を素通りして貼り付けられ、
/// 小窓には「整形済み」が出る。「失敗は騒がしく」に反する。
///
/// 判定は本文だけで行う。`verbose_json` の `compression_ratio` は短い日本語では
/// 当てにならないうえ応答が重い (このファイル冒頭の `response_format` の注記)。
pub fn find_repetition_loop(text: &str) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    for start in 0..n {
        for unit_len in 1..=MAX_LOOP_UNIT_CHARS {
            if start + unit_len * 2 > n {
                break;
            }
            let unit = &chars[start..start + unit_len];
            // 文字を含まない単位 (数字・記号・空白だけ) は数えない。
            // 「100000000」「…………」「!!!!!!!!」はループではない。
            if !unit.iter().any(|c| c.is_alphabetic()) {
                continue;
            }
            let mut repeats = 1;
            while start + unit_len * (repeats + 1) <= n
                && chars[start + unit_len * repeats..start + unit_len * (repeats + 1)] == *unit
            {
                repeats += 1;
            }
            let looping = if unit_len == 1 {
                repeats >= MIN_SINGLE_CHAR_REPEATS
            } else {
                repeats >= MIN_UNIT_REPEATS && unit_len * repeats >= MIN_UNIT_LOOP_CHARS
            };
            if looping {
                return Some((unit_len, repeats));
            }
        }
    }
    None
}

/// STT / 整形で共用する HTTP クライアントを作る。
///
/// # リダイレクトを追わない
///
/// 認証ヘッダ (`Authorization` / `x-goog-api-key`) は、追跡先が別ホストでも
/// 転送されうる。API エンドポイントが正当にリダイレクトすることは無いので、
/// **追わない**方針にして、キーが意図しないホストへ飛ぶ経路を塞ぐ。
/// リダイレクトは 3xx のまま返り、通常の HTTP エラーとして扱われる。
pub fn build_http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("nox-voice/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("HTTP クライアントを構築できません: {e}"))
}

/// テスト用の日本語音声を読む (`tests/assets/japanese-sample.wav`)。
///
/// Windows の SAPI 音声で合成した 16kHz mono。中身が決まっているので、
/// トーン信号と違って「ハルシネーションでも合格」にならない。
#[cfg(test)]
pub fn japanese_sample_wav() -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("assets")
        .join("japanese-sample.wav");
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("音声アセットを読めません ({}): {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_test_server::{CannedResponse, TestServer};

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            // テストを速く終わらせるため本番より短くする。
            .timeout(Duration::from_millis(700))
            .build()
            .expect("テスト用クライアント")
    }

    fn stt(server: &TestServer, key: &str) -> GroqStt {
        GroqStt::new(
            client(),
            server.url(),
            "whisper-large-v3",
            Secret::new(key),
        )
    }

    // --- 応答パース (純関数) ---

    #[test]
    fn parses_plain_json_transcription() {
        let text = parse_transcription(r#"{"text":"  こんにちは世界  "}"#).expect("読める");
        assert_eq!(text, "こんにちは世界");
    }

    #[test]
    fn parses_verbose_json_transcription() {
        let body = r#"{"task":"transcribe","language":"japanese","duration":1.5,
            "text":"テストです","segments":[{"id":0,"text":"テストです"}]}"#;
        assert_eq!(parse_transcription(body).expect("読める"), "テストです");
    }

    #[test]
    fn empty_transcription_is_its_own_error() {
        assert_eq!(parse_transcription(r#"{"text":"   "}"#), Err(SttError::Empty));
    }

    #[test]
    fn malformed_json_is_a_decode_error() {
        assert!(matches!(
            parse_transcription("not json"),
            Err(SttError::Decode(_))
        ));
    }

    #[test]
    fn missing_text_field_is_a_decode_error() {
        assert!(matches!(
            parse_transcription(r#"{"foo":1}"#),
            Err(SttError::Decode(_))
        ));
    }

    #[test]
    fn error_shaped_200_response_is_rejected() {
        let body = r#"{"error":{"message":"invalid model","type":"invalid_request_error"}}"#;
        assert!(matches!(
            parse_transcription(body),
            Err(SttError::Decode(_))
        ));
    }

    // --- 反復ループの判定 (純関数) ---

    #[test]
    fn the_observed_loop_is_detected() {
        // 2026-11-21 の実機の転写と同じ形。ログの「STT 完了: 22 文字」と一致させる
        // (発話そのものは残さないので、前半は置き換えてある)。
        let observed = "今日の会議はははは早早早早早早早早早早早早早";
        assert_eq!(observed.chars().count(), 22);
        assert_eq!(find_repetition_loop(observed), Some((1, 13)));
    }

    #[test]
    fn a_looping_sentence_is_detected() {
        let looped = format!("今日の会議は{}", "ご視聴ありがとうございました。".repeat(4));
        assert_eq!(find_repetition_loop(&looped), Some((15, 4)));
    }

    #[test]
    fn a_looping_short_word_is_detected() {
        assert!(find_repetition_loop(&"そうですね、".repeat(5)).is_some());
        assert!(find_repetition_loop(&"あ".repeat(MIN_SINGLE_CHAR_REPEATS)).is_some());
    }

    #[test]
    fn ordinary_japanese_with_short_repeats_is_not_a_loop() {
        for text in [
            "こんにちは、今日はいい天気ですね。",
            "ははは、それは面白いですね",
            "えーーー、ちょっと待ってください",
            "ワンワンワンワンと犬が鳴いた",
            "はい、はい、はい、はい、分かりました",
            "人々は時々ここへ来る",
        ] {
            assert_eq!(find_repetition_loop(text), None, "誤検知: {text}");
        }
        // 境界: 1 文字の単位は MIN_SINGLE_CHAR_REPEATS 未満なら通す。
        assert_eq!(find_repetition_loop(&"あ".repeat(MIN_SINGLE_CHAR_REPEATS - 1)), None);
    }

    #[test]
    fn digits_and_symbols_are_never_a_loop() {
        for text in [
            "予算は100000000円です",
            "えっと…………そうですね",
            "すごい!!!!!!!!!!",
            "",
        ] {
            assert_eq!(find_repetition_loop(text), None, "誤検知: {text}");
        }
    }

    #[test]
    fn long_error_bodies_are_truncated_at_char_boundaries() {
        let long = "あ".repeat(MAX_ERROR_BODY + 50);
        let truncated = truncate_body(&long);
        assert_eq!(truncated.chars().count(), MAX_ERROR_BODY + 1); // + 省略記号
        assert!(truncated.ends_with('…'));
    }

    // --- HTTP 経路 (ローカルのテストサーバ) ---

    #[test]
    fn missing_key_fails_before_any_request() {
        let server = TestServer::start(vec![CannedResponse::ok(r#"{"text":"呼ばれないはず"}"#)]);
        let result = stt(&server, "").transcribe(&TranscribeRequest::new(b"RIFFfake", "ja"));
        assert_eq!(result, Err(SttError::MissingApiKey));
        assert_eq!(server.request_count(), 0, "キー無しで送信してしまった");
    }

    #[test]
    fn sends_key_in_header_and_never_in_the_url() {
        let server = TestServer::start(vec![CannedResponse::ok(r#"{"text":"ok"}"#)]);
        stt(&server, "gsk_secret_key")
            .transcribe(&TranscribeRequest::new(b"RIFFfake", "ja"))
            .expect("成功する");

        let requests = server.requests();
        let req = requests.first().expect("1 件受信している");
        assert_eq!(req.method, "POST");
        assert_eq!(
            req.header("authorization"),
            Some("Bearer gsk_secret_key"),
            "Authorization ヘッダで渡していない"
        );
        assert!(
            !req.path.contains("gsk_secret_key"),
            "URL にキーが載っている: {}",
            req.path
        );
    }

    #[test]
    fn sends_model_language_and_wav_as_multipart() {
        let server = TestServer::start(vec![CannedResponse::ok(r#"{"text":"ok"}"#)]);
        stt(&server, "k").transcribe(&TranscribeRequest::new(b"RIFFWAVEDATA", "ja")).expect("成功する");

        let requests = server.requests();
        let req = requests.first().expect("1 件受信している");
        let content_type = req.header("content-type").unwrap_or_default();
        assert!(
            content_type.starts_with("multipart/form-data"),
            "multipart で送っていない: {content_type}"
        );
        let body = req.body_lossy();
        assert!(body.contains("whisper-large-v3"), "model が無い");
        assert!(body.contains("name=\"language\""), "language が無い");
        assert!(body.contains("RIFFWAVEDATA"), "WAV 本体が無い");
        assert!(body.contains("audio.wav"), "ファイル名が無い");
    }

    #[test]
    fn the_prompt_is_sent_when_present() {
        let server = TestServer::start(vec![CannedResponse::ok(r#"{"text":"ok"}"#)]);
        let mut request = TranscribeRequest::new(b"RIFF", "ja");
        request.prompt = Some("nox-voice、Tauri");
        stt(&server, "k").transcribe(&request).expect("成功する");

        let requests = server.requests();
        let body = requests.first().expect("1 件").body_lossy();
        assert!(body.contains("name=\"prompt\""), "prompt を送っていない");
        assert!(body.contains("nox-voice、Tauri"), "辞書が載っていない");
    }

    #[test]
    fn a_blank_prompt_is_omitted() {
        let server = TestServer::start(vec![CannedResponse::ok(r#"{"text":"ok"}"#)]);
        let mut request = TranscribeRequest::new(b"RIFF", "ja");
        request.prompt = Some("   ");
        stt(&server, "k").transcribe(&request).expect("成功する");

        let requests = server.requests();
        let body = requests.first().expect("1 件").body_lossy();
        assert!(!body.contains("name=\"prompt\""), "空の prompt を送った");
    }

    #[test]
    fn blank_language_is_omitted_for_auto_detection() {
        let server = TestServer::start(vec![CannedResponse::ok(r#"{"text":"ok"}"#)]);
        stt(&server, "k").transcribe(&TranscribeRequest::new(b"RIFF", "  ")).expect("成功する");

        let requests = server.requests();
        let body = requests.first().expect("1 件").body_lossy();
        assert!(!body.contains("name=\"language\""), "空の language を送った");
    }

    #[test]
    fn unauthorized_is_not_retried() {
        let server = TestServer::start(vec![
            CannedResponse::status(401, r#"{"error":{"message":"bad key"}}"#),
            CannedResponse::ok(r#"{"text":"届かないはず"}"#),
        ]);
        let result = stt(&server, "wrong").transcribe(&TranscribeRequest::new(b"RIFF", "ja"));
        assert!(matches!(result, Err(SttError::Unauthorized(_))), "{result:?}");
        assert_eq!(server.request_count(), 1, "認証エラーで再試行してしまった");
    }

    #[test]
    fn rate_limit_is_retried_once_and_can_succeed() {
        let server = TestServer::start(vec![
            CannedResponse::status(429, r#"{"error":{"message":"slow down"}}"#),
            CannedResponse::ok(r#"{"text":"二回目で成功"}"#),
        ]);
        let transcript = stt(&server, "k")
            .transcribe(&TranscribeRequest::new(b"RIFF", "ja"))
            .expect("再試行で成功する");
        assert_eq!(transcript.text, "二回目で成功");
        assert_eq!(server.request_count(), 2);
    }

    #[test]
    fn server_error_is_retried_once_then_gives_up() {
        let server = TestServer::start(vec![
            CannedResponse::status(500, "boom"),
            CannedResponse::status(500, "boom again"),
            CannedResponse::ok(r#"{"text":"三回目は無い"}"#),
        ]);
        let result = stt(&server, "k").transcribe(&TranscribeRequest::new(b"RIFF", "ja"));
        assert!(matches!(result, Err(SttError::Server { status: 500, .. })), "{result:?}");
        assert_eq!(server.request_count(), 2, "再試行は 1 回だけのはず");
    }

    #[test]
    fn timeout_is_reported_as_timeout() {
        // クライアントのタイムアウトは 700ms。両方遅らせて再試行も落とす。
        let server = TestServer::start(vec![
            CannedResponse::slow(Duration::from_millis(1_500)),
            CannedResponse::slow(Duration::from_millis(1_500)),
        ]);
        let result = stt(&server, "k").transcribe(&TranscribeRequest::new(b"RIFF", "ja"));
        assert_eq!(result, Err(SttError::Timeout));
    }

    #[test]
    fn unreachable_endpoint_fails_without_panicking_and_is_retryable() {
        // いったん bind して即 close したポート = 誰も listen していない。
        // (固定のポート番号だと OS/FW によって「拒否」ではなく「無応答」に
        //  なることがあり、Network か Timeout かは環境依存になる。
        //  重要なのは種別ではなく「落ちない」「再試行対象になる」ことなので
        //  そちらを検証する)
        let port = {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("ポートを確保できる");
            listener.local_addr().expect("アドレス").port()
        };
        let stt = GroqStt::new(
            client(),
            format!("http://127.0.0.1:{port}/v1/audio/transcriptions"),
            "whisper-large-v3",
            Secret::new("k"),
        );
        let err = stt.transcribe(&TranscribeRequest::new(b"RIFF", "ja")).expect_err("到達できないので失敗する");
        assert!(
            matches!(err, SttError::Network(_) | SttError::Timeout),
            "想定外の失敗種別: {err:?}"
        );
        assert!(is_retryable(&err), "到達不能は再試行対象のはず: {err:?}");
    }

    #[test]
    fn error_messages_never_contain_the_api_key() {
        let server = TestServer::start(vec![
            CannedResponse::status(401, "nope"),
            CannedResponse::ok("{}"),
        ]);
        let key = "gsk_this_must_not_leak";
        let err = stt(&server, key)
            .transcribe(&TranscribeRequest::new(b"RIFF", "ja"))
            .expect_err("失敗する");
        let rendered = format!("{err} / {err:?}");
        assert!(!rendered.contains(key), "エラー文にキーが漏れている: {rendered}");
    }

    /// 実 API の**疎通だけ**を見る。440Hz のトーンなので中身は問わない。
    ///
    /// これが通っても「日本語がちゃんと転写できる」証明にはならない
    /// (ハルシネーションでも合格してしまう)。内容の確認は
    /// [`live_groq_transcribes_real_japanese`] の方で行う。
    ///
    /// 実行: `cargo test -- --ignored --nocapture live_groq_transcription`
    #[test]
    #[ignore = "実 API を呼ぶ。GROQ_API_KEY が必要"]
    fn live_groq_transcription() {
        let Ok(key) = std::env::var("GROQ_API_KEY") else {
            println!("GROQ_API_KEY が無いのでスキップします");
            return;
        };
        let wav = crate::audio::encode_wav_for_test(&silence_with_tone(), 16_000);
        let stt = GroqStt::new(
            build_http_client().expect("クライアント"),
            crate::config::DEFAULT_GROQ_ENDPOINT,
            crate::config::DEFAULT_STT_MODEL,
            Secret::new(key),
        );
        match stt.transcribe(&TranscribeRequest::new(&wav, "ja")) {
            Ok(t) => println!("転写結果: {:?}", t.text),
            // 無音に近い音声なので Empty は想定内。疎通の確認が目的。
            Err(SttError::Empty) => println!("空の転写 (無音のため想定内)"),
            Err(e) => panic!("実 API 疎通に失敗: {e}"),
        }
    }

    /// 実際の日本語音声が**内容として**転写できることを確認する。
    ///
    /// 音声は `tests/assets/japanese-sample.wav`
    /// (Windows の SAPI 音声「Haruka」で合成した 16kHz mono、
    /// 内容は「えーと、明日の会議の資料を準備してください。」)。
    /// 合成音声なので発話の癖は本物と違うが、**中身が決まっている**ことが
    /// 要点 — トーン信号ではハルシネーションでも合格してしまう。
    ///
    /// 実行: `cargo test -- --ignored --nocapture live_groq_transcribes_real_japanese`
    #[test]
    #[ignore = "実 API を呼ぶ。GROQ_API_KEY が必要"]
    fn live_groq_transcribes_real_japanese() {
        let Ok(key) = std::env::var("GROQ_API_KEY") else {
            println!("GROQ_API_KEY が無いのでスキップします");
            return;
        };
        let wav = super::japanese_sample_wav();
        let stt = GroqStt::new(
            build_http_client().expect("クライアント"),
            crate::config::DEFAULT_GROQ_ENDPOINT,
            crate::config::DEFAULT_STT_MODEL,
            Secret::new(key),
        );

        let transcript = stt.transcribe(&TranscribeRequest::new(&wav, "ja")).expect("日本語音声を転写できる");
        println!("生転写: {:?}", transcript.text);

        // 表記ゆれ (「明日」/「あした」など) はモデル任せなので、
        // 落ちにくく、かつハルシネーションは弾ける語で確認する。
        for expected in ["会議", "資料"] {
            assert!(
                transcript.text.contains(expected),
                "転写に「{expected}」が含まれない (ハルシネーションの可能性): {:?}",
                transcript.text
            );
        }
    }

    #[test]
    fn japanese_sample_asset_is_a_16k_mono_wav() {
        // アセットが壊れた/差し替えられたことに live テストより先に気づく。
        // (ネットワーク不要なので通常のテストとして常時走らせる)
        let wav = super::japanese_sample_wav();
        assert_eq!(&wav[0..4], b"RIFF");
        let reader = hound::WavReader::new(std::io::Cursor::new(wav)).expect("WAV として読める");
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().bits_per_sample, 16);
        // 数秒あること (無音ファイルに差し替わっていないか)。
        assert!(reader.duration() > 16_000, "音声が短すぎる");
    }

    #[cfg(test)]
    fn silence_with_tone() -> Vec<f32> {
        (0..16_000)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 16_000.0).sin() as f32 * 0.2
            })
            .collect()
    }
}
