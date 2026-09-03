//! 相手アプリの「表示名」と「アイコン」。小窓 (overlay) の表示専用。
//!
//! 録音中の小窓に貼付先を出す ([`crate::session::TargetView`]) ための材料を
//! 集める。ここで扱うのは**実行ファイルの属性だけ**で、ウィンドウの中身にも
//! タイトルにも触れない。
//!
//! # 録音開始の経路では絶対に呼ばない
//!
//! 版情報の読み出しは exe を数百 KB 読むことがあり、`SHGetFileInfoW` は
//! シェルの拡張 (アイコンオーバーレイのハンドラ等) を巻き込む。どちらも
//! 相手のインストール状態次第で数百 ms かかりうる。**録音開始が遅れると
//! 最初の一言が消える** (design.md「押してから話すまで待たせない」) ので、
//! 呼び出し側 (`lib.rs`) は録音を始めてから短命スレッドでここへ入り、
//! 結果は `nox://target-icon` イベントで後追いする。
//!
//! 即座に何か出す必要がある側 (StatusPayload) には [`cached_name`] と
//! [`fallback_name`] を用意してある — キャッシュが当たればそれ、外れたら
//! exe のベース名。**どちらもファイルを読まない**。
//!
//! # キャッシュ
//!
//! 同じアプリへ何度も喋るのが普通の使い方なので、プロセスパス単位で
//! 覚えておく。常駐して 1 日使っても数十件にしかならない。

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// 表示名のキャッシュ (プロセスパス → 版情報の `FileDescription`)。
static NAMES: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// アイコンのキャッシュ。**取れなかったことも覚える** (`None`) —
/// 覚えないと、アイコンを持たない exe に喋るたびにシェルを叩き直す。
static ICONS: LazyLock<Mutex<HashMap<PathBuf, Option<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// exe のベース名から作る当座の表示名。**ファイルを読まない**。
///
/// 版情報が取れるまでの間、小窓に出しておく値。
pub fn fallback_name(process_name: &str) -> String {
    let name = process_name.trim();
    if name.is_empty() {
        return "<unknown>".to_string();
    }
    // 拡張子だけを落とす。`notepad.exe` → `notepad`。
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case("exe") && !stem.is_empty() => {
            stem.to_string()
        }
        _ => name.to_string(),
    }
}

/// キャッシュ済みの表示名。**ファイルには触らない** (録音開始の経路から
/// 呼べるのはここまで)。
pub fn cached_name(path: &Path) -> Option<String> {
    NAMES.lock().ok()?.get(path).cloned()
}

/// キャッシュ済みのアイコン。**ファイルにもシェルにも触らない**。
///
/// 外側の `None` は「まだ引いていない」、`Some(None)` は「引いたが無かった」。
/// 録音開始の経路はこれで**当たった分だけ同期に載せる** — 後追いイベントに
/// 頼ると、状態イベントとの到着順の競合で当たったアイコンを捨てることがある
/// (`lib.rs::resolve_target_icon` の注記)。
pub fn cached_icon(path: &Path) -> Option<Option<String>> {
    ICONS.lock().ok()?.get(path).cloned()
}

