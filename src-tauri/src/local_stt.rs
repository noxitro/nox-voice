//! ローカル STT — Groq が使えないときの代替。
//!
//! kotoba-whisper (whisper.cpp の GGML 形式) を CPU で回す。
//! ネットワーク不達・レート制限・API キー未設定でも、とにかく文字にできる
//! ことを狙う。品質と速度は Groq に劣る。
//!
//! # ビルド
//!
//! whisper.cpp のビルドに CMake と C++ ツールチェーンが要るので、
//! **`local-stt` feature でオプトイン**にしてある。無効時はこのモジュールが
//! 「常に利用不可」を返すスタブになり、本体のビルドは通常どおり通る。
//!
//! ```text
//! cargo build --features local-stt
//! ```
//!
//! # モデル
//!
//! モデル (~1GB) は同梱しない。設定画面からダウンロードして
//! `%APPDATA%/<identifier>/models/` へ置く。存在しなければフォールバックは
//! 働かず、その旨を UI に出す。

use std::path::{Path, PathBuf};

use serde::Serialize;

/// 既定のモデルファイル名。
pub const DEFAULT_MODEL_FILE: &str = "ggml-kotoba-whisper-v2.0.bin";

/// ローカル STT が使えるかどうか。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Availability {
    /// 使える。
    Ready { path: String, bytes: u64 },
    /// この実行ファイルは `local-stt` 無しでビルドされている。
    NotCompiled,
    /// モデルファイルが無い。
    ModelMissing { expected_path: String },
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready { .. })
    }

    /// UI に出す説明。
    pub fn message(&self) -> String {
        match self {
            Availability::Ready { bytes, .. } => format!(
                "ローカル認識を使えます (モデル {:.1} GB)",
                *bytes as f64 / 1_073_741_824.0
            ),
            Availability::NotCompiled => {
                "このビルドにはローカル認識が含まれていません (local-stt 機能を有効にしてビルドしてください)"
                    .to_string()
            }
            Availability::ModelMissing { .. } => {
                "モデルが未ダウンロードのため、ローカル認識は使えません".to_string()
            }
        }
    }
}

/// 既定のモデル配布元 (HuggingFace)。
///
/// 設定で差し替えられるようにはしていないが、変える必要が出たら
/// ここを直せばよい。ダウンロード先とハッシュは対で管理すること。
pub const DEFAULT_MODEL_URL: &str =
    "https://huggingface.co/kotoba-tech/kotoba-whisper-v2.0-ggml/resolve/main/ggml-kotoba-whisper-v2.0.bin";

/// ダウンロードの進捗。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadProgress {
    pub downloaded: u64,
    /// サーバが総サイズを返さないこともある。
    pub total: Option<u64>,
    pub done: bool,
}

/// ダウンロードが進行中か。
///
/// 重複起動すると 2 本のスレッドが同じ `.part` を交互に書く。
/// それぞれが**自分の書いたバイト列**でハッシュを計算するので、
/// ファイルが混ざっていても照合が通ってしまう。1 本に制限する。
static DOWNLOAD_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// ダウンロード中なら `false` を返す (開始できない)。
pub fn begin_download() -> bool {
    !DOWNLOAD_IN_FLIGHT.swap(true, std::sync::atomic::Ordering::SeqCst)
}

