//! 入力のエンジン転送: eguiイベント → `EditorCommand`。

use eframe::egui;
use fyler_core::editor::{EditorCommand, EditorEngine, Key, KeyInput, Mode, Modifiers};

/// このフレームの入力イベントをエンジンへ転送する。
///
/// 実装契約(DESIGN.md「EditorEngineトレイト」):
/// - `egui::Event::Key` → `fyler_core::editor::KeyInput` に正規化して
///   `EditorCommand::Key`。印字可能文字はShift適用済みの `Key::Char` にする
/// - `egui::Event::Text`(**IME確定文字列・日本語入力を含む**)→
///   `EditorCommand::Text`。Keyと二重送信しないこと(egui側でKeyとTextの両方が
///   来る文字の扱いをM0スパイクで確定する)
/// - `egui::Event::Paste` → `EditorCommand::Paste`
/// - 確認ダイアログ表示中はエンジンへ転送せずダイアログが入力を消費する
/// - 送信は失敗し得る(エンジン停止)。失敗はエラー表示へ回す(panicしない)
pub fn forward_input(
    ctx: &egui::Context,
    engine: &dyn EditorEngine,
    mode: &Mode,
) -> anyhow::Result<()> {
    let events = ctx.input(|input| input.events.clone());
    let text_mode = matches!(mode, Mode::Insert | Mode::Replace | Mode::Cmdline);
    let has_text = events.iter().any(|event| match event {
        egui::Event::Text(text) => !text.is_empty(),
        egui::Event::Ime(egui::ImeEvent::Commit(text)) => !text.is_empty(),
        _ => false,
    });
    let has_plain_printable_key = events.iter().any(|event| {
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            return false;
        };
        !modifiers.ctrl
            && !modifiers.command
            && !modifiers.alt
            && printable_char(*key, modifiers.shift).is_some()
    });

    for event in events {
        let command = match event {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                let Some(key_input) = key_input(key, modifiers) else {
                    continue;
                };
                let is_plain_char = matches!(key_input.key, Key::Char(_))
                    && !key_input.mods.ctrl
                    && !key_input.mods.alt;
                if text_mode && has_text && is_plain_char {
                    // eframeは通常の文字キーでKeyとTextを同一フレームに出す。
                    // 挿入系モードではText側だけを使い、IMEと同じliteral経路へ流す。
                    continue;
                }
                EditorCommand::Key(key_input)
            }
            egui::Event::Text(text) if !text.is_empty() => {
                if !text_mode && has_plain_printable_key {
                    // Normal/Visual系ではKey側がVimコマンド。Textは重複なので捨てる。
                    continue;
                }
                EditorCommand::Text(text)
            }
            egui::Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                EditorCommand::Text(text)
            }
            egui::Event::Paste(text) => EditorCommand::Paste(text),
            _ => continue,
        };

        engine.send(command)?;
    }

    Ok(())
}

fn key_input(key: egui::Key, modifiers: egui::Modifiers) -> Option<KeyInput> {
    let core_key = if let Some(character) = printable_char(key, modifiers.shift) {
        Key::Char(character)
    } else {
        match key {
            egui::Key::Enter => Key::Enter,
            egui::Key::Escape => Key::Esc,
            egui::Key::Backspace => Key::Backspace,
            egui::Key::Tab => Key::Tab,
            egui::Key::Delete => Key::Delete,
            egui::Key::ArrowUp => Key::Up,
            egui::Key::ArrowDown => Key::Down,
            egui::Key::ArrowLeft => Key::Left,
            egui::Key::ArrowRight => Key::Right,
            egui::Key::Home => Key::Home,
            egui::Key::End => Key::End,
            egui::Key::PageUp => Key::PageUp,
            egui::Key::PageDown => Key::PageDown,
            egui::Key::F1 => Key::F(1),
            egui::Key::F2 => Key::F(2),
            egui::Key::F3 => Key::F(3),
            egui::Key::F4 => Key::F(4),
            egui::Key::F5 => Key::F(5),
            egui::Key::F6 => Key::F(6),
            egui::Key::F7 => Key::F(7),
            egui::Key::F8 => Key::F(8),
            egui::Key::F9 => Key::F(9),
            egui::Key::F10 => Key::F(10),
            egui::Key::F11 => Key::F(11),
            egui::Key::F12 => Key::F(12),
            _ => return None,
        }
    };

    let is_char = matches!(core_key, Key::Char(_));
    Some(KeyInput {
        key: core_key,
        mods: Modifiers {
            ctrl: modifiers.ctrl || modifiers.command,
            alt: modifiers.alt,
            // Charは論理キーへ反映済み。特殊キーだけShiftを修飾として残す。
            shift: modifiers.shift && !is_char,
        },
    })
}