/// 版情報の `FileDescription`。取れなければ exe のベース名。
///
/// **短命スレッドから呼ぶこと** (モジュール冒頭の注記)。
pub fn display_name(path: &Path) -> String {
    if let Some(hit) = cached_name(path) {
        return hit;
    }
    let name = file_description(path).unwrap_or_else(|| {
        fallback_name(&path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
    });
    if let Ok(mut cache) = NAMES.lock() {
        cache.insert(path.to_path_buf(), name.clone());
    }
    name
}

/// 16px のアプリアイコンを `data:image/png;base64,...` として返す。
///
/// **短命スレッドから呼ぶこと** (モジュール冒頭の注記)。
pub fn icon_data_uri(path: &Path) -> Option<String> {
    if let Ok(cache) = ICONS.lock() {
        if let Some(hit) = cache.get(path) {
            return hit.clone();
        }
    }
    let uri = load_icon_data_uri(path);
    if let Ok(mut cache) = ICONS.lock() {
        cache.insert(path.to_path_buf(), uri.clone());
    }
    uri
}

// ---------------------------------------------------------------------------
// 版情報 (`FileDescription`)
// ---------------------------------------------------------------------------

/// 版情報から `FileDescription` を読む。
///
/// 言語は決め打ちしない。`\VarFileInfo\Translation` にある最初の
/// (言語, コードページ) の組で `\StringFileInfo\{lang}{cp}\FileDescription`
/// を引く。決め打ち (`040904B0` 等) にすると、日本語版のアプリで
/// 何も取れない、という取りこぼし方をする。
fn file_description(path: &Path) -> Option<String> {
    let wide = to_wide(path.as_os_str().to_str()?);
    let file = PCWSTR(wide.as_ptr());

    // SAFETY: file は NUL 終端の UTF-16。ハンドル出力は使わないので None。
    let size = unsafe { GetFileVersionInfoSizeW(file, None) };
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: block は size バイト確保済み。dwhandle は無視される引数。
    unsafe { GetFileVersionInfoW(file, None, size, block.as_mut_ptr() as *mut c_void) }.ok()?;

    let (lang, code_page) = translation(&block)?;
    let key = to_wide(&format!(
        "\\StringFileInfo\\{lang:04x}{code_page:04x}\\FileDescription"
    ));
    let mut value: *mut c_void = std::ptr::null_mut();
    let mut len: u32 = 0;
    // SAFETY: block は上で読んだ版情報。key は NUL 終端の UTF-16。
    // 戻り値が真のとき value/len は block 内部を指す (解放してはならない)。
    let ok = unsafe {
        VerQueryValueW(
            block.as_ptr() as *const c_void,
            PCWSTR(key.as_ptr()),
            &mut value,
            &mut len,
        )
    };
    if !ok.as_bool() || value.is_null() || len == 0 {
        return None;
    }
    // SAFETY: value は block 内部の UTF-16 文字列で、長さは len 文字。
    let text = unsafe { std::slice::from_raw_parts(value as *const u16, len as usize) };
    let text: String = String::from_utf16_lossy(text)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    (!text.is_empty()).then_some(text)
}

/// `\VarFileInfo\Translation` の先頭の (言語 ID, コードページ)。
fn translation(block: &[u8]) -> Option<(u16, u16)> {
    let key = to_wide("\\VarFileInfo\\Translation");
    let mut value: *mut c_void = std::ptr::null_mut();
    let mut len: u32 = 0;
    // SAFETY: block は版情報。key は NUL 終端の UTF-16。
    let ok = unsafe {
        VerQueryValueW(
            block.as_ptr() as *const c_void,
            PCWSTR(key.as_ptr()),
            &mut value,
            &mut len,
        )
    };
    // 1 組は 4 バイト (u16 が 2 つ)。それ未満なら読まない。
    if !ok.as_bool() || value.is_null() || len < 4 {
        return None;
    }
    // SAFETY: value は block 内部の u16 配列を指し、len バイト以上ある。
    let pair = unsafe { std::slice::from_raw_parts(value as *const u16, 2) };
    Some((pair[0], pair[1]))
}

// ---------------------------------------------------------------------------
// アイコン
// ---------------------------------------------------------------------------

fn load_icon_data_uri(path: &Path) -> Option<String> {
    let (rgba, width, height) = icon_rgba(path)?;
    let png = crate::screen::encode_png_rgba(&rgba, width, height)
        .map_err(|e| log::debug!("アプリアイコンを PNG にできません: {e}"))
        .ok()?;
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    Some(format!("data:image/png;base64,{encoded}"))
}

/// アプリアイコンを RGBA (トップダウン) で取り出す。
///
/// `SHGetFileInfoW` は COM の初期化を要求しない (シェルが内部で面倒を見る)
/// ので、呼び出しスレッドを選ばない。ただし遅いことはあるので短命スレッド
/// 専用 (モジュール冒頭)。
fn icon_rgba(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let wide = to_wide(path.as_os_str().to_str()?);
    let mut info = SHFILEINFOW::default();
    // SAFETY: wide は NUL 終端の UTF-16。info は size と対で渡している。
    let result = unsafe {
        SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            Default::default(),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON,
        )
    };
    if result == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let icon = info.hIcon;
    let pixels = icon_bits(icon);
    // **必ず捨てる。** 取ったアイコンは呼び出し側の持ち物で、放置すると
    // 録音のたびに GDI ハンドルが増える。
    // SAFETY: icon は SHGetFileInfoW が返した有効なハンドル。以降使わない。
    unsafe {
        let _ = DestroyIcon(icon);
    }
    pixels
}

/// HICON の色ビットマップを RGBA (トップダウン) として読む。
fn icon_bits(icon: HICON) -> Option<(Vec<u8>, u32, u32)> {
    let mut icon_info = ICONINFO::default();
    // SAFETY: icon は有効。出力先はスタック上の ICONINFO。
    // 成功すると hbmColor / hbmMask の所有権がこちらに移る。
    unsafe { GetIconInfo(icon, &mut icon_info) }.ok()?;

    let color = icon_info.hbmColor;
    let mask = icon_info.hbmMask;
    let result = (!color.is_invalid())
        .then(|| bitmap_rgba(color))
        .flatten();

    // SAFETY: どちらも GetIconInfo が作ったビットマップ。DC には
    // 選択していないので、ここで削除してよい。
    unsafe {
        if !color.is_invalid() {
            let _ = DeleteObject(color.into());
        }
        if !mask.is_invalid() {
            let _ = DeleteObject(mask.into());
        }
    }
    result
}