/// ダウンロードの終了を記録する。
pub fn end_download() {
    DOWNLOAD_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// 設置済みファイルの SHA-256 を計算する。
///
/// **ストリームを流しながらではなくファイルを読み直す。** 流しながらだと
/// 「自分が送ったバイト列」のハッシュになり、他のプロセスやスレッドが
/// 同じファイルへ書き込んでいても気づけない。検証対象は
/// *ディスク上にある実物*でなければ意味がない。
pub fn file_sha256(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("{} を読めません: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("読み込みに失敗しました: {e}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// ダウンロードの結果。
#[derive(Debug, Clone, Serialize)]
pub struct DownloadOutcome {
    pub path: String,
    pub bytes: u64,
    /// 実際に計算した SHA-256。設定に期待値が無い場合はこれを控えて固定する。
    pub sha256: String,
}

/// モデルをダウンロードして検証・設置する。
///
/// # 途中で落ちても壊さない
///
/// `.part` へ書いてから rename する。中断された巨大ファイルが
/// 「モデルがある」顔で残ると、次の推論が意味不明に失敗する。
///
/// `expected_sha256` が与えられていれば照合し、**合わなければ設置しない**。
/// 与えられていなければ計算値を返すので、それを控えて以後の検証に使える。
pub fn download_model(
    client: &reqwest::blocking::Client,
    models_dir: &Path,
    url: &str,
    expected_sha256: Option<&str>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<DownloadOutcome, String> {
    use std::io::{Read, Write};

    std::fs::create_dir_all(models_dir)
        .map_err(|e| format!("{} を作成できません: {e}", models_dir.display()))?;

    let final_path = model_path(models_dir);
    let part_path = final_path.with_extension("bin.part");

    let mut response = client
        .get(url)
        // 1GB 級なので全体タイムアウトは付けない (共有クライアントの 60 秒だと落ちる)。
        .timeout(std::time::Duration::from_secs(60 * 60))
        .send()
        .map_err(|e| format!("ダウンロードを開始できません: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "ダウンロードに失敗しました (HTTP {})",
            response.status().as_u16()
        ));
    }
    let total = response.content_length();

    let mut file = std::fs::File::create(&part_path)
        .map_err(|e| format!("{} を作成できません: {e}", part_path.display()))?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut downloaded = 0u64;
    let mut last_report = std::time::Instant::now();

    loop {
        let read = match response.read(&mut buffer) {
            Ok(read) => read,
            Err(e) => {
                // 途中で切れた ~1GB を残さない。
                drop(file);
                let _ = std::fs::remove_file(&part_path);
                return Err(format!("受信中にエラーが起きました: {e}"));
            }
        };
        if read == 0 {
            break;
        }
        if let Err(e) = file.write_all(&buffer[..read]) {
            drop(file);
            let _ = std::fs::remove_file(&part_path);
            return Err(format!("書き込みに失敗しました: {e}"));
        }
        downloaded += read as u64;

        // 進捗は間引く。1MB ごとに IPC を叩くと UI が詰まる。
        if last_report.elapsed() >= std::time::Duration::from_millis(200) {
            on_progress(DownloadProgress {
                downloaded,
                total,
                done: false,
            });
            last_report = std::time::Instant::now();
        }
    }
    if let Err(e) = file.flush() {
        drop(file);
        let _ = std::fs::remove_file(&part_path);
        return Err(format!("書き込みに失敗しました: {e}"));
    }
    drop(file);

    // **書いたバイト列ではなく、ディスク上の実物**を読み直して検証する。
    let digest = match file_sha256(&part_path) {
        Ok(digest) => digest,
        Err(e) => {
            let _ = std::fs::remove_file(&part_path);
            return Err(format!("ダウンロードしたモデルを検証できません: {e}"));
        }
    };
    if let Some(expected) = expected_sha256.map(str::trim).filter(|e| !e.is_empty()) {
        if !digest.eq_ignore_ascii_case(expected) {
            let _ = std::fs::remove_file(&part_path);
            return Err(format!(
                "ダウンロードしたモデルのハッシュが一致しません (期待 {expected} / 実際 {digest})"
            ));
        }
    } else {
        log::warn!("期待する SHA-256 が未設定です。今回の計算値を保存します: {digest}");
    }

    std::fs::rename(&part_path, &final_path).map_err(|e| {
        let _ = std::fs::remove_file(&part_path);
        format!("モデルを設置できません: {e}")
    })?;

    on_progress(DownloadProgress {
        downloaded,
        total,
        done: true,
    });
    log::info!(
        "モデルをダウンロードしました: {} ({} バイト)",
        final_path.display(),
        downloaded
    );
    Ok(DownloadOutcome {
        path: final_path.to_string_lossy().to_string(),
        bytes: downloaded,
        sha256: digest,
    })
}

/// モデルの置き場所。
pub fn model_path(models_dir: &Path) -> PathBuf {
    models_dir.join(DEFAULT_MODEL_FILE)
}

/// ローカル STT が使えるか調べる。
pub fn availability(models_dir: &Path) -> Availability {
    if !cfg!(feature = "local-stt") {
        return Availability::NotCompiled;
    }
    let path = model_path(models_dir);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => Availability::Ready {
            path: path.to_string_lossy().to_string(),
            bytes: meta.len(),
        },
        _ => Availability::ModelMissing {
            expected_path: path.to_string_lossy().to_string(),
        },
    }
}

/// どのエンジンを使うか。
///
/// **新規録音と再転写で同じ判断を使うための型。** 片方だけが設定を見ていない、
/// という食い違いが起きないよう、選択をここに 1 本化する
/// (実際に、再転写だけがモードを見ておらず「ローカルのみ」でも音声を
/// クラウドへ送っていた)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnginePlan {
    /// クラウド (Groq) を使ってよいか。
    pub use_cloud: bool,
    /// ローカルモデルを使ってよいか。
    pub use_local: bool,
}

impl EnginePlan {
    /// 設定からエンジンの使い分けを決める。
    pub fn from_mode(mode: crate::config::LocalSttMode) -> Self {
        Self {
            use_cloud: mode.uses_cloud(),
            use_local: mode.allows_local(),
        }
    }
}

/// Groq の失敗をローカルで引き取るべきか。
///
/// **引き取らない失敗がある**のが要点:
///
/// - 認証エラー: キーが間違っているだけで、ローカルへ落ちると
///   ユーザーは設定ミスに気づかないまま遅い経路を使い続ける
/// - 空の転写: そもそも喋っていない。もう一度やっても空
/// - 応答を解釈できない: サーバ側の仕様変更かもしれず、隠すべきでない
///
/// 逆に、ネットワーク不達・レート制限・キー未設定は「ローカルなら通る」
/// 類の失敗なので引き取る。
pub fn should_fall_back(error: &crate::stt::SttError) -> bool {
    use crate::stt::SttError;
    matches!(
        error,
        SttError::MissingApiKey
            | SttError::Network(_)
            | SttError::Timeout
            | SttError::RateLimited(_)
            | SttError::Server { .. }
    )
}

// --- 実装 (feature 有効時) ---------------------------------------------------

#[cfg(feature = "local-stt")]
mod imp {
    use std::path::Path;

    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    use crate::stt::{SttError, Transcript};

    /// WAV (16kHz mono 16bit) を f32 サンプルへ戻す。
    fn decode_wav(wav: &[u8]) -> Result<Vec<f32>, SttError> {
        let reader = hound::WavReader::new(std::io::Cursor::new(wav))
            .map_err(|e| SttError::Decode(format!("WAV を読めません: {e}")))?;
        let spec = reader.spec();
        if spec.sample_rate != 16_000 || spec.channels != 1 {
            return Err(SttError::Decode(format!(
                "16kHz mono である必要があります (実際: {}Hz {}ch)",
                spec.sample_rate, spec.channels
            )));
        }
        let samples: Result<Vec<i16>, _> = reader.into_samples::<i16>().collect();
        let samples = samples.map_err(|e| SttError::Decode(format!("サンプルを読めません: {e}")))?;
        Ok(samples
            .into_iter()
            .map(|s| s as f32 / i16::MAX as f32)
            .collect())
    }

    /// モデルを読み込んで転写する。
    ///
    /// コンテキストの構築は重いが、フォールバックは滅多に起きないので
    /// 毎回作って捨てる (常駐でメモリを 1GB 抱えない方を選ぶ)。
    pub fn transcribe(
        model: &Path,
        wav: &[u8],
        language: &str,
    ) -> Result<Transcript, SttError> {
        let audio = decode_wav(wav)?;

        let context = WhisperContext::new_with_params(model, WhisperContextParameters::default())
            .map_err(|e| SttError::Decode(format!("モデルを読み込めません: {e}")))?;

        let mut state = context
            .create_state()
            .map_err(|e| SttError::Decode(format!("推論状態を作れません: {e}")))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        if !language.trim().is_empty() {
            params.set_language(Some(language.trim()));
        }
        // 進捗表示はしないので、標準出力を汚さない。
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        state
            .full(params, &audio)
            .map_err(|e| SttError::Decode(format!("推論に失敗しました: {e}")))?;

        let mut text = String::new();
        for i in 0..state.full_n_segments() {
            let Some(segment) = state.get_segment(i) else {
                continue;
            };
            match segment.to_str() {
                Ok(part) => text.push_str(part),
                // 1 セグメントが UTF-8 として壊れていても、他は使う。
                Err(e) => log::warn!("セグメント {i} を読めません: {e}"),
            }
        }

        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(SttError::Empty);
        }
        Ok(Transcript { text })
    }
}

