//! 履歴の永続化 (SQLite) — **design.md R4 の本実装**。
//!
//! # なぜ「注入前に」書くのか
//!
//! R4 の要求は「生転写・整形結果・音声メタを**注入前に**永続化する」こと。
//! 注入は最も壊れやすい工程 (フォーカスが変わる、クリップボードを奪われる、
//! 昇格アプリで弾かれる) で、そこで失敗したときに発話が消えるのが最悪の結末。
//! 先に書いておけば、貼付が不達でも履歴から取り出せる。
//!
//! # 失敗しても止めない、ただし黙らない
//!
//! DB の書き込み失敗でパイプラインを止めない (発話が消える方が悪い)。
//! ただし wiki「台帳データの静かな破壊」の教訓どおり、
//! **失敗を「空」に化けさせない**:
//!
//! - 読み出しに失敗したら `Err` を返す。空の `Vec` を返してはいけない
//!   (UI が「履歴 0 件」と表示し、ユーザーは消えたと信じる)。
//! - 書き込みに失敗したら呼び出し側へ伝え、WAV 退避とエラー通知に落とす。
//! - 全消去はユーザーの明示操作でのみ行い、失敗を握り潰さない。
//!
//! # WAV は DB 行と運命共同体
//!
//! 退避した WAV (`failed/*.wav` と隣の `*.json`) は、**それを指す行が所有する**。
//! この不変条件を崩すと、片方だけが残って打ち消し合う:
//!
//! - 行だけ消してファイルを残す → 起動時の取り込みが「知らないファイル」として
//!   再登録し、**削除したはずの履歴が復活する**。全消去も嘘になる。
//! - ファイルだけ消して行を残す → 再転写ボタンが永久に失敗する。
//!
//! したがって:
//!
//! 1. **`wav_path` を持つ行は、そのファイルの持ち主**。削除 (単体・全消去) は
//!    先にファイルを消し、消せたものだけ行を消す。消せなければ行を残す
//!    (「消したつもりで残っている」を作らない)。
//! 2. **`wav_path` を持つ行は保持期限で消さない**。まだ回収されていない
//!    データであり、消せば取り込み → 削除の無限ループになる。
//! 3. **再転写に成功したらファイルを消して `wav_path` を NULL にする**。
//!    通常の録音と同じ状態へ収束させ、以後は保持期限の対象になる。
//!    ファイルを消せなかった場合は所有権 (`wav_path`) を保持したままにする —
//!    テキストは守られ、孤児ファイルも生まれない。
//!
//! # 接続の持ち方
//!
//! 接続はプールせず、操作ごとに開いて閉じる短命接続にする。
//! 書き込みはファイナライズワーカー 1 本に直列化されており、
//! 読み出しは UI コマンドからの短い問い合わせだけなので、
//! ロックを跨いで持ち回る利点がない。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// スキーマのバージョン。`PRAGMA user_version` で管理する。
const SCHEMA_VERSION: i64 = 1;

/// 履歴操作のエラー。
#[derive(Debug)]
pub enum HistoryError {
    /// DB を開けない (パス・権限・破損)。
    Open(String),
    /// スキーマの作成・移行に失敗。
    Migrate(String),
    /// クエリの実行に失敗。
    Query(String),
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::Open(e) => write!(f, "履歴データベースを開けません: {e}"),
            HistoryError::Migrate(e) => write!(f, "履歴データベースの初期化に失敗しました: {e}"),
            HistoryError::Query(e) => write!(f, "履歴データベースの操作に失敗しました: {e}"),
        }
    }
}

impl std::error::Error for HistoryError {}

/// 整形の結末を DB に入れるための文字列。
///
/// [`crate::pipeline::FormatOutcome`] と 1 対 1 だが、DB のスキーマを
/// Rust の enum 定義に引きずられないよう、変換をここに閉じ込める。
pub const OUTCOME_FORMATTED: &str = "formatted";
pub const OUTCOME_RAW_FALLBACK: &str = "raw_fallback";
pub const OUTCOME_DISABLED: &str = "disabled";
/// STT がまだ通っていない行 (失敗 WAV の取り込み)。
pub const OUTCOME_UNTRANSCRIBED: &str = "untranscribed";

/// 履歴へ書く 1 件分 (注入前の状態)。
#[derive(Debug, Clone)]
pub struct SessionDraft {
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub target_process: String,
    pub target_hwnd: isize,
    pub raw_text: Option<String>,
    pub formatted_text: Option<String>,
    pub outcome: String,
    pub outcome_reason: Option<String>,
    pub stt_ms: Option<u64>,
    pub format_ms: Option<u64>,
    /// 未転写行のみ。再転写に使う WAV のパス。
    pub wav_path: Option<String>,
}

/// 再転写の結果 (更新用のまとまり)。
#[derive(Debug, Clone)]
pub struct TranscriptionUpdate<'a> {
    pub raw_text: &'a str,
    pub formatted_text: &'a str,
    pub outcome: &'a str,
    pub outcome_reason: Option<&'a str>,
    pub stt_ms: u64,
    pub format_ms: u64,
}

/// UI へ返す 1 行。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionRow {
    pub id: i64,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub target_process: String,
    pub target_hwnd: i64,
    pub raw_text: Option<String>,
    pub formatted_text: Option<String>,
    pub outcome: String,
    pub outcome_reason: Option<String>,
    pub stt_ms: Option<u64>,
    pub format_ms: Option<u64>,
    pub inject_outcome: Option<String>,
    pub clipboard_state: Option<String>,
    /// 未転写行かどうか (再転写ボタンの出し分け)。
    pub has_audio: bool,
    pub created_at_ms: u64,
}

