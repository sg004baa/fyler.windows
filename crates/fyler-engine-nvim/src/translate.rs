//! エンジン非依存の [`KeyInput`] → nvim keycode表記への変換。
//!
//! nvim keycode表記(`<C-a>`, `<Esc>`, `<lt>` 等)はnvim固有概念なので、
//! この変換は**このモジュールの外に出さない**(絶対ルール2)。

use fyler_core::editor::KeyInput;

/// `nvim_input` に渡すkeycode文字列へ変換する。
///
/// 実装契約:
/// - `Key::Char('a')` → `"a"`、`Key::Char('<')` → `"<lt>"`(エスケープ必須)
/// - 特殊キー: `Enter` → `"<CR>"`、`Esc` → `"<Esc>"`、`Backspace` → `"<BS>"`、
///   `Tab` → `"<Tab>"`、`Delete` → `"<Del>"`、矢印 → `"<Up>"` 等、
///   `Home`/`End`/`PageUp`/`PageDown` → `"<Home>"`/`"<End>"`/`"<PageUp>"`/`"<PageDown>"`、
///   `F(n)` → `"<F{n}>"`
/// - 修飾キー: ctrl → `<C-...>`、alt → `<A-...>`、shift → `<S-...>`(特殊キーのみ。
///   印字可能文字のshiftは `Key::Char` に反映済みの契約なので付けない)
/// - 複数修飾は `<C-A-x>` の順(C, A, S)
pub fn to_nvim_keycodes(input: &KeyInput) -> String {
    todo!("M1: keycode変換(M0スパイクで先行検証してよい)")
}