fn printable_char(key: egui::Key, shift: bool) -> Option<char> {
    Some(match key {
        egui::Key::Space => ' ',
        egui::Key::Colon => ':',
        egui::Key::Comma => ',',
        egui::Key::Backslash => '\\',
        egui::Key::Slash => '/',
        egui::Key::Pipe => '|',
        egui::Key::Questionmark => '?',
        egui::Key::Exclamationmark => '!',
        egui::Key::OpenBracket => '[',
        egui::Key::CloseBracket => ']',
        egui::Key::OpenCurlyBracket => '{',
        egui::Key::CloseCurlyBracket => '}',
        egui::Key::Backtick => '`',
        egui::Key::Minus => '-',
        egui::Key::Period => '.',
        egui::Key::Plus => '+',
        egui::Key::Equals => '=',
        egui::Key::Semicolon => ';',
        egui::Key::Quote => '\'',
        egui::Key::Num0 => '0',
        egui::Key::Num1 => '1',
        egui::Key::Num2 => '2',
        egui::Key::Num3 => '3',
        egui::Key::Num4 => '4',
        egui::Key::Num5 => '5',
        egui::Key::Num6 => '6',
        egui::Key::Num7 => '7',
        egui::Key::Num8 => '8',
        egui::Key::Num9 => '9',
        egui::Key::A => letter('a', shift),
        egui::Key::B => letter('b', shift),
        egui::Key::C => letter('c', shift),
        egui::Key::D => letter('d', shift),
        egui::Key::E => letter('e', shift),
        egui::Key::F => letter('f', shift),
        egui::Key::G => letter('g', shift),
        egui::Key::H => letter('h', shift),
        egui::Key::I => letter('i', shift),
        egui::Key::J => letter('j', shift),
        egui::Key::K => letter('k', shift),
        egui::Key::L => letter('l', shift),
        egui::Key::M => letter('m', shift),
        egui::Key::N => letter('n', shift),
        egui::Key::O => letter('o', shift),
        egui::Key::P => letter('p', shift),
        egui::Key::Q => letter('q', shift),
        egui::Key::R => letter('r', shift),
        egui::Key::S => letter('s', shift),
        egui::Key::T => letter('t', shift),
        egui::Key::U => letter('u', shift),
        egui::Key::V => letter('v', shift),
        egui::Key::W => letter('w', shift),
        egui::Key::X => letter('x', shift),
        egui::Key::Y => letter('y', shift),
        egui::Key::Z => letter('z', shift),
        _ => return None,
    })
}

fn letter(lower: char, shift: bool) -> char {
    if shift {
        lower.to_ascii_uppercase()
    } else {
        lower
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_letters_apply_shift_before_crossing_the_engine_boundary() {
        assert_eq!(printable_char(egui::Key::A, false), Some('a'));
        assert_eq!(printable_char(egui::Key::A, true), Some('A'));
    }

    #[test]
    fn special_key_keeps_shift_modifier() {
        let input = key_input(
            egui::Key::Tab,
            egui::Modifiers {
                shift: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(input.key, Key::Tab);
        assert!(input.mods.shift);
    }
}
