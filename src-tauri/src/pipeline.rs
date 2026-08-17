//! WAV → STT → 整形 のパイプラインと、**R2 劣化モード**。
//!
//! # R2: 整形フォールバック (design.md)
//!
//! Gemini の失敗・タイムアウト・レート制限時は、生転写をそのまま採用して
//! 続行する。整形が落ちても音声入力そのものは成立させる、という設計判断。
//! どちらが採用されたかは [`FormatOutcome`] に残り、UI と (M4 の) 履歴で
//! 参照できる — R5 (生転写の可視性) が成り立つのはこの記録があるため。
//!
//! **STT の失敗は劣化できない** (元になるテキストが無い)。この場合は
//! 上位が WAV を退避してユーザーへ通知する。
//!
//! HTTP に触れないトレイト越しの構成なので、実 API 無しで分岐を検証できる。

use std::time::Instant;

use serde::Serialize;

use crate::format::{FormatError, TextFormatter};
use crate::stt::{SpeechToText, SttError};

/// 整形の結末。どのテキストがなぜ採用されたかを保持する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormatOutcome {
    /// 整形に成功し、その結果を採用した。
    Formatted,
    /// 整形に失敗したので生転写を採用した (R2 劣化モード)。
    RawFallback { reason: String },
    /// 設定で整形が無効なので生転写を採用した。
    Disabled,
}

impl FormatOutcome {
    /// 劣化モードで動いたか (UI の注意表示に使う)。
    pub fn is_degraded(&self) -> bool {
        matches!(self, FormatOutcome::RawFallback { .. })
    }
}

/// パイプラインの成果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineResult {
    /// STT が返した生転写。R5 のためつねに保持する。
    pub raw_text: String,
    /// 実際に採用されたテキスト (整形後、または生転写)。
    pub text: String,
    pub outcome: FormatOutcome,
    pub stt_ms: u64,
    pub format_ms: u64,
}

/// WAV を転写し、必要なら整形する。
///
/// `formatter` が `None` (整形無効・キー未設定) なら生転写を採用する。
/// 整形が失敗しても [`Err`] にはせず、[`FormatOutcome::RawFallback`] で返す。
/// [`Err`] になるのは STT が失敗したときだけ。
pub fn run(
    wav: &[u8],
    language: &str,
    dictionary: &[String],
    stt: &dyn SpeechToText,
    formatter: Option<&dyn TextFormatter>,
) -> Result<PipelineResult, SttError> {
    let stt_started = Instant::now();
    let transcript = stt.transcribe(wav, language)?;
    let stt_ms = stt_started.elapsed().as_millis() as u64;
    let raw_text = transcript.text;

    let Some(formatter) = formatter else {
        return Ok(PipelineResult {
            text: raw_text.clone(),
            raw_text,
            outcome: FormatOutcome::Disabled,
            stt_ms,
            format_ms: 0,
        });
    };

    let format_started = Instant::now();
    let (text, outcome) = match formatter.format(&raw_text, dictionary) {
        Ok(formatted) => (formatted, FormatOutcome::Formatted),
        Err(e) => {
            // R2: ここで失敗を握り潰さず、理由つきで生転写へ落とす。
            log::warn!("整形に失敗したため生転写を採用します (劣化モード): {e}");
            (
                raw_text.clone(),
                FormatOutcome::RawFallback {
                    reason: degradation_reason(&e),
                },
            )
        }
    };
    let format_ms = format_started.elapsed().as_millis() as u64;

    Ok(PipelineResult {
        raw_text,
        text,
        outcome,
        stt_ms,
        format_ms,
    })
}

