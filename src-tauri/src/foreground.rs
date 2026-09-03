//! 前景ウィンドウ情報の取得。
//!
//! 録音開始の瞬間に一度だけ呼ばれ、結果は [`crate::session::RecordingSession`] に固定される。
//! 失敗しても録音は続行し、[`TargetWindow::unknown`] を返す。

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, MAX_PATH};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
};

use crate::session::TargetWindow;

/// 現在の前景ウィンドウを採取する。取得できない項目は既定値で埋める。
pub fn capture_foreground() -> TargetWindow {
    // SAFETY: 引数なし。戻り値が NULL でありうるので下でチェックする。
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        log::warn!("GetForegroundWindow が NULL を返した (ロック画面/UAC 中など)");
        return TargetWindow::unknown();
    }

    let mut process_id: u32 = 0;
    // SAFETY: hwnd は非 NULL、出力ポインタはスタック上の有効な u32。
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };

    // フルパスとベース名は**同じ 1 回の問い合わせ**から取る。ベース名だけを
    // 取り直すと `OpenProcess` が 2 回走り、録音開始の経路がその分遅れる。
    let process_path = (process_id != 0)
        .then(|| process_image_path(process_id))
        .flatten();
    let process_name = process_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string());

    TargetWindow {
        hwnd: hwnd.0 as isize,
        process_id,
        process_name,
        window_title: window_title(hwnd),
        process_path,
    }
}

/// ウィンドウタイトル。取得できなければ空文字。
///
/// [`crate::screen`] のウィンドウ列挙からも使う (同じ取り方を 2 度書かない)。
pub(crate) fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: buf は有効なスライス。GetWindowTextW はスライス長を上限に書き込む。
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    let len = (len as usize).min(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// PID から実行ファイル名 (ベース名) を得る。
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` を使うので、昇格プロセスに対しても
/// 名前の取得だけは通ることが多い (完全な情報は取れない)。
///
/// [`crate::screen`] のウィンドウ列挙からも使う。
pub(crate) fn process_image_name(process_id: u32) -> Option<String> {
    process_image_path(process_id).map(|path| base_name(&path.to_string_lossy()))
}

/// PID から実行ファイルの**フルパス**を得る。
///
/// ベース名しか要らない呼び出し元 ([`process_image_name`]) を壊さないよう、
/// 別の関数として足してある。フルパスが要るのは小窓のアプリ表示名と
/// アイコン ([`crate::app_icon`]) — 版情報もアイコンも実体のパスが要る。
pub(crate) fn process_image_path(process_id: u32) -> Option<std::path::PathBuf> {
    // SAFETY: PID は数値、継承なし。失敗時は Err が返る。
    let handle: HANDLE =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
            .map_err(|e| {
                log::debug!("OpenProcess(pid={process_id}) 失敗: {e}");
            })
            .ok()?;

    let mut buf = [0u16; MAX_PATH as usize];
    let mut size = buf.len() as u32;
    // SAFETY: handle は OpenProcess 由来の有効ハンドル。buf/size は対で有効。
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
    };

    // SAFETY: handle は上で得た有効ハンドル。以降は使わない。
    let _ = unsafe { CloseHandle(handle) };

    if let Err(e) = result {
        log::debug!("QueryFullProcessImageNameW(pid={process_id}) 失敗: {e}");
        return None;
    }

    let size = (size as usize).min(buf.len());
    let full = String::from_utf16_lossy(&buf[..size]);
    (!full.is_empty()).then(|| std::path::PathBuf::from(full))
}

/// フルパスから実行ファイル名だけを取り出す。
fn base_name(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::base_name;

    #[test]
    fn base_name_extracts_exe() {
        assert_eq!(base_name(r"C:\Windows\System32\notepad.exe"), "notepad.exe");
        assert_eq!(base_name("notepad.exe"), "notepad.exe");
        assert_eq!(base_name(r"C:/foo/bar/baz.exe"), "baz.exe");
        // 末尾がセパレータの異常系でも panic しない。
        assert_eq!(base_name(r"C:\foo\"), r"C:\foo\");
    }
}
