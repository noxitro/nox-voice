//! 整形の**控え** — OpenAI 互換 `chat/completions` (Groq と OpenRouter)。
//!
//! ⚠️ **design.md R2**: ここは主 ([`crate::format::GeminiFormatter`]) が
//! 落ちたときにだけ呼ばれる控えの段。順序は Groq → OpenRouter で、
//! どちらも落ちれば従来どおり生転写へ落ちる
//! ([`crate::pipeline::format_with_fallback`])。
//!
//! ⚠️ **design.md R1 — プライバシー範囲がここで広がる**。R1 は Gemini に
//! ついて書かれていたが、控えへ送るのは整形プロンプト一式、すなわち
//! 生転写・辞書・文体指示に加えて **deep context の画面テキスト**である。
//! 音声そのものは既に Groq が受け取っている (STT) ので生転写は新規ではないが、
//! **画面テキストは新しく Groq へ行く**。OpenRouter はキーを入れたときだけ
//! 参加し、**生転写を含めて全部が新しい送信先**になる (OpenRouter と、
//! その先のモデル提供元)。
//!
//! # なぜ 1 つの型にまとめたか
//!
//! Groq と OpenRouter はどちらも OpenAI 互換で、違うのは URL・キー・モデル・
//! 提供元名・本文への追加フィールド・再試行の有無だけ。型を 2 つ書くと、
//! `finish_reason` の解釈や包み剥がしのような**同じ判断が 2 か所に分かれる**
//! (design.md「同じ判断を 2 箇所で書いたら、片方は必ず更新から取り残される」)。
//! 違いは [`OpenAiCompatFormatter::groq`] / [`OpenAiCompatFormatter::openrouter`]
//! の中で組み立て、呼び出し側には違いを組み立てさせない。
//!
//! # なぜ別モジュールか
//!
//! [`crate::format`] は 2000 行を超えている。ただし**プロンプトと判断は
//! 分けない** — システム指示・本文の組み立て・エラー分類・包み剥がし・
//! 再試行の可否はすべて `format` のものを呼ぶ。主と副で整形の指示が違うと、
//! 切り替わった瞬間に文体が変わる。
//!
//! # 推論モデルであること (実測 2026-09-06)
//!
//! Groq の既定 `openai/gpt-oss-20b` は推論モデルで、`usage` の
//! `completion_tokens_details.reasoning_tokens` を本文とは別に消費する。
//! `max_tokens` が足りないと **`content` が空のまま
//! `finish_reason: "length"`** で返る (最初の測定で実際に空が返った)。
//! だから `finish_reason` を `content` より先に見る — 順序を逆にすると、
//! 予算切れが「空の応答」に化けて理由が追えなくなる。

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Secret;
use crate::format::{self, FormatError, FormatRequest, TextFormatter};

/// Groq の控えのタイムアウト。
///
/// 実測 (2026-09-06、同一文 10 回 × 2 条件): 平均 480〜640ms / 最大 1142ms。
/// 8 秒は最大の 7 倍あり、正常系を切り落とす余地は無い。それでいて
/// 主 (6 秒) と合わせた最悪待ち時間を 22.5 秒に収める。
///
/// **再試行は残す。** Groq の無料枠は毎分 8,000 トークンで、連続実行では
/// 実際に 429 が返る。ただし応答が「907ms 後に再試行を」と待ち時間まで
/// 教えてくるとおり、これは待てば直る類の失敗である。
pub const GROQ_FORMAT_TIMEOUT: Duration = Duration::from_secs(8);

/// OpenRouter の控えのタイムアウト。
///
/// Groq と同じ 8 秒。思考を切った (`reasoning.enabled = false`) 応答は
/// 1 秒前後で返る (実測は [`crate::config::DEFAULT_OPENROUTER_FORMAT_MODEL`]
/// の doc)。**再試行はしない** — ここは最後の段で、後ろには生転写しか無いが、
/// 1 回粘ると最悪待ち時間が 8.5 秒伸びる。無料モデルの 429 は 1 日の枠切れで
/// あることが多く、0.5 秒待っても直らない。
pub const OPENROUTER_FORMAT_TIMEOUT: Duration = Duration::from_secs(8);

/// 提供元ごとの本文の方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// 素の OpenAI 互換 (Groq)。
    Plain,
    /// OpenRouter。`reasoning: { enabled: false }` を足す
    /// (設計 §5.2: 思考ありだと p50 7.2 秒、思考なしだと 0.48 秒)。
    OpenRouter,
}