fn bitmap_rgba(
    bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Option<(Vec<u8>, u32, u32)> {
    let mut header = BITMAP::default();
    // SAFETY: bitmap は有効。出力先は BITMAP で、サイズも対で渡している。
    let read = unsafe {
        GetObjectW(
            bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut header as *mut BITMAP as *mut c_void),
        )
    };
    if read == 0 || header.bmWidth <= 0 || header.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (header.bmWidth, header.bmHeight);

    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // **負の高さ = トップダウン。** 画面のキャプチャ (screen.rs) は
            // ドライバ差を避けてボトムアップで取って自前で反転しているが、
            // ここはメモリ上のアイコンなので素直に上から取れる。
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    // SAFETY: None は画面全体の DC。対で ReleaseDC する。
    let screen = unsafe { GetDC(None) };
    if screen.is_invalid() {
        return None;
    }
    let mut bgra = vec![0u8; (width as usize) * (height as usize) * 4];
    // SAFETY: screen / bitmap は有効。bgra は info の記述と同じ大きさ。
    let copied = unsafe {
        GetDIBits(
            screen,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr() as *mut c_void),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: 上の GetDC と対。
    unsafe { ReleaseDC(None, screen) };
    if copied == 0 {
        return None;
    }

    Some((bgra_to_rgba(bgra), width as u32, height as u32))
}

/// BGRA → RGBA。**アルファが全部 0 のときは不透明として扱う。**
///
/// 32bpp のアイコンは普通アルファを持つが、古い 24bpp のアイコンを
/// 32bpp で読むとアルファ面が 0 で埋まる。そのまま出すと**完全に透明な
/// 画像**になり、小窓では「アイコンが出ない」ようにしか見えない
/// (取得失敗と区別が付かない)。透明度を捨てて出すほうがまだ役に立つ。
fn bgra_to_rgba(mut pixels: Vec<u8>) -> Vec<u8> {
    let alpha_missing = pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
        if alpha_missing {
            pixel[3] = 255;
        }
    }
    pixels
}

/// NUL 終端の UTF-16 にする (Win32 の W 系 API 用)。
fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_name_drops_the_extension() {
        assert_eq!(fallback_name("notepad.exe"), "notepad");
        assert_eq!(fallback_name("Code.EXE"), "Code");
        // 拡張子が無いものはそのまま。
        assert_eq!(fallback_name("notepad"), "notepad");
        // 取得失敗のプレースホルダは**そのまま残す** — ここで「不明」等に
        // 化けさせると、呼び出し側が失敗を判別できなくなる。
        assert_eq!(fallback_name("<unknown>"), "<unknown>");
        assert_eq!(fallback_name("  "), "<unknown>");
        // `.exe` しか無い異常系でも空文字を返さない。
        assert_eq!(fallback_name(".exe"), ".exe");
    }

    #[test]
    fn bgra_becomes_rgba() {
        // B, G, R, A の並びが R, G, B, A になる。
        let out = bgra_to_rgba(vec![10, 20, 30, 200]);
        assert_eq!(out, vec![30, 20, 10, 200]);
    }

    #[test]
    fn a_fully_transparent_icon_is_treated_as_opaque() {
        // アルファが全部 0 = 24bpp のアイコンを 32bpp で読んだ場合。
        // そのまま出すと「見えないアイコン」になる。
        let out = bgra_to_rgba(vec![10, 20, 30, 0, 40, 50, 60, 0]);
        assert_eq!(out, vec![30, 20, 10, 255, 60, 50, 40, 255]);
    }

    /// **実機のアイコンと版情報を確かめる診断。**
    ///
    /// 出すのは長さと表示名だけ。実行: `cargo test --lib -- --ignored
    /// --nocapture live_explorer_icon`
    #[test]
    #[ignore = "実機のシェルとファイルを叩く"]
    fn live_explorer_icon() {
        let path = Path::new(r"C:\Windows\explorer.exe");
        println!("表示名   : {}", display_name(path));
        match icon_data_uri(path) {
            Some(uri) => println!("アイコン : {} 文字の data URI", uri.len()),
            None => println!("アイコン : 取得できません"),
        }
    }
}
