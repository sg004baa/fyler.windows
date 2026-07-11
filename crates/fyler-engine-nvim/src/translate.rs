//! エンジン非依存の [`KeyInput`] → nvim keycode表記への変換。
//!
//! nvim keycode表記(`<C-a>`, `<Esc>`, `<lt>` 等)はnvim固有概念なので、
//! この変換は**このモジュールの外に出さない**(絶対ルール2)。

use fyler_core::editor::{Key, KeyInput};
use fyler_core::keymap::KeySequence;

/// キーシーケンスを`vim.keymap.set`のlhs文字列へ変換する。
pub(crate) fn sequence_to_lhs(sequence: &KeySequence) -> String {
    sequence.0.iter().map(to_nvim_keycodes).collect()
}

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
    let (token, is_char) = key_token(input.key);
    let mods = &input.mods;
    // shiftは特殊キーのみ。印字可能文字のshiftは Key::Char に反映済み(契約L16-17)。
    let use_shift = mods.shift && !is_char;
    let has_modifier = mods.ctrl || mods.alt || use_shift;

    if !has_modifier {
        // 修飾なし: 通常の印字可能文字は素のまま、エスケープ(`<`)・特殊キーは括弧付き。
        if is_char && token != "lt" {
            return token;
        }
        return format!("<{token}>");
    }

    // 複数修飾は C, A, S の順(契約L18)。
    let mut prefix = String::new();
    if mods.ctrl {
        prefix.push_str("C-");
    }
    if mods.alt {
        prefix.push_str("A-");
    }
    if use_shift {
        prefix.push_str("S-");
    }
    format!("<{prefix}{token}>")
}