impl SessionRow {
    /// 再貼付・コピーに使うテキスト (整形後があればそれ、無ければ生転写)。
    pub fn text(&self) -> Option<&str> {
        self.formatted_text
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.raw_text.as_deref())
    }
}

/// 履歴ストア。
pub struct HistoryStore {
    path: PathBuf,
}

impl HistoryStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 操作ごとの短命接続を開く。
    fn connect(&self) -> Result<Connection, HistoryError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                HistoryError::Open(format!("{} を作成できません: {e}", parent.display()))
            })?;
        }
        let conn = Connection::open(&self.path).map_err(|e| HistoryError::Open(e.to_string()))?;
        // 書き込み中の読み取りを止めないための設定。
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| HistoryError::Open(e.to_string()))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| HistoryError::Open(e.to_string()))?;
        // 他の接続が書いている間、少し待つ (即エラーにしない)。
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| HistoryError::Open(e.to_string()))?;
        Ok(conn)
    }

    /// スキーマを作成・移行する。起動時に一度呼ぶ。
    pub fn initialize(&self) -> Result<(), HistoryError> {
        let conn = self.connect()?;
        migrate(&conn)
    }

    /// 注入前の 1 件を書く。戻り値は行 ID。
    pub fn insert(&self, draft: &SessionDraft) -> Result<i64, HistoryError> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO sessions (
                 started_at_ms, duration_ms, target_process, target_hwnd,
                 raw_text, formatted_text, outcome, outcome_reason,
                 stt_ms, format_ms, wav_path, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                draft.started_at_ms as i64,
                draft.duration_ms as i64,
                draft.target_process,
                draft.target_hwnd as i64,
                draft.raw_text,
                draft.formatted_text,
                draft.outcome,
                draft.outcome_reason,
                draft.stt_ms.map(|v| v as i64),
                draft.format_ms.map(|v| v as i64),
                draft.wav_path,
                now_ms() as i64,
            ],
        )
        .map_err(|e| HistoryError::Query(e.to_string()))?;
        Ok(conn.last_insert_rowid())
    }

    /// 失敗 WAV を「未転写」行として取り込む。
    ///
    /// 同じ `wav_path` が既にあれば何もせず `Ok(None)` を返す
    /// (起動のたびに重複行が増えないように、部分ユニークインデックスで担保)。
    pub fn insert_untranscribed(
        &self,
        draft: &SessionDraft,
    ) -> Result<Option<i64>, HistoryError> {
        let Some(wav_path) = draft.wav_path.as_deref() else {
            return Err(HistoryError::Query(
                "未転写行には wav_path が必要です".to_string(),
            ));
        };
        let conn = self.connect()?;
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM sessions WHERE wav_path = ?1",
                params![wav_path],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| HistoryError::Query(e.to_string()))?;
        if existing.is_some() {
            return Ok(None);
        }
        drop(conn);
        self.insert(draft).map(Some)
    }

    /// 注入の結果を後から書き足す (R4: 挿入は注入前、結果は注入後)。
    pub fn update_injection(
        &self,
        id: i64,
        inject_outcome: &str,
        clipboard_state: &str,
    ) -> Result<(), HistoryError> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE sessions SET inject_outcome = ?2, clipboard_state = ?3 WHERE id = ?1",
            params![id, inject_outcome, clipboard_state],
        )
        .map_err(|e| HistoryError::Query(e.to_string()))?;
        Ok(())
    }

    /// 再転写の結果で行を更新し、**用済みの WAV を手放す**。
    ///
    /// 退避 WAV を削除して `wav_path` を NULL にし、通常の録音と同じ状態へ
    /// 収束させる。そうしないと「保持期限で行が消える → 残ったファイルが
    /// 未転写として再登録される」ループで、回収したテキストが失われる。
    ///
    /// ファイルを削除できなかった場合は `wav_path` を保持したままにする。
    /// 行がファイルの持ち主であり続けるので孤児は生まれず、
    /// テキストも保存される (保持期限の対象外のままになる)。
    ///
    /// 戻り値は更新した行数。**0 なら行が消えている** (処理中に削除された)。
    /// 呼び出し側は 0 を黙って捨てず、結果が行き場を失ったことを知らせること。
    pub fn update_transcription(
        &self,
        id: i64,
        update: &TranscriptionUpdate<'_>,
    ) -> Result<u64, HistoryError> {
        // 先にファイルを手放す。行を先に更新すると、削除に失敗したときに
        // 「wav_path が NULL なのにファイルは在る」= 孤児ができてしまう。
        let released = match self.wav_path(id)? {
            Some(path) => match remove_wav_files(&path) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("再転写後の WAV を削除できません ({path}): {e}");
                    false
                }
            },
            None => true,
        };
        let TranscriptionUpdate {
            raw_text,
            formatted_text,
            outcome,
            outcome_reason,
            stt_ms,
            format_ms,
        } = update;
        let conn = self.connect()?;
        conn.execute(
            "UPDATE sessions
                SET raw_text = ?2, formatted_text = ?3, outcome = ?4,
                    outcome_reason = ?5, stt_ms = ?6, format_ms = ?7,
                    wav_path = CASE WHEN ?8 THEN NULL ELSE wav_path END
              WHERE id = ?1",
            params![
                id,
                raw_text,
                formatted_text,
                outcome,
                outcome_reason,
                *stt_ms as i64,
                *format_ms as i64,
                released
            ],
        )
        .map(|n| n as u64)
        .map_err(|e| HistoryError::Query(e.to_string()))
    }

    /// 新しい順に取り出す。`before_id` を渡すとそれより古い行から続きを返す。
    ///
    /// **失敗時に空の `Vec` を返さない。** 読めなかったことと
    /// 0 件であることは、UI にとってまったく違う情報。
    pub fn recent(
        &self,
        limit: u32,
        before_id: Option<i64>,
    ) -> Result<Vec<SessionRow>, HistoryError> {
        let conn = self.connect()?;
        let limit = limit.clamp(1, 500) as i64;

        let mut stmt = conn
            .prepare(
                "SELECT id, started_at_ms, duration_ms, target_process, target_hwnd,
                        raw_text, formatted_text, outcome, outcome_reason,
                        stt_ms, format_ms, inject_outcome, clipboard_state,
                        wav_path, created_at_ms
                   FROM sessions
                  WHERE (?2 IS NULL OR id < ?2)
                  ORDER BY id DESC
                  LIMIT ?1",
            )
            .map_err(|e| HistoryError::Query(e.to_string()))?;

        let rows = stmt
            .query_map(params![limit, before_id], row_to_session)
            .map_err(|e| HistoryError::Query(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| HistoryError::Query(e.to_string()))?;
        Ok(rows)
    }

    /// 1 件取得。
    pub fn get(&self, id: i64) -> Result<Option<SessionRow>, HistoryError> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, started_at_ms, duration_ms, target_process, target_hwnd,
                    raw_text, formatted_text, outcome, outcome_reason,
                    stt_ms, format_ms, inject_outcome, clipboard_state,
                    wav_path, created_at_ms
               FROM sessions WHERE id = ?1",
            params![id],
            row_to_session,
        )
        .optional()
        .map_err(|e| HistoryError::Query(e.to_string()))
    }

    /// 未転写行に紐づく WAV のパスを引く。
    pub fn wav_path(&self, id: i64) -> Result<Option<String>, HistoryError> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT wav_path FROM sessions WHERE id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(|e| HistoryError::Query(e.to_string()))
    }

    /// 総件数。
    pub fn count(&self) -> Result<u64, HistoryError> {
        let conn = self.connect()?;
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|v| v as u64)
        .map_err(|e| HistoryError::Query(e.to_string()))
    }

    /// 1 件削除。**その行が持っている WAV も消す。**
    ///
    /// ファイルを消せなかった場合は行も消さない。行を消してファイルを残すと、
    /// 次の起動で「知らないファイル」として取り込まれ、削除したはずの履歴が
    /// 復活する。
    pub fn delete(&self, id: i64) -> Result<Removal, HistoryError> {
        let paths = self.wav_path(id)?.into_iter().collect::<Vec<_>>();
        let (removed, failures) = release_wavs(&paths);
        if !failures.is_empty() {
            // 所有ファイルを手放せていないので行は残す。
            return Ok(Removal {
                rows: 0,
                wavs_removed: removed,
                wav_failures: failures,
            });
        }

        let conn = self.connect()?;
        let rows = conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(|e| HistoryError::Query(e.to_string()))? as u64;
        Ok(Removal {
            rows,
            wavs_removed: removed,
            wav_failures: Vec::new(),
        })
    }

    /// 全消去。ユーザーの明示操作でのみ呼ぶこと。
    ///
    /// 退避 WAV も消す。残すと次の起動で復活し、「すべて削除」が嘘になる。
    /// 消せなかったファイルを持つ行だけは残し、その事実を返す。
    pub fn clear(&self) -> Result<Removal, HistoryError> {
        let owned = self.owned_wav_paths(None)?;
        let (removed, failures) = release_wavs(&owned);

        let conn = self.connect()?;
        // 消せなかったファイルを持つ行は残す (孤児を作らない)。
        let rows = if failures.is_empty() {
            conn.execute("DELETE FROM sessions", [])
        } else {
            let keep = failures.join("\u{1}");
            conn.execute(
                "DELETE FROM sessions
                  WHERE wav_path IS NULL
                     OR instr(?1, wav_path) = 0",
                params![keep],
            )
        }
        .map_err(|e| HistoryError::Query(e.to_string()))? as u64;

        Ok(Removal {
            rows,
            wavs_removed: removed,
            wav_failures: failures,
        })
    }

    /// 保持期限を過ぎた行を消す。`days == 0` なら無制限 (何もしない)。
    ///
    /// **WAV を持っている行は消さない。**
    ///
    /// 消してしまうと、起動のたびに「取り込む → 期限切れで消す」を
    /// 際限なく繰り返し、WAV はディスクに残り続けるのに履歴には
    /// 一度も現れない、という状態になる (実機の起動テストで発見)。
    /// 未転写行は「まだ回収できていないデータ」であって、
    /// 保持ポリシーが捨ててよい過去の記録ではない。
    ///
    /// 判定を `outcome` ではなく `wav_path` で行うのが要点。再転写に成功した
    /// 行は outcome が変わるが、ファイルを手放せていなければまだ持ち主であり、
    /// ここで消すと同じループの一段上を再現してしまう
    /// (回収したテキストが期限で消え、幽霊の未転写行が残る)。
    pub fn purge_older_than(&self, days: u32) -> Result<u64, HistoryError> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = cutoff_ms(now_ms(), days);
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM sessions WHERE started_at_ms < ?1 AND wav_path IS NULL",
            params![cutoff as i64],
        )
        .map(|n| n as u64)
        .map_err(|e| HistoryError::Query(e.to_string()))
    }

    /// 行が所有している WAV のパス一覧。`before_cutoff` を渡すとそれより古い行だけ。
    fn owned_wav_paths(&self, before_cutoff: Option<u64>) -> Result<Vec<String>, HistoryError> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT wav_path FROM sessions
                  WHERE wav_path IS NOT NULL
                    AND (?1 IS NULL OR started_at_ms < ?1)",
            )
            .map_err(|e| HistoryError::Query(e.to_string()))?;
        let paths = stmt
            .query_map(params![before_cutoff.map(|v| v as i64)], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| HistoryError::Query(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| HistoryError::Query(e.to_string()))?;
        Ok(paths)
    }
}