/// 劣化理由をユーザー向けの短い文にする。
///
/// 「何が起きたか」より「どうすればよいか」が伝わる粒度にする。
fn degradation_reason(error: &FormatError) -> String {
    match error {
        FormatError::MissingApiKey => "Gemini の API キーが未設定です".to_string(),
        FormatError::Unauthorized(_) => "Gemini の認証に失敗しました".to_string(),
        FormatError::RateLimited(_) => "Gemini のレート制限に達しました".to_string(),
        FormatError::Timeout => "Gemini の応答がタイムアウトしました".to_string(),
        FormatError::Network(_) => "Gemini へ接続できませんでした".to_string(),
        FormatError::Server { status, .. } => format!("Gemini のサーバエラー ({status})"),
        FormatError::Http { status, .. } => format!("Gemini がエラーを返しました ({status})"),
        FormatError::Blocked(reason) => format!("Gemini が応答を生成しませんでした ({reason})"),
        FormatError::Decode(_) => "Gemini の応答を解釈できませんでした".to_string(),
        FormatError::Empty => "Gemini の応答が空でした".to_string(),
        FormatError::Incomplete { reason } => {
            format!("Gemini の生成が途中で終わりました ({reason})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::Transcript;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedStt(Result<&'static str, SttError>);

    impl SpeechToText for FixedStt {
        fn transcribe(&self, _wav: &[u8], _language: &str) -> Result<Transcript, SttError> {
            match &self.0 {
                Ok(text) => Ok(Transcript {
                    text: (*text).to_string(),
                }),
                Err(e) => Err(e.clone()),
            }
        }
    }

    struct FixedFormatter {
        result: Result<String, FormatError>,
        calls: AtomicUsize,
        last_dictionary: std::sync::Mutex<Vec<String>>,
    }

    impl FixedFormatter {
        fn ok(text: &str) -> Self {
            Self {
                result: Ok(text.to_string()),
                calls: AtomicUsize::new(0),
                last_dictionary: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn err(e: FormatError) -> Self {
            Self {
                result: Err(e),
                calls: AtomicUsize::new(0),
                last_dictionary: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl TextFormatter for FixedFormatter {
        fn format(&self, _raw: &str, dictionary: &[String]) -> Result<String, FormatError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut slot) = self.last_dictionary.lock() {
                *slot = dictionary.to_vec();
            }
            self.result.clone()
        }
    }

    #[test]
    fn happy_path_uses_the_formatted_text() {
        let stt = FixedStt(Ok("えーと こんにちは"));
        let formatter = FixedFormatter::ok("こんにちは。");
        let result = run(b"wav", "ja", &[], &stt, Some(&formatter)).expect("成功する");

        assert_eq!(result.raw_text, "えーと こんにちは");
        assert_eq!(result.text, "こんにちは。");
        assert_eq!(result.outcome, FormatOutcome::Formatted);
        assert!(!result.outcome.is_degraded());
    }

    #[test]
    fn raw_transcript_is_always_kept_for_r5() {
        let stt = FixedStt(Ok("生の転写"));
        let formatter = FixedFormatter::ok("整形後のテキスト");
        let result = run(b"wav", "ja", &[], &stt, Some(&formatter)).expect("成功する");
        // 整形後を採用しても、生転写は照合用に残る (R5)。
        assert_eq!(result.raw_text, "生の転写");
        assert_ne!(result.raw_text, result.text);
    }

    #[test]
    fn dictionary_reaches_the_formatter() {
        let stt = FixedStt(Ok("のっくすぼいす"));
        let formatter = FixedFormatter::ok("nox-voice");
        let dictionary = vec!["nox-voice".to_string()];
        run(b"wav", "ja", &dictionary, &stt, Some(&formatter)).expect("成功する");
        assert_eq!(
            *formatter.last_dictionary.lock().expect("ロック"),
            dictionary
        );
    }

    // --- R2 劣化モード ---

    #[test]
    fn format_failure_falls_back_to_the_raw_transcript() {
        let stt = FixedStt(Ok("生転写のテキスト"));
        let formatter = FixedFormatter::err(FormatError::RateLimited("quota".to_string()));
        let result = run(b"wav", "ja", &[], &stt, Some(&formatter)).expect("STT は成功している");

        assert_eq!(result.text, "生転写のテキスト", "生転写へ落ちていない");
        assert_eq!(result.raw_text, "生転写のテキスト");
        assert!(result.outcome.is_degraded());
        match result.outcome {
            FormatOutcome::RawFallback { reason } => {
                assert!(reason.contains("レート制限"), "理由が伝わらない: {reason}");
            }
            other => panic!("劣化モードになっていない: {other:?}"),
        }
    }

    #[test]
    fn every_format_error_degrades_rather_than_failing() {
        let errors = [
            FormatError::MissingApiKey,
            FormatError::Unauthorized("x".into()),
            FormatError::RateLimited("x".into()),
            FormatError::Server { status: 503, body: "x".into() },
            FormatError::Http { status: 400, body: "x".into() },
            FormatError::Timeout,
            FormatError::Network("x".into()),
            FormatError::Decode("x".into()),
            FormatError::Blocked("SAFETY".into()),
            FormatError::Empty,
            FormatError::Incomplete { reason: "MAX_TOKENS".into() },
        ];
        for error in errors {
            let stt = FixedStt(Ok("生転写"));
            let formatter = FixedFormatter::err(error.clone());
            let result = run(b"wav", "ja", &[], &stt, Some(&formatter))
                .unwrap_or_else(|e| panic!("{error:?} で Err になった: {e}"));
            assert_eq!(result.text, "生転写", "{error:?} で生転写に落ちていない");
            assert!(result.outcome.is_degraded(), "{error:?}");
            // 理由は空にしない (UI に出すため)。
            match result.outcome {
                FormatOutcome::RawFallback { reason } => assert!(!reason.is_empty()),
                other => panic!("{other:?}"),
            }
        }
    }

    /// M-1 回帰: 途中で切れた整形結果は採用せず、生転写へ落とす。
    #[test]
    fn truncated_formatting_falls_back_to_the_raw_transcript() {
        let stt = FixedStt(Ok("長い発話の生転写がここに入る"));
        let formatter = FixedFormatter::err(FormatError::Incomplete {
            reason: "MAX_TOKENS".to_string(),
        });
        let result = run(b"wav", "ja", &[], &stt, Some(&formatter)).expect("STT は成功");

        assert_eq!(
            result.text, "長い発話の生転写がここに入る",
            "尻切れの整形結果を採用してしまっている"
        );
        assert!(result.outcome.is_degraded());
        match result.outcome {
            FormatOutcome::RawFallback { reason } => {
                assert!(reason.contains("途中で終わり"), "{reason}");
                assert!(reason.contains("MAX_TOKENS"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn formatting_disabled_skips_the_formatter_entirely() {
        let stt = FixedStt(Ok("生転写のみ"));
        let result = run(b"wav", "ja", &[], &stt, None).expect("成功する");
        assert_eq!(result.text, "生転写のみ");
        assert_eq!(result.outcome, FormatOutcome::Disabled);
        assert!(!result.outcome.is_degraded(), "無効化は劣化ではない");
        assert_eq!(result.format_ms, 0);
    }

    #[test]
    fn missing_gemini_key_degrades_with_an_actionable_reason() {
        let stt = FixedStt(Ok("生転写"));
        let formatter = FixedFormatter::err(FormatError::MissingApiKey);
        let result = run(b"wav", "ja", &[], &stt, Some(&formatter)).expect("STT は成功");
        match result.outcome {
            FormatOutcome::RawFallback { reason } => {
                assert!(reason.contains("API キー"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    // --- STT の失敗は劣化できない ---

    #[test]
    fn stt_failure_propagates_and_skips_formatting() {
        let stt = FixedStt(Err(SttError::MissingApiKey));
        let formatter = FixedFormatter::ok("呼ばれないはず");
        let result = run(b"wav", "ja", &[], &stt, Some(&formatter));
        assert_eq!(result, Err(SttError::MissingApiKey));
        assert_eq!(formatter.calls(), 0, "STT 失敗後に整形を呼んでいる");
    }

    #[test]
    fn stt_timeout_propagates() {
        let stt = FixedStt(Err(SttError::Timeout));
        assert_eq!(run(b"wav", "ja", &[], &stt, None), Err(SttError::Timeout));
    }

    #[test]
    fn empty_transcription_propagates_as_stt_error() {
        let stt = FixedStt(Err(SttError::Empty));
        let formatter = FixedFormatter::ok("呼ばれないはず");
        assert_eq!(
            run(b"wav", "ja", &[], &stt, Some(&formatter)),
            Err(SttError::Empty)
        );
        assert_eq!(formatter.calls(), 0);
    }

    // --- 実 API を使う通しテスト ---
    //
    // どちらも環境変数のキーだけを見る。**ユーザーの設定ファイルには触れない**
    // (読みもしないので、設定を書き換えたり漏らしたりしない)。
    //
    //   cargo test -- --ignored --nocapture live_pipeline

    #[cfg(test)]
    fn live_clients(
        gemini_key: &str,
    ) -> Option<(crate::stt::GroqStt, crate::format::GeminiFormatter)> {
        let groq_key = std::env::var("GROQ_API_KEY").ok()?;
        let http = crate::stt::build_http_client().expect("クライアント");
        let cfg = crate::config::Config::default();
        let stt = crate::stt::GroqStt::new(
            http.clone(),
            &cfg.groq_endpoint,
            &cfg.stt_model,
            crate::config::Secret::new(groq_key),
        );
        let formatter = crate::format::GeminiFormatter::new(
            http,
            cfg.gemini_url(),
            crate::config::Secret::new(gemini_key),
        );
        Some((stt, formatter))
    }

    /// F2: 実際の日本語音声を STT → 整形まで通す。
    ///
    /// フィラー (「えーと」) が落ちて内容が残ることまで見る。
    /// 各段の単体テストが通っていても、**繋いだときに壊れていない**保証は
    /// これでしか取れない。
    #[test]
    #[ignore = "実 API を呼ぶ。GROQ_API_KEY と GEMINI_API_KEY が必要"]
    fn live_pipeline_transcribes_and_formats_japanese() {
        let Ok(gemini_key) = std::env::var("GEMINI_API_KEY") else {
            println!("GEMINI_API_KEY が無いのでスキップします");
            return;
        };
        let Some((stt, formatter)) = live_clients(&gemini_key) else {
            println!("GROQ_API_KEY が無いのでスキップします");
            return;
        };
        let wav = crate::stt::japanese_sample_wav();

        let result = run(&wav, "ja", &[], &stt, Some(&formatter)).expect("通しで成功する");
        println!("生転写: {:?}", result.raw_text);
        println!("整形後: {:?}", result.text);
        println!("結末  : {:?}", result.outcome);
        println!("STT {} ms / 整形 {} ms", result.stt_ms, result.format_ms);

        assert_eq!(result.outcome, FormatOutcome::Formatted, "整形されていない");
        assert!(result.text.contains("会議"), "内容が失われた: {:?}", result.text);
        assert!(result.text.contains("資料"), "内容が失われた: {:?}", result.text);
        assert!(
            !result.text.contains("えーと"),
            "フィラーが残っている: {:?}",
            result.text
        );
        // R5: 生転写は整形後で上書きされず残る。
        assert!(!result.raw_text.is_empty());
    }

    /// F3: R2 劣化モードを実 API で確認する。
    ///
    /// STT は本物のキーで成功させ、**整形だけ無効なキーで失敗させる**。
    /// 生転写が採用され、理由つきの `RawFallback` になること。
    #[test]
    #[ignore = "実 API を呼ぶ。GROQ_API_KEY が必要"]
    fn live_pipeline_degrades_when_formatting_is_rejected() {
        // わざと通らないキーを渡す。ユーザーの設定は読みも書きもしない。
        let Some((stt, formatter)) = live_clients("invalid-key-for-degraded-mode-test") else {
            println!("GROQ_API_KEY が無いのでスキップします");
            return;
        };
        let wav = crate::stt::japanese_sample_wav();

        let result = run(&wav, "ja", &[], &stt, Some(&formatter)).expect("STT は成功する");
        println!("生転写: {:?}", result.raw_text);
        println!("採用  : {:?}", result.text);
        println!("結末  : {:?}", result.outcome);

        assert!(
            result.outcome.is_degraded(),
            "整形が失敗したのに劣化モードになっていない: {:?}",
            result.outcome
        );
        assert_eq!(result.text, result.raw_text, "生転写が採用されていない");
        assert!(!result.text.is_empty(), "採用テキストが空");
        match result.outcome {
            FormatOutcome::RawFallback { reason } => {
                assert!(!reason.is_empty(), "劣化の理由が空");
                println!("劣化理由: {reason}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn outcome_serializes_with_a_discriminant_for_the_ui() {
        let json = serde_json::to_string(&FormatOutcome::Formatted).expect("シリアライズ");
        assert_eq!(json, r#"{"kind":"formatted"}"#);
        let json = serde_json::to_string(&FormatOutcome::RawFallback {
            reason: "理由".to_string(),
        })
        .expect("シリアライズ");
        assert_eq!(json, r#"{"kind":"raw_fallback","reason":"理由"}"#);
    }
}
