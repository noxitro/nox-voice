//! 録音セッションのデータモデルとアプリ状態機械。
//!
//! ここで定義する [`RecordingSession`] が M1 (録音) と M2 (STT) の境界インターフェース。
//! M2 は `wav_bytes` を Groq の Whisper へ送り、`target_hwnd` / `target_process` は
//! M3 の挿入時フォーカス照合 (設計 R7) でそのまま使う。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// アプリ全体の状態。フロントへは `nox://status` イベントで通知する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// 待機中。ホットキー押下を待っている。
    Idle,
    /// 録音中。
    Recording,
    /// 録音停止後の後処理中 (M2 以降で STT / 整形 / 挿入がここに入る)。
    Processing,
}

impl Status {
    /// トレイメニューに出す日本語ラベル。
    pub fn label(self) -> &'static str {
        match self {
            Status::Idle => "待機中",
            Status::Recording => "録音中",
            Status::Processing => "処理中",
        }
    }
}

/// 録音開始の瞬間に確定させた「挿入先」ウィンドウ。
///
/// 録音開始時点で採る点が重要で、録音中にユーザーが別ウィンドウへ移っても
/// 挿入先はここで固定される。M3 では挿入直前の `GetForegroundWindow` と
/// `hwnd` を照合し、不一致なら貼付を中止する (R7)。
#[derive(Debug, Clone, Serialize)]
pub struct TargetWindow {
    /// 前景ウィンドウハンドル。`HWND` は Rust 側では不透明値として持つ。
    pub hwnd: isize,
    /// ウィンドウを所有するプロセス ID。
    pub process_id: u32,
    /// 実行ファイル名 (例: `notepad.exe`)。取得失敗時は `"<unknown>"`。
    pub process_name: String,
    /// ウィンドウタイトル。取得失敗時は空文字。
    pub window_title: String,
}

impl TargetWindow {
    /// 前景ウィンドウが取得できなかった場合のプレースホルダ。
    ///
    /// 取得失敗で録音自体を止めはしない (音声は保全する / 設計 R4 の精神)。
    /// 挿入側が `hwnd == 0` を「照合対象なし」として扱う。
    pub fn unknown() -> Self {
        Self {
            hwnd: 0,
            process_id: 0,
            process_name: "<unknown>".to_string(),
            window_title: String::new(),
        }
    }

    /// 照合可能な前景情報が取れているか。
    pub fn is_known(&self) -> bool {
        self.hwnd != 0
    }
}

/// 1 回の PTT 録音の成果物。M2 の STT はこれを入力に取る。
#[derive(Debug, Clone)]
pub struct RecordingSession {
    /// 16 kHz / mono / 16bit PCM の WAV バイト列 (メモリ上。ファイルには落とさない)。
    pub wav_bytes: Vec<u8>,
    /// `wav_bytes` のサンプリングレート。現状は常に [`crate::audio::TARGET_SAMPLE_RATE`]。
    pub sample_rate: u32,
    /// 録音開始時の前景ウィンドウ。
    pub target: TargetWindow,
    /// 録音開始時刻。
    pub started_at: SystemTime,
    /// 録音長 (サンプル数から算出した実尺。壁時計ではない)。
    pub duration: Duration,
}

impl RecordingSession {
    /// 挿入先 HWND (R7 の照合用)。
    pub fn target_hwnd(&self) -> isize {
        self.target.hwnd
    }

    /// 挿入先プロセス名。
    pub fn target_process(&self) -> &str {
        &self.target.process_name
    }

    /// フロント/ログ向けの要約 (WAV 本体は含めない)。
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            wav_bytes: self.wav_bytes.len(),
            sample_rate: self.sample_rate,
            duration_ms: self.duration.as_millis() as u64,
            started_at_ms: self
                .started_at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            target_hwnd: self.target.hwnd,
            target_process: self.target.process_name.clone(),
            target_title: self.target.window_title.clone(),
        }
    }
}

/// フロントへ渡す録音結果の要約。WAV 本体は Rust 側に留める。
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    /// WAV バイト列の長さ。
    pub wav_bytes: usize,
    pub sample_rate: u32,
    pub duration_ms: u64,
    /// UNIX epoch からのミリ秒。
    pub started_at_ms: u64,
    pub target_hwnd: isize,
    pub target_process: String,
    pub target_title: String,
}

/// 状態変化通知のペイロード。
#[derive(Debug, Clone, Serialize)]
pub struct StatusPayload {
    pub status: Status,
    /// 補足メッセージ (エラー理由など)。無ければ `None`。
    pub message: Option<String>,
}
