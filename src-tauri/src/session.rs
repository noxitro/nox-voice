//! 録音セッションのデータモデルとアプリ状態機械。
//!
//! ここで定義する [`RecordingSession`] が M1 (録音) と M2 (STT) の境界インターフェース。
//! M2 は `wav_bytes` を Groq の Whisper へ送り、`target_hwnd` / `target_process` は
//! M3 の挿入時フォーカス照合 (設計 R7) でそのまま使う。

use std::path::PathBuf;
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
    /// 実行ファイルのフルパス。取得失敗時は `None`。
    ///
    /// 小窓に出すアプリ表示名とアイコン ([`crate::app_icon`]) を引くのに要る。
    /// ベース名 (`process_name`) では版情報もアイコンも引けない。
    /// **挿入の照合 (R7) には使わない** — あちらはこれまでどおり
    /// `hwnd` と `process_name` だけで判断する。
    #[serde(skip)]
    pub process_path: Option<PathBuf>,
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
            process_path: None,
        }
    }

    /// 照合可能な前景情報が取れているか。
    pub fn is_known(&self) -> bool {
        self.hwnd != 0
    }
}

/// 録音開始時に押さえておく情報。
///
/// 画面コンテキストは**その場限り**で、[`RecordingSession`] にも履歴にも
/// 残さない (design.md R1 / [`crate::context`] のプライバシー方針)。
///
/// **`Clone` は付けない。** 画面走査の待ち受け口
/// ([`crate::screen::ScanHandle`]) を持つので、複製できると 2 か所が同じ
/// 走査を待てることになり、結果は 1 回しか流れない以上どちらかが必ず
/// 「タイムアウト」を受け取る。この構造体は `take()` で 1 回だけ
/// 取り出される設計なので、複製できる必要がそもそも無い。
#[derive(Debug)]
pub struct PendingRecording {
    pub target: TargetWindow,
    pub started_at: SystemTime,
    /// どのホットキーで始めた録音か。結果の届け方 (貼り付け /
    /// クリップボードのみ) を決めるので、**開始時に固定する**。
    /// 録音中に設定を変えても、走っている録音の扱いは変わらない。
    pub mode: crate::hotkey::HotkeyMode,
    /// deep context で読んだ画面テキスト。無効なら空。
    pub context: crate::context::ScreenContext,
    /// 小窓に出している相手ウィンドウ ([`TargetView`])。
    ///
    /// **開始時に組み立てたものをそのまま持ち回す。** 処理中 (Processing)
    /// への遷移でも同じものを送るためで、そこで組み直すと「どのモニタを
    /// 読むか」が**録音終了時の前景**で決まってしまい、走査が見た画面
    /// (録音開始時) と食い違う。
    pub target_view: Option<TargetView>,
    /// 画面質問モードの走査 (この用途以外では `None`)。
    ///
    /// **結果ではなく待ち受け口を持つ。** 走査は録音と並行して進み、
    /// 回収は後処理ワーカーで行う ([`crate::screen::ScanHandle`])。
    /// ここで結果を待つと、録音開始が数秒遅れて最初の一言が消える。
    pub screen: Option<crate::screen::ScanHandle>,
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
    /// 録音を始めたホットキーの用途 ([`PendingRecording::mode`])。
    pub mode: crate::hotkey::HotkeyMode,
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

/// 状態変化の出どころ。
///
/// オーバーレイは**録音由来の状態だけ**を映す。再転写のような裏方の作業まで
/// 映すと、小窓が「認識中…」のまま出しっぱなしになる (完了イベントが
/// 録音の結果としては飛んでこないため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusOrigin {
    /// ホットキーによる録音の流れ。
    Recording,
    /// 履歴からの再転写など、裏方の作業。
    Background,
}

/// 状態変化通知のペイロード。
#[derive(Debug, Clone, Serialize)]
pub struct StatusPayload {
    pub status: Status,
    /// 補足メッセージ (エラー理由など)。無ければ `None`。
    pub message: Option<String>,
    pub origin: StatusOrigin,
    /// 画面質問モードでの録音かどうか。
    ///
    /// 小窓 (overlay.ts) が普段の書き取りと見分けを付けられるようにするための
    /// もの。通常の書き取りと画面質問は同じ「録音中」アイコンなので、色を
    /// 変えないと押し間違いに気づけない (実際にユーザーから紛らわしいと
    /// 報告があった)。`Status::Idle` への遷移では意味を持たないので `false`
    /// で送って構わない。
    pub screen_ask: bool,
    /// 録音開始時に掴んだ「相手ウィンドウ」。**小窓の表示専用**。
    ///
    /// クリップボードのみモードと `Status::Idle` では `None`
    /// ([`TargetView`] の doc に理由)。
    pub target: Option<TargetView>,
}

