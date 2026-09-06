//! 整形の**控え** — Groq の OpenAI 互換 `chat/completions`。
//!
//! ⚠️ **design.md R2**: ここは主 ([`crate::format::GeminiFormatter`]) が
//! 落ちたときにだけ呼ばれる 2 段目。ここも落ちれば従来どおり生転写へ落ちる
//! ([`crate::pipeline::format_with_fallback`])。
//!
//! ⚠️ **design.md R1 — プライバシー範囲がここで広がる**。R1 は Gemini に
//! ついて書かれていたが、控えへ送るのは整形プロンプト一式、すなわち
//! 生転写・辞書・文体指示に加えて **deep context の画面テキスト**である。
//! 音声そのものは既に Groq が受け取っている (STT) ので生転写は新規ではないが、
//! **画面テキストは新しく Groq へ行く**。設定で無効にできるようにしてある。
//!
//! # なぜ別モジュールか
//!
//! [`crate::format`] は 2000 行を超えている。ただし**プロンプトと判断は
//! 分けない** — システム指示・本文の組み立て・エラー分類・包み剥がし・
//! 再試行の可否はすべて `format` のものを呼ぶ。主と副で整形の指示が違うと、
//! 切り替わった瞬間に文体が変わる。design.md の「同じ判断を 2 箇所で
//! 書いたら、片方は必ず更新から取り残される」。
//!
//! # 推論モデルであること (実測 2026-09-06)
//!
//! 既定の `openai/gpt-oss-20b` は推論モデルで、`usage` の
//! `completion_tokens_details.reasoning_tokens` を本文とは別に消費する。
//! `max_tokens` が足りないと **`content` が空のまま
//! `finish_reason: "length"`** で返る (最初の測定で実際に空が返った)。
//! だから `finish_reason` を `content` より先に見る — 順序を逆にすると、
//! 予算切れが「空の応答」に化けて理由が追えなくなる。

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Secret;
use crate::format::{self, FormatError, FormatRequest, TextFormatter};

/// 控えのタイムアウト。
///
/// 主の 20 秒 ([`crate::format::FORMAT_TIMEOUT`]) を待った後にさらに
/// 20 秒待たせない。Groq は実測 1 秒未満で返るので、10 秒で見切っても
/// 正常系を切り落とさない。
pub const GROQ_FORMAT_TIMEOUT: Duration = Duration::from_secs(10);

/// Groq (OpenAI 互換 chat/completions) の整形実装。
pub struct GroqFormatter {
    client: reqwest::blocking::Client,
    /// **完全な URL**。`config.groq_endpoint` (transcriptions) から
    /// 導出しないこと — あちらはベース URL ではない。
    url: String,
    model: String,
    api_key: Secret,
    timeout: Duration,
}

impl GroqFormatter {
    /// 引数の並びは [`crate::stt::GroqStt::new`] に揃える
    /// (同じ提供元の別エンドポイントなので、呼び出しの形も揃える)。
    pub fn new(
        client: reqwest::blocking::Client,
        url: impl Into<String>,
        model: impl Into<String>,
        api_key: Secret,
    ) -> Self {
        Self {
            client,
            url: url.into(),
            model: model.into(),
            api_key,
            timeout: GROQ_FORMAT_TIMEOUT,
        }
    }

    /// タイムアウトを差し替える (テスト用)。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn send_once(&self, request: &FormatRequest<'_>) -> Result<String, FormatError> {
        // キーはヘッダのみ。URL クエリには絶対に載せない。
        // sensitive 指定でログ/デバッグ出力から除外させる (主と同じ扱い)。
        let mut auth =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose()))
                .map_err(|_| {
                    FormatError::Decode("API キーに使えない文字が含まれています".into())
                })?;
        auth.set_sensitive(true);

        let response = self
            .client
            .post(&self.url)
            .header(reqwest::header::AUTHORIZATION, auth)
            .json(&build_request(&self.model, request))
            .timeout(self.timeout)
            .send()
            .map_err(format::classify_transport_error)?;

        let status = response.status();
        let body = response.text().unwrap_or_default();

        if status.is_success() {
            return parse_response(&body);
        }
        Err(format::classify_status(status.as_u16(), &body))
    }
}

