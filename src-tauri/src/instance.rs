//! 多重起動の防止 (名前付きミューテックスによる本丸)。
//!
//! # なぜプラグインだけでは足りないのか
//!
//! `tauri-plugin-single-instance` は既に入れてある。にもかかわらず
//! 「たまに 2 つ立ち上がる」が実際に起きる。プラグインの Windows 実装
//! (2.4.3) を読むと、漏れる経路が 2 つある。
//!
//! 1. **起動の競合**。プラグインは `CreateMutexW` の直後に
//!    `FindWindowW` で既存インスタンスの隠しメッセージウィンドウを探し、
//!    **見つかったときだけ** `exit(0)` する。見つからなければ早期 return も
//!    警告もなく、**そのまま 2 個目の起動を続行する**。
//!
//!    1 個目のミューテックス作成とウィンドウ作成は同じ `setup` クロージャ内で
//!    連続して走るので、隙間そのものはミリ秒級である。**それでも穴は穴**で、
//!    2 個目の `CreateMutexW` がちょうどそのミリ秒に重なれば
//!    「ミューテックスはあるがウィンドウは無い」を見て素通りする。
//!    しかもこの判定はアプリ初期化のかなり後 (`tauri::Builder::setup`) にあるため、
//!    2 プロセスが同時に起動したときは**両方が揃ってそこへ到達しやすい**。
//!    スタートアップ登録 + 手動起動や、ペダル / ランチャの二重発火が該当する。
//!
//! 2. **昇格レベルの不一致**。プラグインは `GetLastError()` が
//!    `ERROR_ALREADY_EXISTS` のときしか「既に起動中」と見なさない。
//!    1 個目が管理者昇格していると、そのミューテックスは既定 DACL のため
//!    非昇格の 2 個目からは開けず、`CreateMutexW` は
//!    `ERROR_ACCESS_DENIED` で失敗する。プラグインはこれを
//!    「まだ誰も起動していない」と解釈して `else` 側へ進み、
//!    **NULL ハンドルを抱えたまま 2 個目を丸ごと起動してしまう**。
//!
//! # ここでやること
//!
//! `run()` の 1 行目、Tauri のビルダーに触る前に名前付きミューテックスを取る。
//! カーネルオブジェクトの生成はアトミックで、ウィンドウ生成より桁違いに速い。
//! したがって 1 の競合は原理的に消える。2 は `ERROR_ACCESS_DENIED` も
//! 「既に起動中」として扱うことで塞ぐ。
//!
//! プラグインは**残す**。役割分担は次のとおり。
//!
//! - ミューテックス (このモジュール) = 正しさ。絶対に 2 つ動かさない。
//! - プラグイン = UX。2 個目を起動したら既存のウィンドウを前に出す。
//!
//! ミューテックスで先に `exit` すると、プラグインの「既存ウィンドウを出す」が
//! 効かなくなる。そこで 2 個目は終了する前に、プラグインが待ち受けている
//! メッセージウィンドウへ自分で `WM_COPYDATA` を送る ([`notify_existing_instance`])。
//! 送る形式はプラグインの実装に合わせてある。

use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, LPARAM, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::SystemInformation::GetSystemTime;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_COPYDATA,
};

/// アプリ識別子。`tauri.conf.json` の `identifier` と一致させること。
///
/// プラグインが作るウィンドウ名 / クラス名もこれを基にしているので、
/// ここがずれると 2 個目が既存インスタンスを見つけられなくなる。
///
/// `pub` にしてあるのは疑似 E2E (`sim` モジュール) のため。あちらは**テスト専用の
/// 名前**を使うが、「本番名と衝突していないこと」を毎回突き合わせて確かめる。
/// 衝突したまま走ると、テストが利用者の常駐アプリのウィンドウを叩き、
/// 頼んでもいない設定画面が開く。
pub const APP_IDENTIFIER: &str = "com.noxitro.nox-voice";

/// ミューテックス名。
///
/// `Local\` (ログオンセッション単位) にするのは意図的。`Global\` にすると
/// **別ユーザーが同時ログオンしているときに片方しか起動できなくなる**。
/// 本アプリはユーザーごとの設定・履歴・ホットキーを持つ常駐アプリなので、
/// 「1 ログオンセッションにつき 1 つ」が正しい粒度。
///
/// (`Global\` が作れないから `Local\` にした、のではない。対話ユーザーは
/// 通常 `Global\` も作れる — `SeCreateGlobalPrivilege` が問題になるのは
/// サービス等の文脈。ここは**作れるかどうかではなく、どの粒度で 1 つに
/// したいか**で選んでいる。)
/// (`pub` の理由は [`APP_IDENTIFIER`] と同じ。)
pub const MUTEX_NAME: &str = concat!(r"Local\", "com.noxitro.nox-voice", "-single-instance");

/// プラグイン (`tauri-plugin-single-instance` 2.4.3) が使う `WM_COPYDATA` の識別値。
///
/// この値が一致しないと相手は黙って無視する。プラグイン側の定数と揃えてある。
const WMCOPYDATA_SINGLE_INSTANCE_DATA: usize = 1542;

/// 既存インスタンスへの通知を諦めるまで。
///
/// `SendMessageTimeoutW` は相手が応答するまでブロックする。相手が
/// 転写中などで一時的に重いと、2 個目がここで固まって
/// 「起動したのに何も起きない」プロセスが残る。必ず時間で畳む。
///
/// # 3 秒でよい理由 (疑似 E2E の実測を踏まえて)
///
/// 実測: メッセージを一切回さない受信ウィンドウ相手で **3002 ms** で戻った。
/// **畳んでいるのはこの定数であって `SMTO_ABORTIFHUNG` ではない**
/// (Windows のハング判定は無応答およそ 5 秒後なので、そちらが働くより先に
/// ここが満了する)。つまり最悪ケースの滞留時間はこの値そのものである。
///
/// この 3 秒のあいだ、2 個目は `run()` の 1 行目に居るだけで Tauri の初期化に
/// 入っていない。**フックを刺さず、設定ファイルを開かず、トレイも作らない**。
/// 二重起動の実害 (フックが 2 本 / 設定の奪い合い / ログの切り詰め) は
/// どれも起きようがなく、代償は「タスクマネージャに 2 つ見える時間が最大 3 秒」
/// だけ。正しさは崩れていないので、ここは短さより確実さを取ってよい。
///
/// **短くしない理由**: 通常経路ではマイクロ秒で返るので、この値が効くのは
/// 異常時だけである。効く場面の代表は「1 個目が起動直後で、WebView2 の
/// 立ち上げ・トレイ構築・オーバーレイ生成でメインスレッドが 1 秒級に詰まって
/// いる」ところへ 2 個目が来たとき。1 秒に切り詰めると、この場面で
/// **利用者が意図的に 2 回目を起動したのにウィンドウが出ない**。
/// 得られるのは高々 2 秒早くプロセスが消えることだけで、割に合わない。
///
/// なお満了は「届かなかったことの証明」ではない。`SendMessageTimeoutW` が
/// 諦めたあとに相手がメッセージを処理することはありうる (その場合はこちらが
/// 「届かず」とログに書いたのに窓が出る)。ログの文言はそれを踏まえてある。
///
/// 変更するときは `e2e/single-instance.mjs` の `SETTLE` (2 個目の終了を待つ
/// 時間) がこの値より確実に長いことを併せて確認すること。
const NOTIFY_TIMEOUT_MS: u32 = 3_000;

/// ミューテックスハンドルの保管場所。
///
/// **プロセスが終わるまで持ち続けること**。閉じるとカーネルオブジェクトが
/// 消え、次の起動が「誰も起動していない」と判断してしまう。
/// 明示的な解放は書かない (プロセス終了時に OS が回収する)。
static MUTEX_HANDLE: OnceLock<usize> = OnceLock::new();

/// `CreateMutexW` の結果から導いた起動可否。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupDecision {
    /// 自分が最初。起動してよい。
    Proceed,
    /// 既に起動している。静かに終了する。
    AlreadyRunning,
    /// 判定できなかった。**起動は許す**が警告を出す (エラーコードつき)。
    ProceedUnverified(u32),
}

