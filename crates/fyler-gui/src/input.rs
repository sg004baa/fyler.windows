//! 入力のエンジン転送: eguiイベント → `EditorCommand`。

use eframe::egui;
use fyler_core::editor::{EditorCommand, EditorEngine, Key, KeyInput, Mode, Modifiers};
use fyler_core::pane::PaneId;

use crate::theme;

#[derive(Default)]
pub(crate) struct ImeComposition {
    target: Option<(PaneId, Mode)>,
    preedit: Option<Preedit>,
}

struct Preedit {
    text: String,
    active_range_chars: Option<std::ops::Range<usize>>,
}

impl ImeComposition {
    pub(crate) fn set_target(&mut self, target: Option<(PaneId, Mode)>) -> bool {
        if self.target == target {
            return false;
        }
        let interrupted = self.preedit.is_some();
        self.target = target;
        self.preedit = None;
        interrupted
    }

    fn translate(
        &mut self,
        events: &[egui::Event],
        mode: &Mode,
        interrupted: bool,
    ) -> Vec<EditorCommand> {
        if interrupted {
            return Vec::new();
        }
        let text_mode = ime_enabled(mode);
        let has_text = events
            .iter()
            .any(|event| matches!(event, egui::Event::Text(text) if !text.is_empty()));
        let suppress_composition_keys = self.preedit.is_some()
            || (self.target.is_some()
                && events.iter().any(|event| match event {
                    egui::Event::Ime(egui::ImeEvent::Preedit { text, .. })
                    | egui::Event::Ime(egui::ImeEvent::Commit(text)) => !text.is_empty(),
                    _ => false,
                }));
        let mut commit_echoes: Vec<&str> = if has_text && self.target.is_some() {
            events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut commands = Vec::new();
        for event in events {
            match event {
                egui::Event::Ime(event) => {
                    if self.target.is_none() {
                        continue;
                    }
                    match event {
                        egui::ImeEvent::Preedit {
                            text,
                            active_range_chars,
                        } => {
                            self.preedit = (!text.is_empty()).then(|| Preedit {
                                text: text.clone(),
                                active_range_chars: active_range_chars.clone(),
                            });
                        }
                        egui::ImeEvent::Commit(text) => {
                            self.preedit = None;
                            if !text.is_empty() {
                                commands.push(EditorCommand::Text(text.clone()));
                            }
                        }
                        #[allow(deprecated)]
                        egui::ImeEvent::Disabled => self.preedit = None,
                        #[allow(deprecated)]
                        egui::ImeEvent::Enabled => {}
                    }
                }
                egui::Event::Key {
                    key:
                        egui::Key::Enter
                        | egui::Key::Escape
                        | egui::Key::Tab
                        | egui::Key::Space
                        | egui::Key::ArrowUp
                        | egui::Key::ArrowDown
                        | egui::Key::ArrowLeft
                        | egui::Key::ArrowRight
                        | egui::Key::Home
                        | egui::Key::End
                        | egui::Key::PageUp
                        | egui::Key::PageDown
                        | egui::Key::Backspace
                        | egui::Key::Delete,
                    modifiers,
                    ..
                } if suppress_composition_keys
                    && !modifiers.ctrl
                    && !modifiers.command
                    && !modifiers.alt => {}
                egui::Event::Text(text) => {
                    if let Some(index) = commit_echoes.iter().position(|commit| *commit == text) {
                        commit_echoes.swap_remove(index);
                    } else {
                        translate_event(event, text_mode, has_text, &mut commands);
                    }
                }
                _ => translate_event(event, text_mode, has_text, &mut commands),
            }
        }
        commands
    }

    pub(crate) fn draw(&self, ui: &egui::Ui, caret: egui::Rect, clip: egui::Rect) -> egui::Rect {
        let Some(preedit) = &self.preedit else {
            return caret;
        };
        let normal = egui::TextFormat {
            font_id: egui::TextStyle::Monospace.resolve(ui.style()),
            color: theme::TEXT,
            background: theme::SURFACE,
            underline: egui::Stroke::new(1.0, theme::TEXT),
            ..Default::default()
        };
        let mut job = egui::text::LayoutJob::default();
        let active = preedit.active_range_chars.as_ref().and_then(|range| {
            let len = range.end.checked_sub(range.start)?;
            let mut boundaries = preedit
                .text
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(preedit.text.len()));
            let start = boundaries.nth(range.start)?;
            let end = if len == 0 {
                start
            } else {
                boundaries.nth(len - 1)?
            };
            Some((start, end))
        });
        if let Some((start, end)) = active {
            job.append(&preedit.text[..start], 0.0, normal.clone());
            job.append(
                &preedit.text[start..end],
                0.0,
                egui::TextFormat {
                    background: theme::ACCENT,
                    color: theme::CANVAS,
                    ..normal.clone()
                },
            );
            job.append(&preedit.text[end..], 0.0, normal);
        } else {
            job.append(&preedit.text, 0.0, normal);
        }
        let painter = ui.painter().with_clip_rect(clip);
        let galley = painter.layout_job(job);
        let rect = egui::Rect::from_min_size(caret.min, galley.size());
        painter.rect_filled(rect, 0.0, theme::SURFACE);
        painter.galley(caret.min, galley, theme::TEXT);
        rect
    }
}

pub(crate) fn ime_enabled(mode: &Mode) -> bool {
    matches!(mode, Mode::Insert | Mode::Replace | Mode::Cmdline)
}

