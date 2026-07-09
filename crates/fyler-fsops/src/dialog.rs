//! 起動時の致命的エラーをユーザーへ見せるためのネイティブダイアログ。
//!
//! `windows_subsystem = "windows"` でコンソールを持たないGUIバイナリでは、
//! `main` から早期に抜けるエラー(engine spawn失敗・scan失敗・nvim未検出等)が
//! 画面に何も出ないまま終了してしまう。この関数でネイティブのメッセージボックスを
//! 出し、原因をユーザーへ伝える。`windows` クレートを使うためこのクレートに置く。

/// タイトルと本文でネイティブのエラーダイアログを表示する。
///
/// Windowsでは `MessageBoxW`(OKのみ・エラーアイコン)。非Windowsでは
/// 標準エラー出力へフォールバックする(開発時の検証用)。呼び出しは同期で、
/// ユーザーがダイアログを閉じるまで戻らない。
#[cfg(windows)]
pub fn show_error_dialog(title: &str, message: &str) {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::PCWSTR;

    fn to_wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let title_wide = to_wide(title);
    let message_wide = to_wide(message);
    // SAFETY: 両引数は呼び出し中有効なNUL終端UTF-16文字列であり、
    // ownerウィンドウは渡さない(None)。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message_wide.as_ptr()),
            PCWSTR(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// タイトルと本文でネイティブのエラーダイアログを表示する。
///
/// 非Windowsでは開発時の検証用に標準エラー出力へ書き出す。
#[cfg(not(windows))]
pub fn show_error_dialog(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}