/// `CreateMutexW` の結果コードから「起動してよいか」を決める。
///
/// Win32 呼び出しそのものはテストできないので、判定だけを純関数に切り出す。
///
/// - `acquired`: `CreateMutexW` が有効なハンドルを返したか
/// - `error_code`: 直後の `GetLastError()` (ハンドルを得られなかった場合は失敗理由)
pub fn decide_startup(acquired: bool, error_code: u32) -> StartupDecision {
    match (acquired, error_code) {
        // 既存のミューテックスを開いた = 誰かが先にいる。
        (true, code) if code == ERROR_ALREADY_EXISTS.0 => StartupDecision::AlreadyRunning,
        // ハンドルが取れた。`GetLastError` は成功時に 0 以外の残骸を返すことが
        // あるので、`ERROR_ALREADY_EXISTS` 以外は「自分が最初」と読む。
        (true, _) => StartupDecision::Proceed,
        // **ここが昇格ケース**。昇格プロセスが作ったミューテックスは既定 DACL の
        // ため非昇格からは開けず、アクセス拒否になる。これを「起動していない」と
        // 読むと、まさに二重起動が起きる。既に起動している側へ倒す。
        (false, code) if code == ERROR_ACCESS_DENIED.0 => StartupDecision::AlreadyRunning,
        // それ以外の失敗 (リソース枯渇など) は原因が分からない。
        // ここで終了させるとアプリが**永久に起動できなくなる**ので、
        // 起動を許す側に倒して警告だけ残す。二重起動は「たまに起きる不便」だが、
        // 起動できないのは「使えない」なので、天秤は起動側に傾ける。
        (false, code) => StartupDecision::ProceedUnverified(code),
    }
}

/// [`try_acquire_mutex`] の結果。
///
/// `decision` だけでなく生ハンドルとエラーコードも返すのは、呼び出し側が
/// **ハンドルを持ち続ける責任**を負うため (閉じるとカーネルオブジェクトが
/// 消えて、次の起動が「誰もいない」と判断してしまう)。
#[derive(Debug)]
pub struct MutexAttempt {
    pub decision: StartupDecision,
    /// 取れた生ハンドル。**閉じてはいけない**。プロセス終了で OS が回収する。
    pub handle: Option<usize>,
    /// `CreateMutexW` 直後の `GetLastError()` (失敗時は失敗理由)。
    pub error_code: u32,
}

/// 名前付きミューテックスを取り、[`decide_startup`] で起動可否まで出す。
///
/// # なぜ名前を引数にしたのか
///
/// 本番は [`MUTEX_NAME`] 固定で構わない。それでも引数にしてあるのは、
/// 疑似 E2E (`sim` モジュール) が**この関数そのもの**を 2 プロセスから叩いて
/// 「ちょうど 1 つだけが通る」を確かめられるようにするため。テスト用に
/// 別実装を書いてしまうと、確かめているのはテスト用の写しであって
/// 本番の経路ではなくなる。名前だけを差し替えれば、通る道は完全に同じになる。
///
/// テストが本番名を使うと、開発機で動いている常駐アプリのミューテックスに
/// ぶつかって結果が環境依存になり、しかも**利用者のアプリの挙動を変えうる**。
/// 名前を外から渡せることが、その事故を避ける唯一の手段でもある。
pub fn try_acquire_mutex(mutex_name: &str) -> MutexAttempt {
    let name = encode_wide(mutex_name);

    // SAFETY: 名前は NUL 終端の UTF-16 で、`name` はこの呼び出しの間だけ
    // 参照される。セキュリティ属性は既定 (NULL)。所有権は取らない
    // (`false`) — 待機には使わず「存在するか」だけを見るため。
    let result = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) };

    let (acquired, error_code) = match &result {
        // SAFETY: 引数なし。`CreateMutexW` の直後なので、成功時でも
        // `ERROR_ALREADY_EXISTS` がここに載っている (これが判定の要)。
        Ok(_) => (true, unsafe { GetLastError() }.0),
        // windows クレートは失敗時に `GetLastError` を HRESULT へ包んで返す。
        // 下位 16bit が元の Win32 エラーコード。
        Err(e) => (false, (e.code().0 as u32) & 0xFFFF),
    };

    MutexAttempt {
        decision: decide_startup(acquired, error_code),
        handle: result.ok().map(|h| h.0 as usize),
        error_code,
    }
}