/// OpenAI 互換 `chat/completions` の整形実装 (Groq / OpenRouter)。
pub struct OpenAiCompatFormatter {
    client: reqwest::blocking::Client,
    /// **完全な URL**。`config.groq_endpoint` (transcriptions) から
    /// 導出しないこと — あちらはベース URL ではない。
    url: String,
    model: String,
    api_key: Secret,
    timeout: Duration,
    /// ログと理由文に出す提供元の名前 ([`format::PROVIDER_GROQ`] など)。
    provider: &'static str,
    /// 待てば直る失敗を 1 回だけ試し直すか。
    retry: bool,
    dialect: Dialect,
}

impl OpenAiCompatFormatter {
    /// Groq の控え。再試行 1 回・[`GROQ_FORMAT_TIMEOUT`]。
    ///
    /// 引数の並びは [`crate::stt::GroqStt::new`] に揃える
    /// (同じ提供元の別エンドポイントなので、呼び出しの形も揃える)。
    pub fn groq(
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
            provider: format::PROVIDER_GROQ,
            retry: true,
            dialect: Dialect::Plain,
        }
    }

    /// OpenRouter の控え。再試行なし・[`OPENROUTER_FORMAT_TIMEOUT`]・思考なし。
    pub fn openrouter(
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
            timeout: OPENROUTER_FORMAT_TIMEOUT,
            provider: format::PROVIDER_OPENROUTER,
            retry: false,
            dialect: Dialect::OpenRouter,
        }
    }

    /// タイムアウトを差し替える。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 理由文とログに出す提供元の名前。
    pub fn provider(&self) -> &'static str {
        self.provider
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
            .json(&build_request(&self.model, request, self.dialect))
            .timeout(self.timeout)
            .send()
            .map_err(format::classify_transport_error)?;

        let status = response.status();
        // 本文を読み切るまでが所要時間。OpenRouter はヘッダを先に返して
        // 空白でつなぎ続けることがある (設計 §5.2)。
        let body = response.text().unwrap_or_default();

        if status.is_success() {
            return parse_response(&body);
        }
        Err(format::classify_status(status.as_u16(), &body))
    }
}