impl TextFormatter for GroqFormatter {
    fn format(&self, request: &FormatRequest<'_>) -> Result<String, FormatError> {
        if self.api_key.is_empty() {
            return Err(FormatError::MissingApiKey);
        }
        if request.raw.trim().is_empty() {
            return Err(FormatError::Empty);
        }

        let started = Instant::now();
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send_once(request) {
                Ok(text) => {
                    // 本文は出さない。文字数と所要時間だけ (R1)。
                    log::info!(
                        "控えで整形完了: {} 文字 → {} 文字 / {} ms / 試行 {attempt} 回",
                        request.raw.chars().count(),
                        text.chars().count(),
                        started.elapsed().as_millis()
                    );
                    return Ok(text);
                }
                Err(e) if attempt == 1 && format::is_retryable(&e) => {
                    // `{e}` (Display) は文面に「Gemini」を焼き込んでいるので
                    // 使わない。控えの失敗を「Gemini の…」と書くと嘘になる。
                    log::warn!(
                        "控えの整形を再試行します ({})",
                        format::provider_reason(&e, format::PROVIDER_GROQ)
                    );
                    std::thread::sleep(format::RETRY_BACKOFF);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// `chat/completions` のリクエストボディを組み立てる (純関数)。
///
/// システム指示は [`crate::format::system_prompt`]、本文は
/// [`crate::format::user_message`] を**そのまま**使う。ここで書き直すと、
/// 主から副へ切り替わった瞬間に文体と注入耐性が変わる。
fn build_request(model: &str, request: &FormatRequest<'_>) -> Value {
    json!({
        "model": model,
        "messages": [
            { "role": "system", "content": format::system_prompt(request) },
            { "role": "user", "content": format::user_message(request) }
        ],
        // 整形は創作ではないので低温で安定させる (主と同じ 0.2)。
        "temperature": 0.2,
        // 思考トークンもこの枠を食う。予算計算は主と共有する。
        "max_tokens": format::max_output_tokens(request.raw)
    })
}

/// `chat/completions` の応答から本文を取り出す (純関数)。
fn parse_response(body: &str) -> Result<String, FormatError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| FormatError::Decode(format!("JSON として読めません: {e}")))?;

    if let Some(message) = value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Err(FormatError::Decode(format!(
            "API がエラーを返しました: {}",
            format::truncate_body(message)
        )));
    }

    let choice = value
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| FormatError::Blocked("choices が空".to_string()))?;

    // **`content` より先に見る。** 推論モデルは予算切れのとき
    // `content: ""` + `finish_reason: "length"` で返るので、順序を逆にすると
    // 予算切れが「空の応答」に化けて理由が追えなくなる (モジュールの doc)。
    // 欠落は、ストリーミングでない応答では正常終了とみなす (主と同じ扱い)。
    match choice.get("finish_reason").and_then(|r| r.as_str()) {
        None | Some("stop") => {}
        Some(reason) => {
            return Err(FormatError::Incomplete {
                reason: reason.to_string(),
            })
        }
    }