/// キーを keycode 表記のトークンへ。戻り値の bool は「印字可能文字か」
/// (shift適用可否・素出し可否の判定に使う)。`<` は `lt` にエスケープする。
fn key_token(key: Key) -> (String, bool) {
    match key {
        Key::Char('<') => ("lt".to_string(), true),
        Key::Char(c) => (c.to_string(), true),
        Key::Enter => ("CR".to_string(), false),
        Key::Esc => ("Esc".to_string(), false),
        Key::Backspace => ("BS".to_string(), false),
        Key::Tab => ("Tab".to_string(), false),
        Key::Delete => ("Del".to_string(), false),
        Key::Up => ("Up".to_string(), false),
        Key::Down => ("Down".to_string(), false),
        Key::Left => ("Left".to_string(), false),
        Key::Right => ("Right".to_string(), false),
        Key::Home => ("Home".to_string(), false),
        Key::End => ("End".to_string(), false),
        Key::PageUp => ("PageUp".to_string(), false),
        Key::PageDown => ("PageDown".to_string(), false),
        Key::F(n) => (format!("F{n}"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fyler_core::editor::{Key, KeyInput, Modifiers};

    /// 修飾なしの `KeyInput` を組む。
    fn plain(key: Key) -> KeyInput {
        KeyInput {
            key,
            mods: Modifiers::default(),
        }
    }

    fn sequence(input: &str) -> KeySequence {
        fyler_core::keymap::parse_key_sequence(input, None).unwrap()
    }

    #[test]
    fn sequence_lhs_concatenates_nvim_keycodes() {
        assert_eq!(sequence_to_lhs(&sequence("g d")), "gd");
        assert_eq!(sequence_to_lhs(&sequence("Ctrl+W Ctrl+W")), "<C-w><C-w>");
        assert_eq!(sequence_to_lhs(&sequence("Space f")), " f");
        assert_eq!(sequence_to_lhs(&sequence("?")), "?");
        assert_eq!(sequence_to_lhs(&sequence("<")), "<lt>");
    }

    /// 明示的な修飾つきの `KeyInput` を組む。
    fn with_mods(key: Key, ctrl: bool, alt: bool, shift: bool) -> KeyInput {
        KeyInput {
            key,
            mods: Modifiers { ctrl, alt, shift },
        }
    }

    #[test]
    fn plain_printable_char_passes_through_verbatim() {
        assert_eq!(to_nvim_keycodes(&plain(Key::Char('a'))), "a");
        assert_eq!(to_nvim_keycodes(&plain(Key::Char('z'))), "z");
        assert_eq!(to_nvim_keycodes(&plain(Key::Char('1'))), "1");
    }

    #[test]
    fn uppercase_char_carries_no_shift() {
        // 契約: `A` は `Char('A')` として来るので shift を足して <S-a> にしない。
        assert_eq!(to_nvim_keycodes(&plain(Key::Char('A'))), "A");
    }

    #[test]
    fn lt_is_escaped_even_without_modifiers() {
        // `<` は素の文字と違い、修飾なしでも括弧つきの <lt> になる。
        assert_eq!(to_nvim_keycodes(&plain(Key::Char('<'))), "<lt>");
    }

    #[test]
    fn special_keys_unmodified_use_bracketed_tokens() {
        let cases = [
            (Key::Enter, "<CR>"),
            (Key::Esc, "<Esc>"),
            (Key::Backspace, "<BS>"),
            (Key::Tab, "<Tab>"),
            (Key::Delete, "<Del>"),
            (Key::Up, "<Up>"),
            (Key::Down, "<Down>"),
            (Key::Left, "<Left>"),
            (Key::Right, "<Right>"),
            (Key::Home, "<Home>"),
            (Key::End, "<End>"),
            (Key::PageUp, "<PageUp>"),
            (Key::PageDown, "<PageDown>"),
        ];
        for (key, expected) in cases {
            assert_eq!(to_nvim_keycodes(&plain(key)), expected, "key = {key:?}");
        }
    }

    #[test]
    fn function_keys_span_f1_through_f12() {
        // 1桁/2桁の境界を突く(F1 と F12、繰り上がりの F9/F10)。
        assert_eq!(to_nvim_keycodes(&plain(Key::F(1))), "<F1>");
        assert_eq!(to_nvim_keycodes(&plain(Key::F(9))), "<F9>");
        assert_eq!(to_nvim_keycodes(&plain(Key::F(10))), "<F10>");
        assert_eq!(to_nvim_keycodes(&plain(Key::F(12))), "<F12>");
    }

    #[test]
    fn ctrl_wraps_printable_char() {
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Char('a'), true, false, false)),
            "<C-a>"
        );
    }

    #[test]
    fn alt_wraps_printable_char() {
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Char('a'), false, true, false)),
            "<A-a>"
        );
    }

    #[test]
    fn ctrl_wraps_function_key() {
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::F(5), true, false, false)),
            "<C-F5>"
        );
    }

    #[test]
    fn shift_applies_to_special_key() {
        // shift は特殊キーでのみ有効。
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Enter, false, false, true)),
            "<S-CR>"
        );
    }

    #[test]
    fn modifier_prefix_order_is_ctrl_alt_shift() {
        // 複数修飾は C, A, S の順で並ぶ。
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Char('x'), true, true, false)),
            "<C-A-x>"
        );
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Enter, true, false, true)),
            "<C-S-CR>"
        );
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Enter, true, true, true)),
            "<C-A-S-CR>"
        );
    }

    #[test]
    fn shift_alone_on_printable_char_is_ignored() {
        // 印字可能文字では shift フラグが立っていても素の文字/lt のまま(括弧も付かない)。
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Char('a'), false, false, true)),
            "a"
        );
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Char('<'), false, false, true)),
            "<lt>"
        );
    }

    #[test]
    fn shift_never_prefixes_printable_char_even_alongside_ctrl() {
        // 不変条件: 印字可能文字の shift は他の修飾があっても無視される(<C-S-a> にならない)。
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Char('a'), true, false, true)),
            "<C-a>"
        );
    }

    #[test]
    fn modified_lt_keeps_lt_token_inside_brackets() {
        assert_eq!(
            to_nvim_keycodes(&with_mods(Key::Char('<'), true, false, false)),
            "<C-lt>"
        );
    }
}