/// 多重起動を防ぐ。既に起動していれば**この関数から戻らない**。
///
/// `run()` の 1 行目で呼ぶこと。Tauri のビルダーに触ってからでは、
/// 1 個目のウィンドウ生成との競合に間に合わない。
pub fn ensure_single_instance() {
    let attempt = try_acquire_mutex(MUTEX_NAME);
    let error_code = attempt.error_code;

    match attempt.decision {
        StartupDecision::Proceed => {
            if let Some(handle) = attempt.handle {
                // プロセス生存期間中ずっと持つ。閉じたら意味がない。
                let _ = MUTEX_HANDLE.set(handle);
            }
        }
        StartupDecision::ProceedUnverified(code) => {
            if let Some(handle) = attempt.handle {
                let _ = MUTEX_HANDLE.set(handle);
            }
            early_log(
                "WARN",
                &format!(
                    "多重起動の判定ができませんでした (CreateMutexW: エラー {code})。\
                     判定できないまま起動を止めると復旧手段が無くなるので、起動を続行します"
                ),
            );
        }
        StartupDecision::AlreadyRunning => {
            // 終了する前に既存インスタンスへ知らせる。ここを飛ばすと
            // プラグインが担っていた「既存のウィンドウを前に出す」が失われ、
            // 利用者から見て「起動したのに何も起きない」アプリになる。
            let notified = notify_existing_instance();

            // **必ずログを 1 行残す**。ここで無言で消えると、利用者には
            // 「たまに起動しない謎のアプリ」にしか見えず、原因を追えない。
            // 文言は「起動できなかった」ではなく「既に起動しているので終了した」。
            early_log(
                "INFO",
                &format!(
                    "nox-voice は既に起動しています (CreateMutexW: エラー {error_code})。\
                     多重起動を避けるため、この 2 個目のプロセスは終了します \
                     (既存インスタンスへの通知: {})",
                    if notified { "成功" } else { "届かず" }
                ),
            );
            std::process::exit(0);
        }
    }
}

/// 既存インスタンスへ「起動しようとした」と伝える。届いたら `true`。
///
/// プラグインが `tauri::Builder::setup` で作る隠しウィンドウを探して
/// `WM_COPYDATA` を送る。受け取った側はコールバック
/// (`lib.rs` の `tauri_plugin_single_instance::init`) を回してウィンドウを出す。
///
/// # 見つからないときに待たない理由
///
/// ウィンドウが無いのは「1 個目がまだ起動中」のとき、つまり
/// **スタートアップと手動起動が同時に走った競合の最中**である。この場面で
/// 待ってまでウィンドウを出させると、利用者が頼んでもいない設定画面が
/// 起動のたびに開くことになる。一方、利用者が意図的に 2 回目を起動した
/// ケースでは 1 個目はとうに立ち上がっており、ここは一発で見つかる。
/// つまり「見つかったときだけ出す」で、欲しい UX はちょうど満たされる。
///
/// # 昇格が食い違うとき
///
/// 1 個目が昇格していると UIPI で `WM_COPYDATA` が遮断され、届かない。
/// それでも 2 個目は終了する — 通知は UX、終了は正しさであり、
/// UX の失敗を理由に正しさを曲げない。
fn notify_existing_instance() -> bool {
    let (class_name, window_name) = instance_window_names();
    notify_instance_window(&class_name, &window_name)
}

/// プラグインが待ち受けるウィンドウの (クラス名, ウィンドウ名)。
///
/// プラグイン側 (`platform_impl/windows.rs`) が `{identifier}-sic` /
/// `{identifier}-siw` で作るので、それに合わせてある。
pub fn instance_window_names() -> (String, String) {
    (
        format!("{APP_IDENTIFIER}-sic"),
        format!("{APP_IDENTIFIER}-siw"),
    )
}

/// 指定のクラス名 / ウィンドウ名を探して `WM_COPYDATA` を送る。届いたら `true`。
///
/// # なぜ名前を引数にしたのか
///
/// [`try_acquire_mutex`] と同じ理由。疑似 E2E (`sim` モジュール) は自前の受信
/// ウィンドウを立てて「本当に届くか・中身が壊れないか」を確かめるが、
/// **本番のクラス名で探させると、開発機で動いている常駐アプリの
/// ウィンドウを叩いてしまう** (利用者の設定画面が勝手に開く)。
/// 名前を差し替えられることが、送信経路を実プロセス間で試すための前提になる。
pub fn notify_instance_window(class_name: &str, window_name: &str) -> bool {
    let class_name = encode_wide(class_name);
    let window_name = encode_wide(window_name);

    // SAFETY: どちらも NUL 終端の UTF-16。見つからなければ Err が返る。
    let hwnd = unsafe { FindWindowW(PCWSTR(class_name.as_ptr()), PCWSTR(window_name.as_ptr())) };
    let Ok(hwnd) = hwnd else {
        return false;
    };
    if hwnd.0.is_null() {
        return false;
    }

    // プラグインが期待する形式: "{cwd}|{argv を | で連結}\0" の C 文字列。
    // 受け側は `CStr::from_ptr` で読むので、NUL 終端が無いと読み越す。
    let cwd = std::env::current_dir().unwrap_or_default();
    let args = std::env::args().collect::<Vec<_>>().join("|");
    let payload = format!("{}|{}\0", cwd.to_string_lossy(), args);
    let bytes = payload.as_bytes();

    let cds = COPYDATASTRUCT {
        dwData: WMCOPYDATA_SINGLE_INSTANCE_DATA,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr() as *mut _,
    };

    // SAFETY: hwnd は FindWindowW 由来の有効なウィンドウ。cds と payload は
    // この呼び出しが返るまで生きている (SendMessageTimeoutW は同期)。
    //
    // 相手が固まっていても戻ってくることは疑似 E2E で実測済み。ただし
    // **畳んでいるのは NOTIFY_TIMEOUT_MS であって SMTO_ABORTIFHUNG ではない**:
    // 応答しない相手への送信は 3002 ms かかった (= 自前のタイムアウトの満了)。
    // Windows がスレッドをハングと見なすのは無応答およそ 5 秒後なので、
    // このフラグが働くより先にこちらが満了する。フラグは残してあるが、
    // 「これがあるから固まらない」と読んではいけない。実際に効いているのは
    // タイムアウト値のほうで、安全性はそちらに依存している。
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG,
            NOTIFY_TIMEOUT_MS,
            None,
        )
    };
    sent.0 != 0
}