/// 小窓に出す「相手ウィンドウ」。
///
/// 出す理由: 通常モードの貼付先は**録音開始時の前景ウィンドウ**で固定される
/// (design.md R7)。押し間違い (別ウィンドウを見ながら喋った・フォーカスが
/// 思っていた場所に無かった) は、貼られた後にしか気づけないのが一番痛い。
/// 録音中に相手の名前が出ていれば、話し終える前に気づける。
///
/// **クリップボードのみモードでは出さない。** あちらは貼らない設計
/// (フォーカス無し運用が前提) なので、相手を出すと「そこに貼られる」と
/// 読めてしまう — 出さないことが正しい情報になる。
///
/// **画面質問モードは前景ではなくモニタ 1 枚が対象**であり、前景は
/// モニタ選択にしか効かない (`screen/win32.rs::choose_monitor`)。
/// そこで「前景アプリ + どのモニタを読むか」を出す。
#[derive(Debug, Clone, Serialize)]
pub struct TargetView {
    /// 相手ウィンドウのハンドル。**アイコン到着イベントとの突き合わせ用**
    /// (`nox://target-icon`)。古い録音の遅れて届いたアイコンを弾く。
    pub hwnd: isize,
    /// 前景を掴めたか。`false` なら小窓は「挿入先を特定できません」を出す。
    pub known: bool,
    /// アプリの表示名。**録音開始を待たせないため、ここに入るのは
    /// 即座に出せる値**(キャッシュ済みの版情報、無ければ exe のベース名)。
    /// 版情報を引き終えたら `nox://target-icon` で上書きされる。
    pub app_name: String,
    /// ウィンドウタイトル (末尾のアプリ名は落としてある)。
    pub title: String,
    /// アプリアイコン (`data:image/png;base64,...`)。**キャッシュが当たった
    /// ときだけ** Some で、外れたら `nox://target-icon` で後追いする。
    pub icon: Option<String>,
    /// 画面質問モードで**複数モニタのときだけ** Some。「どのモニタを読むか」。
    pub monitor: Option<String>,
}

/// タイトル末尾の ` - アプリ名` を 1 回だけ落とす。
///
/// 小窓は「アプリ名 · タイトル」の形で出すので、`設計メモ - Google Chrome`
/// をそのまま出すとアプリ名が 2 度並ぶ。狭い 1 行の半分がその重複で埋まる。
///
/// 区切りは ASCII ハイフン・em dash・en dash の 3 種 (Chrome / Edge /
/// エディタで実際に使われている)。**完全一致 (タイトルがアプリ名そのもの)
/// のときは落とさない** — 空文字にすると「タイトルが取れなかった」のと
/// 見分けが付かなくなる。
pub fn trim_app_suffix(title: &str, app_name: &str) -> String {
    let title = title.trim();
    let app_name = app_name.trim();
    if app_name.is_empty() {
        return title.to_string();
    }
    for separator in [" - ", " — ", " – "] {
        // 大小無視で末尾を見る。`.rfind` ではなく長さで切るのは、
        // タイトル中に同じ並びがあっても**末尾のものだけ**を落とすため。
        let suffix_len = separator.len() + app_name.len();
        if title.len() <= suffix_len {
            continue;
        }
        let cut = title.len() - suffix_len;
        // **バイト位置で切らない。** 日本語タイトルでは境界の途中に落ちうる
        // (`split_at` ならそこで panic する)。`get` は境界外なら None を返す。
        let (Some(head), Some(tail)) = (title.get(..cut), title.get(cut..)) else {
            continue;
        };
        if tail.eq_ignore_ascii_case(&format!("{separator}{app_name}")) {
            return head.trim_end().to_string();
        }
    }
    title.to_string()
}

#[cfg(test)]
mod tests {
    use super::trim_app_suffix;

    #[test]
    fn it_drops_the_app_name_from_the_end_of_a_title() {
        assert_eq!(
            trim_app_suffix("設計ドキュメント — Wiki - Google Chrome", "Google Chrome"),
            "設計ドキュメント — Wiki"
        );
        // em dash / en dash 区切りも同じように落とす。
        assert_eq!(trim_app_suffix("メモ — Notepad", "Notepad"), "メモ");
        assert_eq!(trim_app_suffix("メモ – Notepad", "Notepad"), "メモ");
        // 大小は無視する (アプリによって表記がぶれる)。
        assert_eq!(trim_app_suffix("メモ - notepad", "Notepad"), "メモ");
    }

    #[test]
    fn it_drops_only_the_last_occurrence() {
        // タイトルの途中に同じ並びがあっても、落とすのは末尾だけ。
        assert_eq!(
            trim_app_suffix("Notepad - の使い方 - Notepad", "Notepad"),
            "Notepad - の使い方"
        );
    }

    #[test]
    fn it_keeps_a_title_that_is_only_the_app_name() {
        // 空にすると「タイトルが取れなかった」と区別が付かなくなる。
        assert_eq!(trim_app_suffix("Google Chrome", "Google Chrome"), "Google Chrome");
    }

    #[test]
    fn it_does_nothing_without_an_app_name() {
        assert_eq!(trim_app_suffix("メモ - Notepad", ""), "メモ - Notepad");
        assert_eq!(trim_app_suffix("  メモ  ", "Notepad"), "メモ");
    }

    #[test]
    fn it_survives_a_multibyte_title() {
        // バイト位置で切ると文字境界の途中に落ちて panic しうる。
        assert_eq!(trim_app_suffix("あいうえお", "え"), "あいうえお");
        assert_eq!(trim_app_suffix("あいう - メモ帳", "メモ帳"), "あいう");
    }
}