/// このフレームの入力イベントをエンジンへ転送する。
///
/// 実装契約(DESIGN.md「EditorEngineトレイト」):
/// - 文字入力はキーボードレイアウト適用済みの `egui::Event::Text` を優先する。
///   `Event::Key` の論理キーだけではShiftと記号の組み合わせを復元できないため、
///   同一フレームのplain printableなKeyは二重送信せず捨てる
/// - Insert / Replace / CmdlineではTextを `EditorCommand::Text`、Normal / Visual等
///   ではTextの各文字を修飾なしの `EditorCommand::Key(Key::Char)` として送る
/// - Textを伴わない `egui::Event::Key` は `fyler_core::editor::KeyInput` に正規化し、
///   Ctrl組み合わせ・特殊キー・印字可能文字のfallbackとして送る
/// - IME未確定文字列はGUIだけに保持し、確定文字列のみ `EditorCommand::Text` にする
/// - `egui::Event::Paste` → `EditorCommand::Paste`
/// - 確認ダイアログ表示中はエンジンへ転送せずダイアログが入力を消費する
/// - 送信は失敗し得る(エンジン停止)。失敗はエラー表示へ回す(panicしない)
pub(crate) fn forward_input(
    ctx: &egui::Context,
    engine: &dyn EditorEngine,
    mode: &Mode,
    ime: &mut ImeComposition,
    interrupted: bool,
) -> anyhow::Result<()> {
    let commands = ctx.input(|input| ime.translate(&input.events, mode, interrupted));
    for command in commands {
        engine.send(command)?;
    }

    Ok(())
}
/// GUI内のキーボード操作向けに、このフレームの入力をNormal mode相当の
/// エンジン非依存キー列へ正規化する。
pub(crate) fn normalized_keys(ctx: &egui::Context) -> Vec<KeyInput> {
    let events = ctx.input(|input| input.events.clone());
    translate_events(&events, &Mode::Normal)
        .into_iter()
        .filter_map(|command| match command {
            EditorCommand::Key(key) => Some(key),
            _ => None,
        })
        .collect()
}

fn translate_events(events: &[egui::Event], mode: &Mode) -> Vec<EditorCommand> {
    let text_mode = ime_enabled(mode);
    let has_text = events.iter().any(|event| match event {
        egui::Event::Text(text) => !text.is_empty(),
        _ => false,
    });
    let mut commands = Vec::new();
    for event in events {
        translate_event(event, text_mode, has_text, &mut commands);
    }
    commands
}

fn translate_event(
    event: &egui::Event,
    text_mode: bool,
    has_text: bool,
    commands: &mut Vec<EditorCommand>,
) {
    match event {
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => {
            let Some(key_input) = key_input(*key, *modifiers) else {
                return;
            };
            let is_plain_char = matches!(key_input.key, Key::Char(_))
                && !key_input.mods.ctrl
                && !key_input.mods.alt;
            if has_text && is_plain_char {
                // eframeは通常の文字キーでKeyとTextを同一フレームに出す。
                // OSのキーボードレイアウトを反映したText側だけを使う。
                return;
            }
            commands.push(EditorCommand::Key(key_input));
        }
        egui::Event::Text(text) if !text.is_empty() => {
            if text_mode {
                commands.push(EditorCommand::Text(text.clone()));
            } else {
                commands.extend(text.chars().map(|character| {
                    EditorCommand::Key(KeyInput {
                        key: Key::Char(character),
                        mods: Modifiers::default(),
                    })
                }));
            }
        }
        egui::Event::Paste(text) => commands.push(EditorCommand::Paste(text.clone())),
        _ => {}
    }
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

    fn key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn normal_mode_prefers_layout_aware_shifted_text() {
        let events = [
            key_event(
                egui::Key::Num6,
                egui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            ),
            egui::Event::Text("^".to_owned()),
        ];

        assert_eq!(
            translate_events(&events, &Mode::Normal),
            [EditorCommand::Key(KeyInput {
                key: Key::Char('^'),
                mods: Modifiers::default(),
            })]
        );
    }

    #[test]
    fn normal_mode_sends_plain_text_once_as_a_key() {
        let events = [
            key_event(egui::Key::A, egui::Modifiers::default()),
            egui::Event::Text("a".to_owned()),
        ];

        assert_eq!(
            translate_events(&events, &Mode::Normal),
            [EditorCommand::Key(KeyInput {
                key: Key::Char('a'),
                mods: Modifiers::default(),
            })]
        );
    }

    #[test]
    fn normal_mode_sends_escape_key_without_text() {
        let events = [key_event(egui::Key::Escape, egui::Modifiers::default())];

        assert_eq!(
            translate_events(&events, &Mode::Normal),
            [EditorCommand::Key(KeyInput {
                key: Key::Esc,
                mods: Modifiers::default(),
            })]
        );
    }

    #[test]
    fn normal_mode_keeps_control_modifier_without_text() {
        let events = [key_event(
            egui::Key::A,
            egui::Modifiers {
                ctrl: true,
                ..Default::default()
            },
        )];

        assert_eq!(
            translate_events(&events, &Mode::Normal),
            [EditorCommand::Key(KeyInput {
                key: Key::Char('a'),
                mods: Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            })]
        );
    }

    #[test]
    fn insert_mode_keeps_text_input_path() {
        let events = [
            key_event(egui::Key::A, egui::Modifiers::default()),
            egui::Event::Text("a".to_owned()),
        ];

        assert_eq!(
            translate_events(&events, &Mode::Insert),
            [EditorCommand::Text("a".to_owned())]
        );
    }

    #[test]
    fn normal_mode_ignores_ime_commit() {
        let events = [egui::Event::Ime(egui::ImeEvent::Commit("あ".to_owned()))];

        assert!(translate_events(&events, &Mode::Normal).is_empty());
    }

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