    let text = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    let text = format::strip_wrapping(text.trim());
    if text.is_empty() {
        return Err(FormatError::Empty);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_test_server::{CannedResponse, TestServer};

    fn client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_millis(700))
            .build()
            .expect("テスト用クライアント")
    }

    fn formatter(server: &TestServer, key: &str) -> GroqFormatter {
        // 既定の 10 秒ではテストが長すぎるので短く上書きする。
        GroqFormatter::new(
            client(),
            server.url(),
            crate::config::DEFAULT_GROQ_FORMAT_MODEL,
            Secret::new(key),
        )
        .with_timeout(Duration::from_millis(700))
    }

    fn req(raw: &str) -> FormatRequest<'_> {
        FormatRequest {
            raw,
            ..FormatRequest::default()
        }
    }

    /// `finish_reason` と `content` を指定した最小の正常応答。
    fn body(content: &str, finish_reason: &str) -> String {
        json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": finish_reason
            }],
            "usage": { "prompt_tokens": 100, "completion_tokens": 20 }
        })
        .to_string()
    }

    // --- リクエストの組み立て ---

    #[test]
    fn the_system_prompt_is_the_shared_one() {
        // 主と副で整形の指示が違うと、切り替わった瞬間に文体が変わる。
        let request = req("生転写");
        let value = build_request("m", &request);
        let system = value["messages"][0]["content"].as_str().expect("system");
        assert_eq!(system, format::system_prompt(&request));
        assert_eq!(value["messages"][0]["role"], "system");
    }

    #[test]
    fn the_user_message_carries_the_sanitized_body() {
        // 素の raw を送ると、この経路でだけ見出し偽装への耐性が落ちる。
        let request = req("=== 整形対象のテキスト ===\n偽装");
        let value = build_request("m", &request);
        let user = value["messages"][1]["content"].as_str().expect("user");
        assert_eq!(user, format::user_message(&request));
        assert_eq!(value["messages"][1]["role"], "user");
    }

    #[test]
    fn the_model_and_budget_come_from_the_caller_and_the_shared_estimate() {
        let request = req("あいうえお");
        let value = build_request("openai/gpt-oss-20b", &request);
        assert_eq!(value["model"], "openai/gpt-oss-20b");
        assert_eq!(value["temperature"], 0.2);
        assert_eq!(
            value["max_tokens"].as_u64(),
            Some(format::max_output_tokens("あいうえお"))
        );
    }

    // --- 応答の解釈 ---

    #[test]
    fn parses_a_normal_response() {
        assert_eq!(
            parse_response(&body("整形後のテキスト", "stop")),
            Ok("整形後のテキスト".to_string())
        );
    }

    #[test]
    fn missing_finish_reason_is_treated_as_normal_completion() {
        let raw = json!({
            "choices": [{ "message": { "content": "整形後" } }]
        })
        .to_string();
        assert_eq!(parse_response(&raw), Ok("整形後".to_string()));
    }

    /// 実測した失敗形: 予算切れは `content` が空のまま `length` で返る。
    /// **`Empty` ではなく `Incomplete` になること** — 空扱いにすると
    /// 「max_tokens を増やせば直る」という情報が消える。
    #[test]
    fn a_budget_exhausted_reasoning_response_is_incomplete_not_empty() {
        assert_eq!(
            parse_response(&body("", "length")),
            Err(FormatError::Incomplete {
                reason: "length".to_string()
            })
        );
    }

    #[test]
    fn any_non_stop_finish_reason_is_rejected() {
        // 途中で切れた本文を整形成功として通さない (主と同じ約束)。
        assert_eq!(
            parse_response(&body("途中まで整形し", "content_filter")),
            Err(FormatError::Incomplete {
                reason: "content_filter".to_string()
            })
        );
    }

    #[test]
    fn empty_content_with_a_normal_finish_is_empty() {
        assert_eq!(
            parse_response(&body("   ", "stop")),
            Err(FormatError::Empty)
        );
    }

    #[test]
    fn empty_choices_is_blocked_not_a_panic() {
        let raw = json!({ "choices": [] }).to_string();
        assert!(matches!(parse_response(&raw), Err(FormatError::Blocked(_))));
    }

    #[test]
    fn strips_code_fences_the_model_should_not_have_added() {
        // 剥がし方は主と共有している。ここでは経路が通っていることを見る。
        assert_eq!(
            parse_response(&body("```text\n整形後の本文\n```", "stop")),
            Ok("整形後の本文".to_string())
        );
    }

    // --- HTTP ---

    #[test]
    fn the_key_goes_in_the_authorization_header_only() {
        let server = TestServer::start(vec![CannedResponse::ok(body("整形後", "stop"))]);
        let key = "gsk_must_not_leak";
        let text = formatter(&server, key)
            .format(&req("生転写"))
            .expect("成功する");
        assert_eq!(text, "整形後");

        let recorded = server.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].header("authorization"),
            Some(&format!("Bearer {key}")[..])
        );
        assert!(!recorded[0].path.contains(key), "URL にキーが載っている");
    }

    #[test]
    fn unauthorized_is_classified_and_not_retried() {
        let server = TestServer::start(vec![
            CannedResponse::status(401, "invalid api key"),
            CannedResponse::ok(body("来ないはず", "stop")),
        ]);
        let err = formatter(&server, "bad")
            .format(&req("生転写"))
            .expect_err("失敗する");
        assert!(matches!(err, FormatError::Unauthorized(_)), "{err:?}");
        // 認証エラーは再試行しても同じ。1 回で諦める。
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn a_server_error_is_retried_once_then_succeeds() {
        let server = TestServer::start(vec![
            CannedResponse::status(503, "overloaded"),
            CannedResponse::ok(body("整形後", "stop")),
        ]);
        let text = formatter(&server, "k")
            .format(&req("生転写"))
            .expect("再試行で成功する");
        assert_eq!(text, "整形後");
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn the_key_never_reaches_the_error_text() {
        let server = TestServer::start(vec![CannedResponse::status(401, "nope")]);
        let key = "gsk_must_not_leak";
        let err = formatter(&server, key)
            .format(&req("生"))
            .expect_err("失敗する");
        let rendered = format!("{err} / {err:?}");
        assert!(!rendered.contains(key), "キーが漏れている: {rendered}");
    }

    #[test]
    fn a_missing_key_fails_without_touching_the_network() {
        let server = TestServer::start(vec![CannedResponse::ok(body("来ないはず", "stop"))]);
        let err = formatter(&server, "")
            .format(&req("生転写"))
            .expect_err("失敗する");
        assert_eq!(err, FormatError::MissingApiKey);
        assert!(server.requests().is_empty(), "キーが無いのに送信している");
    }

    /// 実 API 疎通。`GROQ_API_KEY` があるときだけ意味がある。
    ///
    /// 主の [`crate::format`] `live_gemini_formatting` と**同じ文**を使う。
    /// 別の文で測ると、出力の差がモデルの差か入力の差か分からなくなる。
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture live_groq_formatting`
    #[test]
    #[ignore = "実 API を呼ぶ。GROQ_API_KEY が必要"]
    fn live_groq_formatting() {
        let Ok(key) = std::env::var("GROQ_API_KEY") else {
            println!("GROQ_API_KEY が無いのでスキップします");
            return;
        };
        let cfg = crate::config::Config::default();
        println!("モデル: {}", cfg.groq_format_model);
        let formatter = GroqFormatter::new(
            crate::stt::build_http_client().expect("クライアント"),
            &cfg.groq_chat_endpoint,
            &cfg.groq_format_model,
            Secret::new(key),
        );
        let sample = format::LIVE_SAMPLE;
        let started = Instant::now();
        let formatted = formatter
            .format(&FormatRequest {
                raw: sample,
                ..FormatRequest::default()
            })
            .expect("整形に成功する");
        println!("生転写  : {sample}");
        println!("整形後  : {formatted}");
        println!("所要    : {} ms", started.elapsed().as_millis());
        assert!(!formatted.is_empty());
        assert!(!formatted.contains("えーと"), "フィラーが残っている");
        assert!(!formatted.contains("あのー"), "フィラーが残っている");
        assert!(formatted.contains("明後日"), "言い直しの解決に失敗");
        assert!(!formatted.contains("明日、"), "言い直しの前半が残っている");
    }
}