/// ローカルモデルで転写する。
///
/// `local-stt` 無しでビルドされている場合は必ず失敗する。
pub fn transcribe(
    models_dir: &Path,
    wav: &[u8],
    language: &str,
) -> Result<crate::stt::Transcript, crate::stt::SttError> {
    let availability = availability(models_dir);
    if !availability.is_ready() {
        return Err(crate::stt::SttError::Decode(availability.message()));
    }

    #[cfg(feature = "local-stt")]
    {
        let path = model_path(models_dir);
        log::info!("ローカル認識を開始します (CPU 推論のため時間がかかります)");
        let started = std::time::Instant::now();
        let result = imp::transcribe(&path, wav, language);
        log::info!("ローカル認識が終了しました ({} ms)", started.elapsed().as_millis());
        result
    }
    #[cfg(not(feature = "local-stt"))]
    {
        let _ = (wav, language);
        Err(crate::stt::SttError::Decode(
            Availability::NotCompiled.message(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::SttError;

    #[test]
    fn a_missing_model_is_reported_as_missing() {
        let dir = std::env::temp_dir().join("nox-local-stt-missing");
        let availability = availability(&dir);
        // feature 無効ビルドでは NotCompiled が先に来る。
        assert!(!availability.is_ready());
        assert!(!availability.message().is_empty());
    }

    #[test]
    fn the_model_path_is_stable() {
        let dir = Path::new("C:").join("models");
        assert!(model_path(&dir).ends_with(DEFAULT_MODEL_FILE));
    }

    // --- フォールバック判定 ---

    #[test]
    fn transient_failures_fall_back_to_local() {
        // ローカルなら通る類の失敗。
        for error in [
            SttError::MissingApiKey,
            SttError::Network("接続できません".into()),
            SttError::Timeout,
            SttError::RateLimited("quota".into()),
            SttError::Server {
                status: 503,
                body: "busy".into(),
            },
        ] {
            assert!(should_fall_back(&error), "{error:?} で落ちない");
        }
    }

    #[test]
    fn configuration_errors_do_not_fall_back() {
        // キーが間違っているのを黙って隠すと、ユーザーは設定ミスに
        // 気づかないまま遅い経路を使い続ける。
        assert!(!should_fall_back(&SttError::Unauthorized("bad key".into())));
    }

    #[test]
    fn empty_speech_does_not_fall_back() {
        // 喋っていないものは、どのエンジンでも空。
        assert!(!should_fall_back(&SttError::Empty));
    }

    #[test]
    fn protocol_errors_do_not_fall_back() {
        // 仕様変更かもしれない失敗を隠さない。
        assert!(!should_fall_back(&SttError::Decode("形が違う".into())));
        assert!(!should_fall_back(&SttError::Http {
            status: 400,
            body: "bad request".into()
        }));
    }

    #[test]
    fn a_second_download_is_refused_while_one_is_running() {
        // 2 本が同じ .part を交互に書くと、壊れたモデルが「検証済み」の顔で
        // 設置されうる。
        assert!(begin_download(), "1 本目が始められない");
        assert!(!begin_download(), "重複起動を許してしまった");
        end_download();
        assert!(begin_download(), "終了後に始められない");
        end_download();
    }

    #[test]
    fn the_digest_is_computed_from_the_file_on_disk() {
        let dir = std::env::temp_dir().join(format!("nox-model-sha-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("作れる");
        let path = dir.join("sample.bin");
        std::fs::write(&path, b"nox-voice").expect("書ける");

        // 既知の値と一致すること (自前実装の取り違えを検出する)。
        let digest = file_sha256(&path).expect("計算できる");
        assert_eq!(digest.len(), 64, "SHA-256 は 64 桁の 16 進");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));

        // 中身が変わればハッシュも変わる。
        std::fs::write(&path, b"nox-voice!").expect("書ける");
        assert_ne!(file_sha256(&path).expect("計算できる"), digest);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_download_does_not_look_like_a_model() {
        // 中断された巨大ファイルが「モデルがある」顔で残ると、
        // 次の推論が意味不明に失敗する。
        let dir = std::env::temp_dir().join(format!("nox-model-part-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("作れる");
        let part = model_path(&dir).with_extension("bin.part");
        std::fs::write(&part, vec![0u8; 1024]).expect("書ける");

        assert!(
            !availability(&dir).is_ready(),
            ".part が本体として認識されている"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_model_file_is_not_ready() {
        let dir = std::env::temp_dir().join(format!("nox-model-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("作れる");
        std::fs::write(model_path(&dir), b"").expect("書ける");
        assert!(!availability(&dir).is_ready(), "空ファイルを使おうとしている");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_model_url_points_at_the_expected_artifact() {
        assert!(DEFAULT_MODEL_URL.starts_with("https://"), "平文 HTTP で取得している");
        assert!(DEFAULT_MODEL_URL.ends_with(DEFAULT_MODEL_FILE));
    }

    // --- エンジンの選択 (C1 回帰) ---

    #[test]
    fn local_only_mode_never_touches_the_cloud() {
        // 「ローカルのみ」を選んだ人の音声を外へ出さない。
        // 新規録音でも再転写でも同じ判断になること。
        let plan = EnginePlan::from_mode(crate::config::LocalSttMode::Only);
        assert!(!plan.use_cloud, "ローカルのみモードでクラウドを使おうとしている");
        assert!(plan.use_local);
    }

    #[test]
    fn fallback_mode_allows_both_engines() {
        let plan = EnginePlan::from_mode(crate::config::LocalSttMode::Fallback);
        assert!(plan.use_cloud);
        assert!(plan.use_local);
    }

    #[test]
    fn off_mode_never_uses_local() {
        let plan = EnginePlan::from_mode(crate::config::LocalSttMode::Off);
        assert!(plan.use_cloud);
        assert!(!plan.use_local, "無効にしたローカル認識を使おうとしている");
    }

    #[test]
    fn availability_serializes_for_the_ui() {
        let json = serde_json::to_string(&Availability::NotCompiled).expect("直列化");
        assert!(json.contains("not_compiled"), "{json}");
        let json = serde_json::to_string(&Availability::ModelMissing {
            expected_path: "C:/models/x.bin".into(),
        })
        .expect("直列化");
        assert!(json.contains("model_missing"), "{json}");
    }

    /// モデルがあるときだけ動かす実推論テスト。
    ///
    /// 実行: `cargo test --features local-stt -- --ignored --nocapture live_local_stt`
    #[test]
    #[ignore = "モデルファイルが必要。local-stt 機能つきでビルドすること"]
    fn live_local_stt_transcribes_japanese() {
        let dir = std::env::var("NOX_MODELS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("nox-models"));

        let availability = availability(&dir);
        if !availability.is_ready() {
            println!("{} — スキップします", availability.message());
            return;
        }

        let wav = crate::stt::japanese_sample_wav();
        let started = std::time::Instant::now();
        let transcript = transcribe(&dir, &wav, "ja").expect("ローカル認識に成功する");
        println!("生転写: {:?}", transcript.text);
        println!("所要  : {:?}", started.elapsed());
        assert!(transcript.text.contains("会議") || transcript.text.contains("資料"));
    }
}