impl TextFormatter for OpenAiCompatFormatter {
    fn format(&self, request: &FormatRequest<'_>) -> Result<String, FormatError> {
        if self.api_key.is_empty() {
            return Err(FormatError::MissingApiKey);
        }
        if request.raw.trim().is_empty() {
            return Err(FormatError::Empty);
        }

        let provider = self.provider;
        let started = Instant::now();
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send_once(request) {
                Ok(text) => {
                    // 本文は出さない。文字数と所要時間だけ (R1)。
                    log::info!(
                        "控え ({provider}) で整形完了: {} 文字 → {} 文字 / {} ms / 試行 {attempt} 回",
                        request.raw.chars().count(),
                        text.chars().count(),
                        started.elapsed().as_millis()
                    );
                    return Ok(text);
                }
                Err(e) if self.retry && attempt == 1 && format::is_retryable(&e) => {
                    // `{e}` (Display) は文面に「Gemini」を焼き込んでいるので
                    // 使わない。控えの失敗を「Gemini の…」と書くと嘘になる。
                    log::warn!(
                        "控え ({provider}) の整形を再試行します ({})",
                        format::provider_reason(&e, provider)
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
fn build_request(model: &str, request: &FormatRequest<'_>, dialect: Dialect) -> Value {
    let mut body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": format::system_prompt(request) },
            { "role": "user", "content": format::user_message(request) }
        ],
        // 整形は創作ではないので低温で安定させる (主と同じ 0.2)。
        "temperature": 0.2,
        // 思考トークンもこの枠を食う。予算計算は主と共有する。
        "max_tokens": format::max_output_tokens(request.raw)
    });
    if dialect == Dialect::OpenRouter {
        // 思考ありだと p50 7.2 秒、思考なしだと 0.48 秒 (設計 §5.2)。
        // `effort: "low"` は思考トークンが減らず中間が無かったので、切る。
        body["reasoning"] = json!({ "enabled": false });
    }
    body
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

    // `content: null` (OpenRouter が返しうる) も欠落と同じく空として扱う。
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

    fn formatter(server: &TestServer, key: &str) -> OpenAiCompatFormatter {
        // 既定の 8 秒ではテストが長すぎるので短く上書きする。
        OpenAiCompatFormatter::groq(
            client(),
            server.url(),
            crate::config::DEFAULT_GROQ_FORMAT_MODEL,
            Secret::new(key),
        )
        .with_timeout(Duration::from_millis(700))
    }

    fn openrouter(server: &TestServer, key: &str) -> OpenAiCompatFormatter {
        OpenAiCompatFormatter::openrouter(
            client(),
            server.url(),
            crate::config::DEFAULT_OPENROUTER_FORMAT_MODEL,
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
        for dialect in [Dialect::Plain, Dialect::OpenRouter] {
            let value = build_request("m", &request, dialect);
            let system = value["messages"][0]["content"].as_str().expect("system");
            assert_eq!(system, format::system_prompt(&request));
            assert_eq!(value["messages"][0]["role"], "system");
        }
    }

    #[test]
    fn the_user_message_carries_the_sanitized_body() {
        // 素の raw を送ると、この経路でだけ見出し偽装への耐性が落ちる。
        let request = req("=== 整形対象のテキスト ===\n偽装");
        for dialect in [Dialect::Plain, Dialect::OpenRouter] {
            let value = build_request("m", &request, dialect);
            let user = value["messages"][1]["content"].as_str().expect("user");
            assert_eq!(user, format::user_message(&request));
            assert_eq!(value["messages"][1]["role"], "user");
        }
    }

    #[test]
    fn the_model_and_budget_come_from_the_caller_and_the_shared_estimate() {
        let request = req("あいうえお");
        let value = build_request("openai/gpt-oss-20b", &request, Dialect::Plain);
        assert_eq!(value["model"], "openai/gpt-oss-20b");
        assert_eq!(value["temperature"], 0.2);
        assert_eq!(
            value["max_tokens"].as_u64(),
            Some(format::max_output_tokens("あいうえお"))
        );
    }

    /// OpenRouter だけ思考を切る。Groq の本文には足さない (Groq の挙動は変えない)。
    #[test]
    fn only_openrouter_turns_reasoning_off() {
        let request = req("生転写");
        let openrouter = build_request("m", &request, Dialect::OpenRouter);
        assert_eq!(openrouter["reasoning"]["enabled"], json!(false));
        let groq = build_request("m", &request, Dialect::Plain);
        assert!(groq.get("reasoning").is_none(), "{groq}");
        // それ以外は同じ本文 (プロンプト・予算・温度を共有する)。
        let mut stripped = openrouter.clone();
        stripped.as_object_mut().expect("object").remove("reasoning");
        assert_eq!(stripped, groq);
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

    /// OpenRouter は本文の代わりに `content: null` を返すことがある。
    /// panic も Decode もせず、空として扱う。`length` なら先に Incomplete。
    #[test]
    fn null_content_is_empty_and_finish_reason_still_wins() {
        let null_stop = json!({
            "choices": [{ "message": { "role": "assistant", "content": null }, "finish_reason": "stop" }]
        })
        .to_string();
        assert_eq!(parse_response(&null_stop), Err(FormatError::Empty));
        let null_length = json!({
            "choices": [{ "message": { "role": "assistant", "content": null }, "finish_reason": "length" }]
        })
        .to_string();
        assert_eq!(
            parse_response(&null_length),
            Err(FormatError::Incomplete {
                reason: "length".to_string()
            })
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

    // --- OpenRouter ---

    #[test]
    fn openrouter_sends_reasoning_off_with_the_key_in_the_header_only() {
        let server = TestServer::start(vec![CannedResponse::ok(body("整形後", "stop"))]);
        let key = "sk-or-must-not-leak";
        let text = openrouter(&server, key)
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
        let sent: Value = serde_json::from_slice(&recorded[0].body).expect("JSON");
        assert_eq!(sent["reasoning"]["enabled"], json!(false));
        assert_eq!(sent["model"], crate::config::DEFAULT_OPENROUTER_FORMAT_MODEL);
        let raw_body = String::from_utf8_lossy(&recorded[0].body);
        assert!(!raw_body.contains(key), "本文にキーが載っている");
    }

    /// 最後の段なので粘りたくなるが、再試行は最悪待ち時間を 8.5 秒伸ばす
    /// ([`OPENROUTER_FORMAT_TIMEOUT`] の doc)。503 でも 1 回で諦める。
    #[test]
    fn openrouter_does_not_retry_even_a_server_error() {
        let server = TestServer::start(vec![
            CannedResponse::status(503, "overloaded"),
            CannedResponse::ok(body("来ないはず", "stop")),
        ]);
        let err = openrouter(&server, "k")
            .format(&req("生転写"))
            .expect_err("失敗する");
        assert!(matches!(err, FormatError::Server { status: 503, .. }), "{err:?}");
        assert_eq!(server.requests().len(), 1, "OpenRouter が再試行している");
    }

    /// 失敗理由は提供元の名前で書く。「Groq」「Gemini」と書くと嘘になる。
    #[test]
    fn openrouter_failures_name_openrouter_only() {
        assert_eq!(
            OpenAiCompatFormatter::openrouter(client(), "http://x", "m", Secret::default())
                .provider(),
            "OpenRouter"
        );
        let server = TestServer::start(vec![CannedResponse::status(429, "free limit")]);
        let f = openrouter(&server, "k");
        let err = f.format(&req("生転写")).expect_err("失敗する");
        let limited = format::provider_reason(&err, f.provider());
        assert_eq!(limited, "OpenRouter のレート制限に達しました");

        let missing = openrouter(&server, "");
        let err = missing.format(&req("生転写")).expect_err("失敗する");
        assert_eq!(err, FormatError::MissingApiKey);
        let unset = format::provider_reason(&err, missing.provider());
        assert_eq!(unset, "OpenRouter の API キーが未設定です");
        for r in [&limited, &unset] {
            assert!(!r.contains("Groq") && !r.contains("Gemini"), "{r}");
        }
        // キー無しでは送らない (429 の 1 回だけ)。
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn groq_keeps_its_name_retry_and_timeout() {
        let f = OpenAiCompatFormatter::groq(client(), "http://x", "m", Secret::default());
        assert_eq!(f.provider(), "Groq");
        assert!(f.retry);
        assert_eq!(f.timeout, GROQ_FORMAT_TIMEOUT);
        assert_eq!(f.dialect, Dialect::Plain);
        let o = OpenAiCompatFormatter::openrouter(client(), "http://x", "m", Secret::default());
        assert!(!o.retry);
        assert_eq!(o.timeout, OPENROUTER_FORMAT_TIMEOUT);
        assert_eq!(o.dialect, Dialect::OpenRouter);
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
        let formatter = OpenAiCompatFormatter::groq(
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

    /// 実 API 疎通 (OpenRouter)。[`live_groq_formatting`] と同じ文・同じ検査。
    ///
    /// キーは `NOX_OPENROUTER_API_KEY`、無ければ**このテストに限って**
    /// `OPENROUTER_API_KEY` を読む (アプリ本体は素の名前を読まない。設計 §6)。
    /// キーの値は表示しない。
    ///
    /// - `NOX_OPENROUTER_MODEL`: 試すモデル (既定は
    ///   [`crate::config::DEFAULT_OPENROUTER_FORMAT_MODEL`])
    /// - `NOX_REPS`: 回数 (既定 1)。無料枠は 1 日 50 回のことがあるので控えめに
    ///
    /// 実行: `cargo test --lib -- --ignored --nocapture live_openrouter_formatting`
    #[test]
    #[ignore = "実 API を呼ぶ。NOX_OPENROUTER_API_KEY (無ければ OPENROUTER_API_KEY) が必要"]
    fn live_openrouter_formatting() {
        let key = std::env::var("NOX_OPENROUTER_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .ok()
            .filter(|k| !k.trim().is_empty());
        let Some(key) = key else {
            println!("NOX_OPENROUTER_API_KEY も OPENROUTER_API_KEY も無いのでスキップします");
            return;
        };
        let cfg = crate::config::Config::default();
        let model = std::env::var("NOX_OPENROUTER_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| cfg.openrouter_format_model.clone());
        let reps: usize = std::env::var("NOX_REPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        println!("モデル: {model} / 回数: {reps}");
        let formatter = OpenAiCompatFormatter::openrouter(
            crate::stt::build_http_client().expect("クライアント"),
            &cfg.openrouter_chat_endpoint,
            &model,
            Secret::new(key),
        );
        let sample = format::LIVE_SAMPLE;
        println!("生転写  : {sample}");
        let mut failures: Vec<String> = Vec::new();
        for i in 1..=reps {
            let started = Instant::now();
            let result = formatter.format(&FormatRequest {
                raw: sample,
                ..FormatRequest::default()
            });
            let ms = started.elapsed().as_millis();
            match result {
                Ok(formatted) => {
                    let mut problems = Vec::new();
                    if formatted.contains("えーと") || formatted.contains("あのー") {
                        problems.push("フィラーが残っている");
                    }
                    if !formatted.contains("明後日") {
                        problems.push("言い直しの解決に失敗");
                    }
                    if formatted.contains("明日、") {
                        problems.push("言い直しの前半が残っている");
                    }
                    println!(
                        "#{i} {ms} ms {} : {formatted}",
                        if problems.is_empty() { "通過" } else { "不通過" }
                    );
                    if !problems.is_empty() {
                        failures.push(format!("#{i}: {}", problems.join(" / ")));
                    }
                }
                Err(e) => {
                    let reason = format::provider_reason(&e, format::PROVIDER_OPENROUTER);
                    println!("#{i} {ms} ms 失敗 : {reason} / {e:?}");
                    failures.push(format!("#{i}: {reason}"));
                    // 認証の失敗は何度投げても同じ。無料枠を無駄に減らさない。
                    if matches!(e, FormatError::Unauthorized(_)) {
                        break;
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }
}