/// ロガー初期化前に使える緊急ログ。
///
/// `run()` の 1 行目は `tauri_plugin_log` より前なので、`log::info!` は
/// **どこにも出ない**。ここで無言になると、この機能はまるごと
/// 「調べられない挙動」になってしまうので、ログファイルへ直接追記する。
///
/// 書き込み先とフォーマットは `tauri_plugin_log` の `LogDir` ターゲットに
/// 合わせてある (時刻が UTC なのもそのため。[`log_prefix`] を参照)。
/// あちらは追記モードで開いているので、別プロセスから 1 行足しても
/// 取りこぼしにはならない。失敗しても起動は続ける。
fn early_log(level: &str, message: &str) {
    // 端末から起動した人にはこちらが見える。
    eprintln!("[nox-voice][{level}] {message}");

    let Some(dir) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let path = std::path::Path::new(&dir)
        .join(APP_IDENTIFIER)
        .join("logs")
        .join("nox-voice.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{} {message}", log_prefix(level));
    }
}

/// `tauri_plugin_log` と同じ体裁の行頭を作る。
///
/// 例: `[2026-08-27][09:12:33][nox_voice_lib][INFO]`
/// 既存の行と揃えておかないと、利用者がログを追うときに見落とす。
///
/// # なぜ UTC なのか (`GetLocalTime` ではなく `GetSystemTime`)
///
/// `tauri_plugin_log` の既定は `TimezoneStrategy::UseUtc` で、本アプリは
/// これを変えていない。つまり**ログファイルの既存行はすべて UTC** である。
/// ここでローカル時刻を書くと、JST 環境ではこの 1 行だけ +9 時間ずれ、
/// 「2 個目が終了した」行が未来の時刻でファイルに挟まる。障害調査では
/// 時系列こそが手がかりなので、これは誤読を生む。
///
/// 選択肢は「UTC で書く」「行内に (local) と明記する」「アプリ側を
/// `UseLocal` へ揃える」の 3 つあったが、**UTC で書く**を選んだ。理由は、
/// 他の 2 つがどちらもログ全体の見え方を変える (= 既存の運用・過去ログとの
/// 突き合わせに影響する) のに対し、これだけが**既存の行に何も影響しない**ため。
/// この関数はアプリの表示仕様ではなく「既存フォーマットへの追従」なので、
/// 追従する側が合わせるのが筋である。
fn log_prefix(level: &str) -> String {
    // SAFETY: 引数なし。戻り値は値渡しの SYSTEMTIME。
    let t = unsafe { GetSystemTime() };
    format!(
        "[{:04}-{:02}-{:02}][{:02}:{:02}:{:02}][nox_voice_lib][{level}]",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// NUL 終端の UTF-16 へ。
fn encode_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 素直に取れたときは起動する。
    #[test]
    fn fresh_mutex_proceeds() {
        assert_eq!(decide_startup(true, 0), StartupDecision::Proceed);
    }

    /// 既にある = 先客がいる。ここが本筋。
    #[test]
    fn already_exists_means_another_instance() {
        assert_eq!(
            decide_startup(true, ERROR_ALREADY_EXISTS.0),
            StartupDecision::AlreadyRunning
        );
    }

    /// **昇格ケース**。1 個目が管理者だと非昇格の 2 個目はアクセス拒否になる。
    /// これを「起動していない」と読むとちょうど二重起動が起きるので、
    /// 起動中として扱わなければならない。
    #[test]
    fn access_denied_means_elevated_instance_is_running() {
        assert_eq!(
            decide_startup(false, ERROR_ACCESS_DENIED.0),
            StartupDecision::AlreadyRunning
        );
    }

    /// 判定不能な失敗でアプリを起動不能にしない。
    ///
    /// 二重起動は「たまに起きる不便」だが、起動できないのは「使えない」。
    /// 未知のエラーでは起動を許し、警告だけ残す。
    #[test]
    fn unknown_error_still_allows_startup() {
        // ERROR_NOT_ENOUGH_MEMORY (8) を例に。
        assert_eq!(
            decide_startup(false, 8),
            StartupDecision::ProceedUnverified(8)
        );
        // 0 で失敗する (原因不明) ケースも起動側へ倒す。
        assert_eq!(
            decide_startup(false, 0),
            StartupDecision::ProceedUnverified(0)
        );
    }

    /// ハンドルが取れていれば、`GetLastError` の残骸に引きずられない。
    ///
    /// `CreateMutexW` は成功時に `GetLastError` を 0 に**しない**ことがあり、
    /// 前の API の残骸が見える。`ERROR_ALREADY_EXISTS` 以外を「先客あり」と
    /// 読んでしまうと、正当な起動が黙って落ちる。
    #[test]
    fn stale_error_with_valid_handle_proceeds() {
        assert_eq!(
            decide_startup(true, ERROR_ACCESS_DENIED.0),
            StartupDecision::Proceed
        );
        assert_eq!(decide_startup(true, 8), StartupDecision::Proceed);
    }

    /// ミューテックス名の取り決め。
    ///
    /// `Local\` を落とすと (= 既定の名前空間) 挙動は同じだが明示性が失われ、
    /// `Global\` にすると別ユーザーの同時ログオンで片方が起動できなくなる。
    /// 名前には識別子を含め、他アプリと衝突させない。
    #[test]
    fn mutex_name_is_session_scoped_and_identifiable() {
        assert!(MUTEX_NAME.starts_with(r"Local\"));
        assert!(MUTEX_NAME.contains(APP_IDENTIFIER));
        // 名前空間の区切り以外に `\` を含めてはいけない (CreateMutexW が失敗する)。
        assert_eq!(MUTEX_NAME.matches('\\').count(), 1);
    }

    /// ログ行はプラグインのログと同じ体裁で出す (利用者が見落とさないため)。
    #[test]
    fn log_prefix_matches_plugin_format() {
        let prefix = log_prefix("INFO");
        assert!(prefix.ends_with("[nox_voice_lib][INFO]"));
        // `[YYYY-MM-DD][HH:MM:SS][target][LEVEL]` = 括弧 4 組。
        assert_eq!(prefix.matches('[').count(), 4);
        assert_eq!(
            prefix.len(),
            "[2026-08-27][09:12:33][nox_voice_lib][INFO]".len()
        );
    }

    /// ログの時刻は **UTC** で書く (`tauri-plugin-log` の既定に合わせるため)。
    ///
    /// ここが `GetLocalTime` に戻ると、JST 環境では既存行と 9 時間ずれ、
    /// 「2 個目が終了した」行が未来の時刻でログに挟まる。時系列が手がかりの
    /// 障害調査で誤読を生むので、退行を検出できるようにしておく。
    ///
    /// 日付をまたぐ計算を避けるため「時刻の時」だけを UNIX 時間から出して比べる。
    /// (UTC で動いている環境ではこのテストは差を見つけられないが、
    ///  ずれが問題になるのはローカル時刻が UTC と違う環境だけなので支障ない。)
    #[test]
    fn log_prefix_uses_utc_not_local_time() {
        let prefix = log_prefix("INFO");
        let hour: u64 = prefix[13..15].parse().expect("時刻の位置が変わった");

        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("システム時計が UNIX_EPOCH より前")
            .as_secs();
        let utc_hour = (unix % 86_400) / 3_600;

        // 秒境界で時が繰り上がることがあるので隣も許す。
        let ok = hour == utc_hour || hour == (utc_hour + 1) % 24;
        assert!(
            ok,
            "log_prefix の時 {hour} が UTC の時 {utc_hour} と一致しない"
        );
    }

    /// UTF-16 化は必ず NUL 終端する (終端が無いと Win32 が読み越す)。
    #[test]
    fn encode_wide_is_nul_terminated() {
        let w = encode_wide("ab");
        assert_eq!(w, vec![b'a' as u16, b'b' as u16, 0]);
    }
}

/// 疑似 E2E (simulated E2E)。実プロセスを 2 つ起こして機構そのものを試す。
///
/// # 実機 E2E (`e2e/single-instance.mjs`) との違い
///
/// あちらは **`nox-voice.exe` を 2 個起動して、生き残ったプロセス数を数える**。
/// こちらは exe を一切起動しない。代わりに**このテストバイナリ自身**を
/// 子プロセスとして起こし、[`try_acquire_mutex`] / [`notify_instance_window`]
/// という**本番がまさに通る関数**を、名前だけテスト用に差し替えて叩く。
///
/// なぜ exe を使わないのか: `nox-voice.exe` はグローバルキーボードフックを
/// 張る常駐アプリで、開発機では利用者のインスタンスが動いている。もう 1 つ
/// 起こすとフックが二重に効き、1 回のホットキーで録音が 2 回始まる。設定
/// ファイルと履歴 DB も奪い合う。**開発者が仕事をしている間は走らせられない**
/// のが実機 E2E の泣きどころで、ここはその穴を別の角度から埋める。
///
/// ## ここで確かめられること
///
/// - 2 つの OS プロセスが同じ名前のミューテックスを**同時刻に**取り合ったとき、
///   `Proceed` はちょうど 1 つで、もう 1 つは必ず `AlreadyRunning` になる
///   (`<= 1` ではなく「ちょうど 1」。両方弾かれる退行 = アプリが起動しなくなる
///   故障を緑で通さないため)
/// - `WM_COPYDATA` が**プロセス境界を越えて**届き、`dwData` が 1542 で、
///   ペイロードが `{cwd}|{argv}\0` の UTF-8 として復元でき、日本語や空白を
///   含むパスでも壊れないこと
/// - 受信ウィンドウが無いとき、送信側が待たずに諦めること
/// - 受信ウィンドウが応答しないとき、送信側が `NOTIFY_TIMEOUT_MS` で畳んで
///   戻ってくること (2 個目が固まったまま残らない)
///
/// ## ここでは確かめられないこと (実機 E2E にしかできない)
///
/// - [`ensure_single_instance`] が `run()` の 1 行目にあること、つまり
///   **Tauri の初期化より前に**判定が終わること。ここで呼ぶのは
///   [`try_acquire_mutex`] までで、`std::process::exit(0)` する本体は呼べない
///   (テストプロセスごと死ぬ)
/// - 2 個目が本当に終了し、常駐が 1 プロセスだけになること
/// - プラグイン側の受信ハンドラが動いて**既存ウィンドウが前に出る**こと。
///   ここの受信側はプラグインの窓プロシージャを写した別実装であり、
///   「同じ形式なら受け取れる」ことは示せても「プラグインが受け取った」ではない
/// - 昇格 (`ERROR_ACCESS_DENIED`) 経路。理由は `probe_mutex` の項に書いた
#[cfg(test)]
mod sim {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use windows::Win32::Foundation::{HWND, LRESULT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassExW, SetWindowLongPtrW, TranslateMessage, GWL_STYLE, MSG, PM_REMOVE,
        WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
        WS_OVERLAPPED, WS_POPUP, WS_VISIBLE,
    };

    /// 子プロセスに役を伝える環境変数。無ければ子役のテストは何もしない
    /// (`cargo test -- --ignored` で素通しされても落ちないように)。
    const ENV_MUTEX_NAME: &str = "NOX_SIM_MUTEX_NAME";
    const ENV_HOLD_MS: &str = "NOX_SIM_HOLD_MS";
    const ENV_START_AT_MS: &str = "NOX_SIM_START_AT_MS";
    const ENV_CLASS: &str = "NOX_SIM_CLASS";
    const ENV_WINDOW: &str = "NOX_SIM_WINDOW";

    /// 同時起動を試す回数。競合は確率的なので 1 回では意味がない。
    fn race_rounds() -> usize {
        std::env::var("NOX_SIM_RACE_ROUNDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
    }

    /// テスト専用の名前を作る。**本番名と絶対に重ねない**。
    ///
    /// 重ねると (1) 開発機で動いている利用者のアプリの状態に結果が左右され、
    /// (2) `WM_COPYDATA` が利用者のアプリへ飛んで設定画面が勝手に開く。
    /// pid と連番を混ぜるのは、同じ機で複数の `cargo test` が並走しても
    /// 名前がぶつからないようにするため。
    fn unique(kind: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        format!(
            "{APP_IDENTIFIER}-simtest-{kind}-{}-{}",
            std::process::id(),
            n
        )
    }

    /// 現在の UNIX 時刻 (ms)。子プロセス同士の「せーの」に使う。
    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("システム時計が UNIX_EPOCH より前")
            .as_millis()
    }

    /// 指定時刻まで待つ。最後の 2ms はスピンで詰める。
    ///
    /// `sleep` だけで揃えると OS のタイマ粒度 (既定 15.6ms) の分だけ散らばり、
    /// 「ほぼ同時」が「数十 ms ずれ」になる。競合をわざと作るのが目的なので、
    /// 最後だけは CPU を回して詰める。
    fn spin_until_ms(target: u128) {
        loop {
            let now = now_ms();
            if now >= target {
                return;
            }
            let remaining = target - now;
            if remaining > 2 {
                std::thread::sleep(Duration::from_millis((remaining - 2) as u64));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    /// 自分自身 (テストバイナリ) を子プロセスとして起こすコマンドを作る。
    ///
    /// **専用の小さな exe をビルドしない**のは、そちらだと本番の依存関係から
    /// 切り離された写しを作ることになり、「本番と同じ経路を通った」と言えなく
    /// なるため。テストバイナリは本番の lib をそのままリンクしているので、
    /// 子プロセスが呼ぶ [`try_acquire_mutex`] は本番が呼ぶものと同一の実体。
    fn probe(test_name: &str) -> Command {
        let exe = std::env::current_exe().expect("テストバイナリのパスが取れない");
        let mut cmd = Command::new(exe);
        cmd.args([test_name, "--exact", "--ignored", "--nocapture"]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd
    }

    /// 子プロセスの stdout から `PROBE ...` の行を拾う。
    fn probe_line(output: &std::process::Output) -> String {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .find(|l| l.starts_with("PROBE "))
            .unwrap_or_else(|| {
                panic!(
                    "子プロセスが PROBE 行を出さなかった\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })
            .to_string()
    }

    // ===== A. ミューテックス機構 =============================================

    /// 子役: 名前付きミューテックスを取って結果を 1 行で報告し、しばらく持つ。
    ///
    /// # 昇格ケース (`ERROR_ACCESS_DENIED`) を疑似化できない理由
    ///
    /// あの経路が成立する条件は「**昇格したプロセスが先にミューテックスを
    /// 作っている**」ことで、既定 DACL のために非昇格側が開けずアクセス拒否に
    /// なる。再現するには子プロセスの片方を昇格して起こす必要があり、それには
    /// UAC の同意ダイアログが要る。テストからは出せない (出せたらそれはそれで
    /// 問題である)。`CreateMutexW` に厳しい DACL を渡して「拒否される名前」を
    /// 作る手も検討したが、それだと**拒否のさせ方が本番と別物**になり、
    /// 確かめているのは自作の DACL でしかない。判定そのものは純関数テスト
    /// `access_denied_means_elevated_instance_is_running` が押さえているので、
    /// ここは「疑似化できない」と明示して残す。
    #[test]
    #[ignore = "疑似 E2E の子プロセス役。親テストが環境変数付きで起動する"]
    fn probe_mutex() {
        let Ok(name) = std::env::var(ENV_MUTEX_NAME) else {
            return;
        };
        if let Some(at) = std::env::var(ENV_START_AT_MS)
            .ok()
            .and_then(|v| v.parse().ok())
        {
            spin_until_ms(at);
        }

        let attempt = try_acquire_mutex(&name);
        println!(
            "PROBE decision={:?} error={}",
            attempt.decision, attempt.error_code
        );
        // 親が読み終える前にバッファへ残っていると取りこぼす。
        std::io::stdout().flush().expect("stdout を流せない");

        // ハンドルは**閉じない**。持ったまま眠ることで、相手が
        // `AlreadyRunning` を踏む窓を作る。閉じると相手が Proceed になり、
        // このテストは「同時に取り合った」を測れなくなる。
        let hold: u64 = std::env::var(ENV_HOLD_MS)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900);
        std::thread::sleep(Duration::from_millis(hold));
    }

    /// テスト用の名前が本番名と衝突していないこと。
    ///
    /// **これが崩れると、以降のテストは利用者の常駐アプリを巻き込む。**
    /// 他のどのテストより先に効いてほしい前提なので、独立して置く。
    #[test]
    fn sim_names_never_collide_with_production() {
        let mutex = format!(r"Local\{}", unique("mutex"));
        assert_ne!(mutex, MUTEX_NAME);
        let (prod_class, prod_window) = instance_window_names();
        let class = unique("class");
        let window = unique("window");
        assert_ne!(class, prod_class);
        assert_ne!(window, prod_window);
        // 本番名は `-sic` / `-siw` / `-single-instance` で終わる。テスト名が
        // 将来その形に近づいても気づけるよう、接尾辞の重なりも見ておく。
        assert!(!class.ends_with("-sic"), "{class}");
        assert!(!window.ends_with("-siw"), "{window}");
        assert!(!mutex.ends_with("-single-instance"), "{mutex}");
    }

    /// 誰も持っていない名前なら、単独の子プロセスは必ず `Proceed`。
    ///
    /// 陽性コントロール。これが落ちるなら、以降の「ちょうど 1 つ」は
    /// 「常に全部弾かれている」でも通ってしまう。
    #[test]
    fn lone_process_proceeds() {
        let name = format!(r"Local\{}", unique("lone"));
        let out = probe("instance::sim::probe_mutex")
            .env(ENV_MUTEX_NAME, &name)
            .env(ENV_HOLD_MS, "0")
            .output()
            .expect("子プロセスを起こせない");
        let line = probe_line(&out);
        assert!(line.contains("decision=Proceed"), "{line}");
    }

    /// 親が持っている間に来た子は `AlreadyRunning`。
    ///
    /// 陰性コントロール。競合を作らない決定的な形で「先客がいれば弾く」を見る。
    #[test]
    fn second_process_is_rejected_while_first_holds() {
        let name = format!(r"Local\{}", unique("held"));
        let held = try_acquire_mutex(&name);
        assert_eq!(held.decision, StartupDecision::Proceed);
        // ハンドルは閉じない。閉じるとこのテストの前提そのものが消える。

        let out = probe("instance::sim::probe_mutex")
            .env(ENV_MUTEX_NAME, &name)
            .env(ENV_HOLD_MS, "0")
            .output()
            .expect("子プロセスを起こせない");
        let line = probe_line(&out);
        assert!(line.contains("decision=AlreadyRunning"), "{line}");

        // ここまで `held` を生かしておくのがこのテストの前提。ハンドルを
        // 落とすとミューテックスごと消え、子が Proceed になってしまう。
        // 最後に触ることで「途中で最適化されて消える」ことも防ぐ。
        assert!(held.handle.is_some(), "親がハンドルを持てていない");
    }

    /// **本題**。ほぼ同時刻に 2 プロセスが取り合っても、通るのはちょうど 1 つ。
    ///
    /// 「1 つ以下」ではなく「ちょうど 1 つ」を毎回要求する。`<= 1` にすると、
    /// 互いに譲り合って**両方弾かれる**退行 (= アプリが二度と起動しない、
    /// 二重起動より重い故障) を緑で通してしまう。
    ///
    /// 同時性は環境変数で渡した UNIX 時刻を両者が待ち合わせることで作る。
    /// プロセス生成の時差 (数十 ms) をそのまま競合の時差にすると、
    /// 先に着いた方が毎回勝つだけの、競合になっていないテストになる。
    #[test]
    fn concurrent_start_lets_exactly_one_through() {
        let rounds = race_rounds();
        let mut log = Vec::new();
        for round in 0..rounds {
            let name = format!(r"Local\{}", unique(&format!("race{round}")));
            // 子プロセスの立ち上がりに十分な猶予を取る。ここが短すぎると
            // 片方が待ち合わせ時刻に間に合わず、競合そのものが消える。
            let start_at = now_ms() + 700;

            let spawn = || {
                probe("instance::sim::probe_mutex")
                    .env(ENV_MUTEX_NAME, &name)
                    .env(ENV_START_AT_MS, start_at.to_string())
                    .env(ENV_HOLD_MS, "600")
                    .spawn()
                    .expect("子プロセスを起こせない")
            };
            let a = spawn();
            let b = spawn();
            let out_a = a.wait_with_output().expect("子プロセス A が終わらない");
            let out_b = b.wait_with_output().expect("子プロセス B が終わらない");

            let line_a = probe_line(&out_a);
            let line_b = probe_line(&out_b);
            let proceeded = [&line_a, &line_b]
                .iter()
                .filter(|l| l.contains("decision=Proceed"))
                .count();
            let rejected = [&line_a, &line_b]
                .iter()
                .filter(|l| l.contains("decision=AlreadyRunning"))
                .count();
            log.push(format!("#{round}: A[{line_a}] B[{line_b}]"));
            assert_eq!(
                (proceeded, rejected),
                (1, 1),
                "同時起動で「ちょうど 1 つ」にならなかった\n{}\n\
                 (Proceed 2 = 競合が塞げていない / Proceed 0 = 両方が譲り合って落ちた)",
                log.join("\n")
            );
        }
        println!("{rounds} 回すべてで「ちょうど 1 つ」\n{}", log.join("\n"));
    }

    // ===== B. WM_COPYDATA の疎通 =============================================

    /// 子役: 指定のウィンドウへ `WM_COPYDATA` を送り、結果と所要時間を報告する。
    ///
    /// 送信を**別プロセスから**行うのが要点。同一プロセス内の別スレッドへ
    /// 送っても、`COPYDATASTRUCT` のカーネルによるマーシャリング
    /// (アドレス空間をまたいだバッファのコピー) は起きず、本番で壊れうる
    /// 部分をすり抜けてしまう。
    #[test]
    #[ignore = "疑似 E2E の子プロセス役。親テストが環境変数付きで起動する"]
    fn probe_copydata() {
        let (Ok(class), Ok(window)) = (std::env::var(ENV_CLASS), std::env::var(ENV_WINDOW)) else {
            return;
        };
        let started = Instant::now();
        let notified = notify_instance_window(&class, &window);
        println!(
            "PROBE notified={notified} elapsed_ms={}",
            started.elapsed().as_millis()
        );
        std::io::stdout().flush().expect("stdout を流せない");
    }

    /// 受け取った `WM_COPYDATA` の置き場。
    ///
    /// 窓プロシージャは `extern "system"` なので状態を持てない。プラグインは
    /// `GWLP_USERDATA` に自前の箱を刺しているが、ここで確かめたいのは
    /// 「受け取れたか・中身が壊れていないか」だけなので静的な置き場で足りる。
    static RECEIVED: Mutex<Vec<(usize, Vec<u8>)>> = Mutex::new(Vec::new());

    /// `tauri-plugin-single-instance` の窓プロシージャを写したもの。
    ///
    /// あちらは `dwData` が 1542 のときだけコールバックを回し、`LRESULT(1)` を
    /// 返す。送信側は戻り値が 0 でないことを「届いた」と読むので、
    /// **この 1 を返すところまでが疎通の定義**である。
    unsafe extern "system" fn sim_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_COPYDATA {
            // SAFETY: WM_COPYDATA の lParam は COPYDATASTRUCT へのポインタ。
            // 中身のバッファは送信側の SendMessageTimeoutW が返るまで有効。
            let cds = unsafe { &*(lparam.0 as *const COPYDATASTRUCT) };
            let bytes = if cds.lpData.is_null() {
                Vec::new()
            } else {
                // SAFETY: cbData バイト分が読める (カーネルが受信側の
                // アドレス空間へコピー済み)。ここで複製してから返す。
                unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) }
                    .to_vec()
            };
            RECEIVED
                .lock()
                .expect("受信バッファのロック")
                .push((cds.dwData, bytes));
            return LRESULT(1);
        }
        // SAFETY: 既定処理へ素通し。
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// プラグインと同じ作りの受信ウィンドウ。
    ///
    /// スタイルまで写しているのは、**`FindWindowW` で見つかるかどうかが
    /// ウィンドウの作り方に左右される**ため。メッセージ専用ウィンドウ
    /// (`HWND_MESSAGE` の子) にすると `FindWindowW` では見つからず、
    /// 「本番でも見つからないのでは」という肝心の疑問を素通りしてしまう。
    struct Receiver {
        hwnd: HWND,
    }

    impl Receiver {
        /// **必ず、メッセージを回すのと同じスレッドで作ること。**
        /// ウィンドウメッセージは作成スレッドのキューへ届く。
        fn create(class_name: &str, window_name: &str) -> Self {
            let class = encode_wide(class_name);
            let window = encode_wide(window_name);
            // SAFETY: 引数なし (自モジュール)。
            let hinstance = unsafe { GetModuleHandleW(None) }.expect("モジュールハンドル");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(sim_wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            // SAFETY: クラス名は一意 (`unique`) なので再登録衝突は起きない。
            let atom = unsafe { RegisterClassExW(&wc) };
            assert_ne!(atom, 0, "ウィンドウクラスを登録できない: {class_name}");

            // SAFETY: 直前に登録したクラスで生成する。親は無し (トップレベル) —
            // プラグイン側と同じ。lpParam は使わない。
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                    PCWSTR(class.as_ptr()),
                    PCWSTR(window.as_ptr()),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(hinstance.into()),
                    None,
                )
            }
            .expect("受信ウィンドウを作れない");
            // SAFETY: 直前に作った有効なウィンドウ。プラグインと同じ後始末。
            unsafe {
                SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_VISIBLE | WS_POPUP).0 as isize);
            }
            Self { hwnd }
        }

        /// 受信するかタイムアウトするまでメッセージを回す。
        fn pump_until_received(&self, timeout: Duration) -> Option<(usize, Vec<u8>)> {
            let deadline = Instant::now() + timeout;
            loop {
                let mut msg = MSG::default();
                // SAFETY: 自スレッドのキューを非ブロッキングで覗く。
                while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    // SAFETY: 直前に取り出した有効な MSG。
                    unsafe {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                if let Some(hit) = RECEIVED.lock().expect("受信バッファのロック").pop() {
                    return Some(hit);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for Receiver {
        fn drop(&mut self) {
            // 消さないと、後続のテストの `FindWindowW` が古い窓を拾う。
            // SAFETY: 自分が作った有効なウィンドウ。
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    /// 日本語と空白を含む作業ディレクトリを作る。
    ///
    /// ペイロードは `{cwd}|{argv}` なので、cwd が非 ASCII だと UTF-8 の
    /// マルチバイト列がそのまま `WM_COPYDATA` に載る。ここが壊れると
    /// 受信側の `CStr::from_ptr` が途中で切れたり、`|` の分割位置がずれる。
    /// 「日本語のユーザー名のホームからスタートアップで起動する」は
    /// **本アプリの標準的な使われ方**なので、素通しにはできない。
    fn japanese_cwd() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nox 疑似テスト 作業場 {}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("作業ディレクトリを作れない");
        dir
    }

    /// **本題**。別プロセスから送った `WM_COPYDATA` が、形式どおり届く。
    #[test]
    fn copydata_crosses_process_boundary_intact() {
        let class = unique("class");
        let window = unique("window");
        let receiver = Receiver::create(&class, &window);
        let cwd = japanese_cwd();

        let child = probe("instance::sim::probe_copydata")
            .env(ENV_CLASS, &class)
            .env(ENV_WINDOW, &window)
            .current_dir(&cwd)
            .spawn()
            .expect("子プロセスを起こせない");

        let received = receiver.pump_until_received(Duration::from_secs(20));
        let out = child.wait_with_output().expect("子プロセスが終わらない");
        let line = probe_line(&out);

        let (dw_data, bytes) =
            received.unwrap_or_else(|| panic!("WM_COPYDATA が届かなかった (送信側の報告: {line})"));

        // 1. 識別値。ここが違うとプラグインは黙って捨てる。
        assert_eq!(dw_data, WMCOPYDATA_SINGLE_INSTANCE_DATA, "dwData");
        // 2. NUL 終端。受信側は `CStr::from_ptr` で読むので、無いと読み越す。
        assert_eq!(bytes.last(), Some(&0u8), "NUL 終端が無い");
        // 3. 途中に NUL が無い (あると argv がそこで切れる)。
        assert_eq!(
            bytes[..bytes.len() - 1].iter().filter(|b| **b == 0).count(),
            0,
            "ペイロードの途中に NUL がある"
        );
        // 4. UTF-8 として復元できる。
        let text =
            std::str::from_utf8(&bytes[..bytes.len() - 1]).expect("ペイロードが UTF-8 で読めない");
        // 5. `{cwd}|{argv}` の形。プラグインは `split('|')` の先頭を cwd と読む。
        let mut parts = text.split('|');
        let got_cwd = parts.next().expect("cwd が無い");
        assert_eq!(
            got_cwd,
            cwd.to_string_lossy(),
            "cwd が壊れた (日本語・空白を含むパス)"
        );
        // 6. argv が 1 つ以上あり、先頭は実行ファイルのパス。
        let argv: Vec<&str> = parts.collect();
        assert!(!argv.is_empty(), "argv が空: {text}");
        assert!(
            argv[0].to_ascii_lowercase().ends_with(".exe"),
            "argv[0] が実行ファイルに見えない: {}",
            argv[0]
        );
        // 7. 送信側も「届いた」と読めていること (戻り値 != 0)。
        assert!(line.contains("notified=true"), "{line}");
        println!("受信: dwData={dw_data} / {} バイト / {text}", bytes.len());

        drop(receiver);
    }

    /// 受信ウィンドウが無ければ、待たずに諦める。
    ///
    /// 待ってしまうと、スタートアップと手動起動が競合した場面で
    /// 「起動したのに何も起きないプロセス」が数秒残る。
    #[test]
    fn missing_window_gives_up_immediately() {
        let started = Instant::now();
        let notified = notify_instance_window(&unique("nowhere-class"), &unique("nowhere-window"));
        let elapsed = started.elapsed();
        assert!(!notified, "存在しないウィンドウへ送れたことになっている");
        assert!(
            elapsed < Duration::from_millis(500),
            "見つからないのに {elapsed:?} 待っている"
        );
    }

    /// 応答しないウィンドウ相手でも、送信側は必ず戻ってくる。
    ///
    /// 実機では「1 個目が転写中で重い」がこれに当たる。ここで固まると
    /// 2 個目が終了できず、結局 2 プロセスが並ぶ。`SendMessageTimeoutW` +
    /// `SMTO_ABORTIFHUNG` が効いていることを、送信側の所要時間で見る。
    ///
    /// 受信側はウィンドウを作ったあと**一切メッセージを回さない**スレッド。
    /// `SMTO_ABORTIFHUNG` が即座に諦めるか、`NOTIFY_TIMEOUT_MS` まで待って
    /// 諦めるかは OS のハング判定次第なので、どちらでも通るよう上限だけを見る。
    #[test]
    fn unresponsive_window_times_out_instead_of_hanging() {
        let class = unique("hung-class");
        let window = unique("hung-window");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let class_for_thread = class.clone();
        let window_for_thread = window.clone();
        let hung = std::thread::spawn(move || {
            let receiver = Receiver::create(&class_for_thread, &window_for_thread);
            ready_tx.send(()).expect("準備完了を伝えられない");
            // ここでメッセージを回さないのが肝。回すと普通に受信してしまう。
            let _ = done_rx.recv_timeout(Duration::from_secs(30));
            drop(receiver);
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("受信ウィンドウが立たない");

        let out = probe("instance::sim::probe_copydata")
            .env(ENV_CLASS, &class)
            .env(ENV_WINDOW, &window)
            .output()
            .expect("子プロセスを起こせない");
        let line = probe_line(&out);
        let elapsed: u64 = line
            .split("elapsed_ms=")
            .nth(1)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("所要時間を読めない: {line}"));

        let _ = done_tx.send(());
        hung.join().expect("受信スレッドが終わらない");

        assert!(line.contains("notified=false"), "{line}");
        assert!(
            elapsed < u64::from(NOTIFY_TIMEOUT_MS) + 2_500,
            "送信側が畳めていない ({elapsed} ms): {line}"
        );
        println!(
            "応答しない相手への送信: {elapsed} ms で戻った (上限 {NOTIFY_TIMEOUT_MS} ms + 猶予)"
        );
    }
}