/// 削除の結果。行とファイルの両方について報告する。
///
/// 「行は消えたがファイルは残った」を無言にしないための型。
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Removal {
    /// 削除できた行数。
    pub rows: u64,
    /// 削除できた WAV の数。
    pub wavs_removed: usize,
    /// 削除できなかった WAV のパス (該当行は残してある)。
    pub wav_failures: Vec<String>,
}

impl Removal {
    /// 消し残しがあるか。
    pub fn is_complete(&self) -> bool {
        self.wav_failures.is_empty()
    }
}

/// 退避 WAV とその隣のメタ JSON を消す。
///
/// 既に無い場合は成功扱い (目的は「残っていないこと」なので)。
fn remove_wav_files(wav_path: &str) -> Result<(), String> {
    let wav = std::path::Path::new(wav_path);
    remove_if_exists(wav)?;
    // メタが消せなくても致命ではないが、残すと退避先が散らかる。
    let meta = wav.with_extension("json");
    if let Err(e) = remove_if_exists(&meta) {
        log::warn!("退避メタ情報を削除できません ({}): {e}", meta.display());
    }
    Ok(())
}

fn remove_if_exists(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// まとめて WAV を手放す。戻り値は (消せた数, 消せなかったパス)。
fn release_wavs(paths: &[String]) -> (usize, Vec<String>) {
    let mut removed = 0;
    let mut failures = Vec::new();
    for path in paths {
        match remove_wav_files(path) {
            Ok(()) => removed += 1,
            Err(e) => {
                log::error!("退避 WAV を削除できません: {e}");
                failures.push(path.clone());
            }
        }
    }
    (removed, failures)
}

/// 保持期限の境界時刻 (これより古い行は消す)。純関数なのでテストできる。
fn cutoff_ms(now_ms: u64, days: u32) -> u64 {
    let span = (days as u64).saturating_mul(24 * 60 * 60 * 1_000);
    now_ms.saturating_sub(span)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let wav_path: Option<String> = row.get(13)?;
    Ok(SessionRow {
        id: row.get(0)?,
        started_at_ms: row.get::<_, i64>(1)? as u64,
        duration_ms: row.get::<_, i64>(2)? as u64,
        target_process: row.get(3)?,
        target_hwnd: row.get(4)?,
        raw_text: row.get(5)?,
        formatted_text: row.get(6)?,
        outcome: row.get(7)?,
        outcome_reason: row.get(8)?,
        stt_ms: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        format_ms: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        inject_outcome: row.get(11)?,
        clipboard_state: row.get(12)?,
        has_audio: wav_path.is_some(),
        created_at_ms: row.get::<_, i64>(14)? as u64,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `PRAGMA user_version` 方式のマイグレーション。
///
/// 版を 1 つずつ上げる形にしてあるので、将来 v2 を足すときは
/// `if version < 2 { ... }` を追記するだけでよい。既存の DB は
/// 自分の版から順に適用される。**版を下げる移行は用意しない**
/// (古いアプリで新しい DB を開く運用を想定しない)。
fn migrate(conn: &Connection) -> Result<(), HistoryError> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| HistoryError::Migrate(e.to_string()))?;

    if version > SCHEMA_VERSION {
        return Err(HistoryError::Migrate(format!(
            "履歴データベースの版 ({version}) がこのアプリ ({SCHEMA_VERSION}) より新しいため開けません"
        )));
    }
    if version == SCHEMA_VERSION {
        return Ok(());
    }

    if version < 1 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS sessions (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 started_at_ms   INTEGER NOT NULL,
                 duration_ms     INTEGER NOT NULL,
                 target_process  TEXT    NOT NULL,
                 target_hwnd     INTEGER NOT NULL,
                 raw_text        TEXT,
                 formatted_text  TEXT,
                 outcome         TEXT    NOT NULL,
                 outcome_reason  TEXT,
                 stt_ms          INTEGER,
                 format_ms       INTEGER,
                 inject_outcome  TEXT,
                 clipboard_state TEXT,
                 wav_path        TEXT,
                 created_at_ms   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_sessions_started_at
                 ON sessions(started_at_ms DESC);
             -- 失敗 WAV の取り込みを冪等にする。
             CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_wav_path
                 ON sessions(wav_path) WHERE wav_path IS NOT NULL;
             COMMIT;",
        )
        .map_err(|e| HistoryError::Migrate(e.to_string()))?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|e| HistoryError::Migrate(e.to_string()))?;
    log::info!("履歴データベースを版 {SCHEMA_VERSION} へ初期化しました");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDb {
        dir: PathBuf,
        store: HistoryStore,
    }

    impl TempDb {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "nox-history-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let store = HistoryStore::new(dir.join("nox-voice.db"));
            store.initialize().expect("初期化できる");
            Self { dir, store }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn draft(started_at_ms: u64) -> SessionDraft {
        SessionDraft {
            started_at_ms,
            duration_ms: 2_500,
            target_process: "notepad.exe".to_string(),
            target_hwnd: 0x1234,
            raw_text: Some("えーと こんにちは".to_string()),
            formatted_text: Some("こんにちは。".to_string()),
            outcome: OUTCOME_FORMATTED.to_string(),
            outcome_reason: None,
            stt_ms: Some(800),
            format_ms: Some(1_200),
            wav_path: None,
        }
    }

    // --- スキーマ往復 ---

    #[test]
    fn inserted_row_round_trips_every_field() {
        let db = TempDb::new("roundtrip");
        let id = db.store.insert(&draft(1_700_000_000_000)).expect("書ける");

        let row = db.store.get(id).expect("読める").expect("行がある");
        assert_eq!(row.id, id);
        assert_eq!(row.started_at_ms, 1_700_000_000_000);
        assert_eq!(row.duration_ms, 2_500);
        assert_eq!(row.target_process, "notepad.exe");
        assert_eq!(row.target_hwnd, 0x1234);
        assert_eq!(row.raw_text.as_deref(), Some("えーと こんにちは"));
        assert_eq!(row.formatted_text.as_deref(), Some("こんにちは。"));
        assert_eq!(row.outcome, OUTCOME_FORMATTED);
        assert_eq!(row.stt_ms, Some(800));
        assert_eq!(row.format_ms, Some(1_200));
        // 注入はまだ行われていない。
        assert_eq!(row.inject_outcome, None);
        assert_eq!(row.clipboard_state, None);
        assert!(!row.has_audio);
        assert!(row.created_at_ms > 0);
    }

    #[test]
    fn raw_transcript_survives_even_when_formatting_degraded() {
        // R5: 生転写と整形結果を並置できることが履歴の存在意義。
        let db = TempDb::new("degraded");
        let mut d = draft(1_700_000_000_000);
        d.outcome = OUTCOME_RAW_FALLBACK.to_string();
        d.outcome_reason = Some("Gemini のレート制限に達しました".to_string());
        d.formatted_text = d.raw_text.clone();
        let id = db.store.insert(&d).expect("書ける");

        let row = db.store.get(id).expect("読める").expect("行がある");
        assert_eq!(row.outcome, OUTCOME_RAW_FALLBACK);
        assert_eq!(
            row.outcome_reason.as_deref(),
            Some("Gemini のレート制限に達しました")
        );
        assert_eq!(row.raw_text, row.formatted_text);
    }

    #[test]
    fn injection_result_is_written_after_the_row_exists() {
        // R4: 行は注入前に作られ、結果だけ後から足される。
        let db = TempDb::new("inject-update");
        let id = db.store.insert(&draft(1_700_000_000_000)).expect("書ける");
        db.store
            .update_injection(id, "injected", "restored_original")
            .expect("更新できる");

        let row = db.store.get(id).expect("読める").expect("行がある");
        assert_eq!(row.inject_outcome.as_deref(), Some("injected"));
        assert_eq!(row.clipboard_state.as_deref(), Some("restored_original"));
        // 本文は壊れていない。
        assert_eq!(row.raw_text.as_deref(), Some("えーと こんにちは"));
    }

    #[test]
    fn text_prefers_formatted_but_falls_back_to_raw() {
        let mut row = SessionRow {
            id: 1,
            started_at_ms: 0,
            duration_ms: 0,
            target_process: String::new(),
            target_hwnd: 0,
            raw_text: Some("生".to_string()),
            formatted_text: Some("整形".to_string()),
            outcome: OUTCOME_FORMATTED.to_string(),
            outcome_reason: None,
            stt_ms: None,
            format_ms: None,
            inject_outcome: None,
            clipboard_state: None,
            has_audio: false,
            created_at_ms: 0,
        };
        assert_eq!(row.text(), Some("整形"));
        row.formatted_text = None;
        assert_eq!(row.text(), Some("生"));
        // 空文字の整形結果で生転写を隠さない。
        row.formatted_text = Some(String::new());
        assert_eq!(row.text(), Some("生"));
        row.raw_text = None;
        row.formatted_text = None;
        assert_eq!(row.text(), None);
    }

    // --- 一覧とページング ---

    #[test]
    fn recent_returns_newest_first_and_pages() {
        let db = TempDb::new("paging");
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(db.store.insert(&draft(1_700_000_000_000 + i)).expect("書ける"));
        }

        let page1 = db.store.recent(3, None).expect("読める");
        assert_eq!(page1.len(), 3);
        assert_eq!(page1[0].id, ids[4], "新しい順になっていない");
        assert_eq!(page1[2].id, ids[2]);

        let page2 = db.store.recent(3, Some(page1[2].id)).expect("読める");
        assert_eq!(page2.len(), 2, "続きが取れていない");
        assert_eq!(page2[0].id, ids[1]);
        assert_eq!(page2[1].id, ids[0]);

        // 末尾より先は空 (エラーではない)。
        assert!(db.store.recent(3, Some(ids[0])).expect("読める").is_empty());
    }

    #[test]
    fn empty_history_is_ok_not_an_error() {
        // 「0 件」と「読めなかった」は別物。0 件は Ok(空) で表す。
        let db = TempDb::new("empty");
        assert!(db.store.recent(50, None).expect("読める").is_empty());
        assert_eq!(db.store.count().expect("読める"), 0);
    }

    #[test]
    fn unreadable_database_is_an_error_not_an_empty_list() {
        // wiki「台帳データの静かな破壊」: 読めない状態を空に化けさせない。
        let dir = std::env::temp_dir().join(format!("nox-history-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("テスト用ディレクトリ");
        let path = dir.join("nox-voice.db");
        std::fs::write(&path, b"this is definitely not a sqlite file").expect("書ける");

        let store = HistoryStore::new(path);
        let result = store.recent(50, None);
        assert!(
            result.is_err(),
            "壊れた DB から空リストを返した (UI が『履歴 0 件』と表示してしまう)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- 失敗 WAV の取り込み ---

    #[test]
    fn untranscribed_rows_are_imported_once() {
        let db = TempDb::new("untranscribed");
        let mut d = draft(1_700_000_000_000);
        d.raw_text = None;
        d.formatted_text = None;
        d.outcome = OUTCOME_UNTRANSCRIBED.to_string();
        d.stt_ms = None;
        d.format_ms = None;
        d.wav_path = Some(r"C:\tmp\failed\1700000000000.wav".to_string());

        let first = db.store.insert_untranscribed(&d).expect("書ける");
        assert!(first.is_some());

        // 起動のたびに走っても増えない。
        let second = db.store.insert_untranscribed(&d).expect("書ける");
        assert_eq!(second, None, "同じ WAV が二重登録された");
        assert_eq!(db.store.count().expect("読める"), 1);

        let row = db.store.get(first.expect("id")).expect("読める").expect("行");
        assert!(row.has_audio, "再転写できる行として見えていない");
        assert_eq!(row.outcome, OUTCOME_UNTRANSCRIBED);
        assert_eq!(row.raw_text, None);
    }

    #[test]
    fn untranscribed_without_a_wav_path_is_rejected() {
        let db = TempDb::new("untranscribed-nopath");
        let d = draft(1_700_000_000_000);
        assert!(db.store.insert_untranscribed(&d).is_err());
    }

    #[test]
    fn retranscription_fills_in_the_row() {
        let db = TempDb::new("retranscribe");
        let mut d = draft(1_700_000_000_000);
        d.raw_text = None;
        d.formatted_text = None;
        d.outcome = OUTCOME_UNTRANSCRIBED.to_string();
        d.wav_path = Some(r"C:\tmp\failed\a.wav".to_string());
        let id = db.store.insert_untranscribed(&d).expect("書ける").expect("id");

        db.store
            .update_transcription(
                id,
                &TranscriptionUpdate {
                    raw_text: "生転写",
                    formatted_text: "整形後。",
                    outcome: OUTCOME_FORMATTED,
                    outcome_reason: None,
                    stt_ms: 700,
                    format_ms: 900,
                },
            )
            .expect("更新できる");

        let row = db.store.get(id).expect("読める").expect("行");
        assert_eq!(row.raw_text.as_deref(), Some("生転写"));
        assert_eq!(row.formatted_text.as_deref(), Some("整形後。"));
        assert_eq!(row.outcome, OUTCOME_FORMATTED);
        assert_eq!(row.stt_ms, Some(700));
        // 実ファイルが無いパスなので「削除済み」とみなして所有権を手放す。
        // 回収済みの行は通常の録音と同じ状態 (音声なし) に収束する。
        assert!(!row.has_audio, "wav_path を持ったままだと幽霊行の元になる");
        assert_eq!(db.store.wav_path(id).expect("読める"), None);
    }

    // --- 削除と保持ポリシー ---

    #[test]
    fn delete_removes_only_the_requested_row() {
        let db = TempDb::new("delete");
        let a = db.store.insert(&draft(1)).expect("書ける");
        let b = db.store.insert(&draft(2)).expect("書ける");

        assert_eq!(db.store.delete(a).expect("消せる").rows, 1);
        assert!(db.store.get(a).expect("読める").is_none());
        assert!(db.store.get(b).expect("読める").is_some());

        // 存在しない ID の削除は 0 件 (エラーではない)。
        assert_eq!(db.store.delete(a).expect("消せる").rows, 0);
    }

    #[test]
    fn clear_removes_everything_and_reports_the_count() {
        let db = TempDb::new("clear");
        for i in 0..3 {
            db.store.insert(&draft(i)).expect("書ける");
        }
        let removal = db.store.clear().expect("消せる");
        assert_eq!(removal.rows, 3);
        assert!(removal.is_complete());
        assert_eq!(db.store.count().expect("読める"), 0);
    }

    #[test]
    fn retention_deletes_only_expired_rows() {
        let db = TempDb::new("retention");
        let now = now_ms();
        let day = 24 * 60 * 60 * 1_000u64;
        let old = db.store.insert(&draft(now - 40 * day)).expect("書ける");
        let fresh = db.store.insert(&draft(now - 3 * day)).expect("書ける");

        let purged = db.store.purge_older_than(30).expect("消せる");
        assert_eq!(purged, 1);
        assert!(db.store.get(old).expect("読める").is_none());
        assert!(db.store.get(fresh).expect("読める").is_some());
    }

    /// 未転写行は保持期限で消さない。
    ///
    /// 消すと「起動のたびに取り込んでは消える」無限ループになり、
    /// 退避した音声が履歴に一度も現れないまま残り続ける。
    #[test]
    fn retention_keeps_untranscribed_rows_with_audio() {
        let db = TempDb::new("retention-untranscribed");
        let now = now_ms();
        let day = 24 * 60 * 60 * 1_000u64;

        // 保持期限をとうに過ぎた失敗録音 (退避 WAV あり)。
        let mut pending = draft(now - 400 * day);
        pending.raw_text = None;
        pending.formatted_text = None;
        pending.outcome = OUTCOME_UNTRANSCRIBED.to_string();
        pending.wav_path = Some(r"C:\tmp\failed\old.wav".to_string());
        let pending_id = db
            .store
            .insert_untranscribed(&pending)
            .expect("書ける")
            .expect("id");

        // 同じくらい古い、転写済みの行。
        let old_id = db.store.insert(&draft(now - 400 * day)).expect("書ける");

        let purged = db.store.purge_older_than(30).expect("消せる");
        assert_eq!(purged, 1, "転写済みの古い行だけが消えるはず");
        assert!(
            db.store.get(pending_id).expect("読める").is_some(),
            "未転写行が消された (次回起動で再取り込み → また削除、の無限ループになる)"
        );
        assert!(db.store.get(old_id).expect("読める").is_none());
    }

    /// 再転写に成功した行は、以後は通常どおり期限の対象になる。
    #[test]
    fn retention_applies_once_a_pending_row_is_transcribed() {
        let db = TempDb::new("retention-after-retranscribe");
        let now = now_ms();
        let day = 24 * 60 * 60 * 1_000u64;

        let mut pending = draft(now - 400 * day);
        pending.raw_text = None;
        pending.formatted_text = None;
        pending.outcome = OUTCOME_UNTRANSCRIBED.to_string();
        pending.wav_path = Some(r"C:\tmp\failed\pending-b.wav".to_string());
        let id = db
            .store
            .insert_untranscribed(&pending)
            .expect("書ける")
            .expect("id");

        db.store
            .update_transcription(
                id,
                &TranscriptionUpdate {
                    raw_text: "生転写",
                    formatted_text: "整形後。",
                    outcome: OUTCOME_FORMATTED,
                    outcome_reason: None,
                    stt_ms: 1,
                    format_ms: 1,
                },
            )
            .expect("更新できる");

        assert_eq!(db.store.purge_older_than(30).expect("消せる"), 1);
        assert!(db.store.get(id).expect("読める").is_none());
    }

    #[test]
    fn retention_zero_means_keep_forever() {
        let db = TempDb::new("retention-zero");
        db.store.insert(&draft(0)).expect("書ける"); // epoch = 極めて古い
        assert_eq!(db.store.purge_older_than(0).expect("消せる"), 0);
        assert_eq!(db.store.count().expect("読める"), 1);
    }

    #[test]
    fn cutoff_is_computed_without_overflow() {
        let day = 24 * 60 * 60 * 1_000u64;
        assert_eq!(cutoff_ms(100 * day, 30), 70 * day);
        // 現在時刻が保持期間より小さくても panic しない。
        assert_eq!(cutoff_ms(day, 30), 0);
        assert_eq!(cutoff_ms(u64::MAX, u32::MAX), u64::MAX - (u32::MAX as u64) * day);
    }

    // --- WAV と DB 行のライフサイクル (レビュー指摘 M-1 / M-2) ---

    /// 実ファイルを持つ未転写行を作る。
    fn seed_pending(db: &TempDb, name: &str) -> (i64, PathBuf) {
        let wav_dir = db.dir.join("failed");
        std::fs::create_dir_all(&wav_dir).expect("作れる");
        let wav = wav_dir.join(format!("{name}.wav"));
        std::fs::write(&wav, b"RIFF____WAVEfmt ").expect("書ける");
        std::fs::write(wav.with_extension("json"), r#"{"error":"テスト"}"#).expect("書ける");

        let mut d = draft(now_ms());
        d.raw_text = None;
        d.formatted_text = None;
        d.outcome = OUTCOME_UNTRANSCRIBED.to_string();
        d.stt_ms = None;
        d.format_ms = None;
        d.wav_path = Some(wav.to_string_lossy().to_string());
        let id = db
            .store
            .insert_untranscribed(&d)
            .expect("書ける")
            .expect("id");
        (id, wav)
    }

    /// M-1 回帰: 削除した履歴は次回起動の取り込みで復活しない。
    ///
    /// 行だけ消して WAV を残すと、取り込みの重複判定 (wav_path の有無) も
    /// 一緒に消えるため、起動のたびに「削除したはずの履歴」が戻ってくる。
    #[test]
    fn deleting_a_row_also_removes_its_audio() {
        let db = TempDb::new("delete-wav");
        let (id, wav) = seed_pending(&db, "gone");
        assert!(wav.exists());

        let removal = db.store.delete(id).expect("消せる");
        assert_eq!(removal.rows, 1);
        assert_eq!(removal.wavs_removed, 1);
        assert!(removal.is_complete());

        assert!(!wav.exists(), "WAV が残っている (次回起動で復活する)");
        assert!(
            !wav.with_extension("json").exists(),
            "メタ JSON が残っている"
        );
        assert!(db.store.get(id).expect("読める").is_none());
    }

    /// M-1 回帰: 全消去も WAV ごと消す (でないと「すべて削除」が嘘になる)。
    #[test]
    fn clearing_history_also_removes_all_audio() {
        let db = TempDb::new("clear-wav");
        let (_, wav_a) = seed_pending(&db, "a");
        let (_, wav_b) = seed_pending(&db, "b");
        db.store.insert(&draft(now_ms())).expect("書ける"); // 音声なしの行

        let removal = db.store.clear().expect("消せる");
        assert_eq!(removal.rows, 3);
        assert_eq!(removal.wavs_removed, 2);
        assert!(removal.is_complete());
        assert!(!wav_a.exists() && !wav_b.exists(), "退避 WAV が残っている");
        assert_eq!(db.store.count().expect("読める"), 0);
    }

    /// 消せなかったファイルがあるときは、その行を残して報告する。
    ///
    /// 行だけ消してファイルを残すと復活するので、あえて残す方が正しい。
    #[test]
    fn a_row_survives_when_its_audio_cannot_be_removed() {
        let db = TempDb::new("delete-locked");
        let (id, wav) = seed_pending(&db, "locked");
        // ファイルを開いたままにして削除を失敗させる。
        let handle = std::fs::File::open(&wav).expect("開ける");

        let removal = db.store.delete(id).expect("エラーにはしない");
        if removal.is_complete() {
            // この環境では開いたままでも消せた。invariant は保たれている。
            assert!(!wav.exists());
            assert!(db.store.get(id).expect("読める").is_none());
        } else {
            assert_eq!(removal.rows, 0, "ファイルを残したまま行を消した");
            assert_eq!(removal.wav_failures.len(), 1);
            assert!(
                db.store.get(id).expect("読める").is_some(),
                "行が消えてファイルだけ残った (次回起動で復活する)"
            );
        }
        drop(handle);
    }

    /// M-2 回帰: 再転写に成功したら WAV を手放し、通常の行に収束する。
    ///
    /// `wav_path` を残したままだと、保持期限で行が消えた後にファイルが
    /// 再び未転写として取り込まれ、回収したテキストが失われる。
    #[test]
    fn a_successful_retranscription_releases_the_audio() {
        let db = TempDb::new("retranscribe-release");
        let (id, wav) = seed_pending(&db, "recovered");

        let updated = db
            .store
            .update_transcription(
                id,
                &TranscriptionUpdate {
                    raw_text: "回収した生転写",
                    formatted_text: "回収したテキスト。",
                    outcome: OUTCOME_FORMATTED,
                    outcome_reason: None,
                    stt_ms: 700,
                    format_ms: 900,
                },
            )
            .expect("更新できる");
        assert_eq!(updated, 1);

        assert!(!wav.exists(), "再転写後も WAV が残っている");
        let row = db.store.get(id).expect("読める").expect("行");
        assert_eq!(row.formatted_text.as_deref(), Some("回収したテキスト。"));
        assert!(!row.has_audio, "wav_path が残っている (幽霊行の元になる)");
        assert_eq!(db.store.wav_path(id).expect("読める"), None);
    }

    /// M-2 回帰: 再転写 → 保持期限 の順でテキストが失われないこと。
    ///
    /// 「期限で行が消える → 残った WAV が未転写として復活する」の
    /// 一段上のループを塞げているかを見る。
    #[test]
    fn retranscribed_text_survives_the_retention_pass() {
        let db = TempDb::new("retranscribe-retention");
        let (id, wav) = seed_pending(&db, "old-recovered");

        db.store
            .update_transcription(
                id,
                &TranscriptionUpdate {
                    raw_text: "生",
                    formatted_text: "回収済みのテキスト。",
                    outcome: OUTCOME_FORMATTED,
                    outcome_reason: None,
                    stt_ms: 1,
                    format_ms: 1,
                },
            )
            .expect("更新できる");

        // 保持期限を執行する (この行はまだ新しいので消えない)。
        assert_eq!(db.store.purge_older_than(30).expect("消せる"), 0);
        let row = db.store.get(id).expect("読める").expect("行");
        assert_eq!(row.formatted_text.as_deref(), Some("回収済みのテキスト。"));

        // WAV は既に無いので、再取り込みで幽霊行が湧くこともない。
        assert!(!wav.exists());
    }

    /// 保持期限は「WAV を持っている行」を消さない。
    ///
    /// 判定を outcome ではなく wav_path で行うのが要点 —
    /// 再転写でファイルを手放せなかった行も守られる。
    #[test]
    fn retention_never_touches_rows_that_still_own_audio() {
        let db = TempDb::new("retention-owns-audio");
        let day = 24 * 60 * 60 * 1_000u64;
        let (pending_id, _) = seed_pending(&db, "still-owned");

        // 古い時刻へ書き換える (期限の対象になる年代)。
        {
            let conn = db.store.connect().expect("開ける");
            conn.execute(
                "UPDATE sessions SET started_at_ms = ?2 WHERE id = ?1",
                params![pending_id, (now_ms() - 400 * day) as i64],
            )
            .expect("更新できる");
        }
        let old_plain = db.store.insert(&draft(now_ms() - 400 * day)).expect("書ける");

        assert_eq!(db.store.purge_older_than(30).expect("消せる"), 1);
        assert!(
            db.store.get(pending_id).expect("読める").is_some(),
            "音声を持つ行が消された (取り込み → 削除の無限ループになる)"
        );
        assert!(db.store.get(old_plain).expect("読める").is_none());
    }

    /// 削除中に行が消えていた場合、更新は 0 行を返す (静かに成功しない)。
    #[test]
    fn updating_a_deleted_row_reports_zero() {
        let db = TempDb::new("update-deleted");
        let (id, _) = seed_pending(&db, "vanishing");
        db.store.delete(id).expect("消せる");

        let updated = db
            .store
            .update_transcription(
                id,
                &TranscriptionUpdate {
                    raw_text: "生",
                    formatted_text: "整形",
                    outcome: OUTCOME_FORMATTED,
                    outcome_reason: None,
                    stt_ms: 1,
                    format_ms: 1,
                },
            )
            .expect("エラーにはしない");
        assert_eq!(updated, 0, "消えた行への更新が成功扱いになっている");
    }

    #[test]
    fn removing_missing_files_is_not_an_error() {
        // 既に無い = 目的 (残っていないこと) は達成されている。
        let dir = std::env::temp_dir().join(format!("nox-history-nofile-{}", std::process::id()));
        let path = dir.join("does-not-exist.wav");
        assert!(remove_wav_files(&path.to_string_lossy()).is_ok());
    }

    // --- マイグレーション ---

    #[test]
    fn initialize_is_idempotent() {
        let db = TempDb::new("migrate-idempotent");
        let id = db.store.insert(&draft(1)).expect("書ける");
        // 起動のたびに走る。既存データを壊さない。
        db.store.initialize().expect("再初期化できる");
        db.store.initialize().expect("再初期化できる");
        assert!(db.store.get(id).expect("読める").is_some());
    }

    #[test]
    fn migration_stamps_the_schema_version() {
        let db = TempDb::new("migrate-version");
        let conn = db.store.connect().expect("開ける");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("読める");
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn a_fresh_database_migrates_from_zero() {
        let db = TempDb::new("migrate-fresh");
        // TempDb は initialize 済み。素の接続で版 0 から上がることを確認する。
        let conn = db.store.connect().expect("開ける");
        conn.pragma_update(None, "user_version", 0i64).expect("戻せる");
        conn.execute_batch("DROP TABLE sessions").expect("落とせる");
        drop(conn);

        db.store.initialize().expect("再作成できる");
        assert_eq!(db.store.count().expect("読める"), 0);
        db.store.insert(&draft(1)).expect("書ける");
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_corrupted() {
        // 将来版の DB を古いアプリで開いたら、勝手に触らず止める。
        let db = TempDb::new("migrate-newer");
        let conn = db.store.connect().expect("開ける");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("上げられる");
        drop(conn);

        let err = db.store.initialize().expect_err("拒否される");
        assert!(matches!(err, HistoryError::Migrate(_)), "{err:?}");
        assert!(err.to_string().contains("新しい"), "{err}");
    }
}
