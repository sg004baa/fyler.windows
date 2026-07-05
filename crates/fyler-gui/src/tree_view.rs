//! ツリー本体の描画。

use std::collections::HashMap;

use eframe::egui;
use fyler_core::editor::{Cursor, EditorSnapshot, Mode};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::EntryId;

use crate::confirm::IconStyle;
use crate::{conceal, icon};

/// 前フレームのツリー可視範囲。
#[derive(Debug, Clone, Copy)]
pub struct TreeViewport {
    /// 可視範囲上端のスクロールオフセット。
    pub scroll_offset: f32,
    /// スクロール領域の表示高。
    pub height: f32,
}

/// ツリー描画後にapp層へ返す表示情報。
#[derive(Debug, Clone, Copy)]
pub struct TreeViewOutput {
    /// 可視範囲内にある描画済みカーソルの矩形。
    pub cursor_rect: Option<egui::Rect>,
    /// スクロールバーを除くツリー描画領域全体の矩形。
    pub tree_rect: egui::Rect,
    /// 次フレームのカーソル追従判定に使う現在の可視範囲。
    pub viewport: TreeViewport,
}

/// snapshotのバッファ行をツリーとして描画する。
///
/// 実装契約:
/// - 各行は [`crate::conceal`] を通してから描く(生テキストを直接描かない)
/// - カーソルは [`crate::conceal::display_cursor`] の補正後座標に描く。
///   モードによって形を変える(Normal=ブロック、Insert=バー等)
/// - Visual系モードの選択範囲ハイライトもここ(M1はカーソルのみでよい)
/// - アイコン・git status・インデントガイドはバッファ文字列に含まれない
///   Rust側装飾として描く(M5)
pub fn draw(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    git_badges: &HashMap<EntryId, GitBadge>,
    icon_style: IconStyle,
    follow_cursor: bool,
    previous_viewport: Option<TreeViewport>,
) -> TreeViewOutput {
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let text_color = ui.visuals().text_color();
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    let row_height_with_spacing = row_height + ui.spacing().item_spacing.y;
    let selection = display_selection(snapshot);
    let requested_offset = if follow_cursor {
        previous_viewport
            .filter(|_| snapshot.cursor.line < snapshot.lines.len())
            .and_then(|viewport| {
                let cursor_top = snapshot.cursor.line as f32 * row_height_with_spacing;
                follow_offset(
                    cursor_top,
                    cursor_top + row_height,
                    viewport.scroll_offset,
                    viewport.height,
                )
            })
    } else {
        None
    };

    let mut scroll_area = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if let Some(offset) = requested_offset {
        scroll_area = scroll_area.vertical_scroll_offset(offset);
    }
    let output = scroll_area.show_rows(ui, row_height, snapshot.lines.len(), |ui, row_range| {
        let mut cursor_rect = None;
        let first_line = row_range.start;
        for (line_offset, line) in snapshot.lines[row_range].iter().enumerate() {
            let line_index = first_line + line_offset;
            let concealed = conceal::conceal_line(&line.text);
            let painter = ui.painter().clone();
            let icon_galley = painter.layout_no_wrap(
                format!(
                    "{} ",
                    icon::for_display_name_styled(concealed.display, icon_style)
                ),
                font_id.clone(),
                text_color,
            );
            let badge = badge_for_line(&line.text, git_badges);
            let badge_galley = painter.layout_no_wrap(
                format!("{} ", badge.map(badge_character).unwrap_or(" ")),
                font_id.clone(),
                badge_color(ui.visuals(), badge),
            );
            let text_galley =
                painter.layout_no_wrap(concealed.display.to_owned(), font_id.clone(), text_color);
            let icon_width = icon_galley.size().x;
            let badge_width = badge_galley.size().x;
            let text_offset = icon_width + badge_width;
            let width = ui.available_width().max(text_offset + text_galley.size().x);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::hover());

            if let Some((start, cursor)) = selection {
                draw_selection(
                    ui,
                    rect,
                    concealed.display,
                    &snapshot.mode,
                    start,
                    cursor,
                    line_index,
                    &font_id,
                    text_offset,
                );
            }
            painter.galley(rect.min, icon_galley, text_color);
            painter.galley(
                egui::pos2(rect.left() + icon_width, rect.top()),
                badge_galley,
                badge_color(ui.visuals(), badge),
            );
            painter.galley(
                egui::pos2(rect.left() + text_offset, rect.top()),
                text_galley,
                text_color,
            );

            if snapshot.cursor.line == line_index {
                cursor_rect = Some(draw_cursor(
                    ui,
                    rect,
                    concealed.display,
                    &line.text,
                    snapshot,
                    &font_id,
                    text_offset,
                ));
            }
        }
        cursor_rect
    });

    TreeViewOutput {
        cursor_rect: output.inner.filter(|cursor_rect| {
            cursor_rect.bottom() > output.inner_rect.top()
                && cursor_rect.top() < output.inner_rect.bottom()
        }),
        tree_rect: output.inner_rect,
        viewport: TreeViewport {
            scroll_offset: output.state.offset.y,
            height: output.inner_rect.height(),
        },
    }
}

fn display_selection(snapshot: &EditorSnapshot) -> Option<(Cursor, Cursor)> {
    let start = snapshot.visual_start?;
    let start_line = snapshot.lines.get(start.line)?;
    let cursor_line = snapshot.lines.get(snapshot.cursor.line)?;
    Some((
        conceal::display_cursor(&start_line.text, start),
        conceal::display_cursor(&cursor_line.text, snapshot.cursor),
    ))
}

#[allow(clippy::too_many_arguments)]
fn draw_selection(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    display: &str,
    mode: &Mode,
    start: Cursor,
    cursor: Cursor,
    line_index: usize,
    font_id: &egui::FontId,
    text_offset: f32,
) {
    let Some((span_start, span_end)) = selection_span(mode, start, cursor, line_index, display)
    else {
        return;
    };
    let fill = translucent_selection_fill(ui.visuals());
    let painter = ui.painter();

    if matches!(mode, Mode::VisualLine) {
        painter.rect_filled(row_rect, 0.0, fill);
        return;
    }

    let before_width = painter
        .layout_no_wrap(
            display[..span_start].to_owned(),
            font_id.clone(),
            ui.visuals().text_color(),
        )
        .size()
        .x;
    let selected = &display[span_start..span_end];
    let selected_width = painter
        .layout_no_wrap(
            if selected.is_empty() {
                " ".to_owned()
            } else {
                selected.to_owned()
            },
            font_id.clone(),
            ui.visuals().text_color(),
        )
        .size()
        .x
        .max(1.0);
    let selection_rect = egui::Rect::from_min_size(
        egui::pos2(row_rect.left() + text_offset + before_width, row_rect.top()),
        egui::vec2(selected_width, row_rect.height()),
    );
    painter.rect_filled(selection_rect, 0.0, fill);
}

/// Visual系モードの各行について、表示文字列内の選択範囲を半開区間で返す。
fn selection_span(
    mode: &Mode,
    start: Cursor,
    cursor: Cursor,
    line_index: usize,
    line_display: &str,
) -> Option<(usize, usize)> {
    let first_line = start.line.min(cursor.line);
    let last_line = start.line.max(cursor.line);
    if !(first_line..=last_line).contains(&line_index) {
        return None;
    }

    match mode {
        Mode::VisualLine => Some((0, line_display.len())),
        Mode::VisualBlock => {
            let span_start = valid_byte_index(line_display, start.col.min(cursor.col));
            let span_end = byte_after_character(line_display, start.col.max(cursor.col));
            Some((span_start, span_end.max(span_start)))
        }
        Mode::Visual => {
            let (first, last) = if (start.line, start.col) <= (cursor.line, cursor.col) {
                (start, cursor)
            } else {
                (cursor, start)
            };

            if first.line == last.line {
                let span_start = valid_byte_index(line_display, first.col);
                let span_end = byte_after_character(line_display, last.col);
                Some((span_start, span_end.max(span_start)))
            } else if line_index == first.line {
                Some((
                    valid_byte_index(line_display, first.col),
                    line_display.len(),
                ))
            } else if line_index == last.line {
                Some((0, byte_after_character(line_display, last.col)))
            } else {
                Some((0, line_display.len()))
            }
        }
        _ => None,
    }
}

fn byte_after_character(text: &str, requested: usize) -> usize {
    let index = valid_byte_index(text, requested);
    text[index..]
        .chars()
        .next()
        .map_or(index, |character| index + character.len_utf8())
}

fn translucent_selection_fill(visuals: &egui::Visuals) -> egui::Color32 {
    let color = visuals.selection.bg_fill;
    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), color.a().min(96))
}

fn badge_for_line(raw: &str, git_badges: &HashMap<EntryId, GitBadge>) -> Option<GitBadge> {
    let PrefixParse::WithId { id, .. } = fyler_core::grammar::split_id_prefix(raw) else {
        return None;
    };
    git_badges.get(&id).copied()
}

fn badge_character(badge: GitBadge) -> &'static str {
    match badge {
        GitBadge::Modified => "M",
        GitBadge::Added => "A",
        GitBadge::Deleted => "D",
        GitBadge::Renamed => "R",
        GitBadge::Untracked => "?",
        GitBadge::Conflicted => "!",
    }
}

fn badge_color(visuals: &egui::Visuals, badge: Option<GitBadge>) -> egui::Color32 {
    match badge {
        Some(GitBadge::Modified | GitBadge::Renamed) => egui::Color32::from_rgb(230, 190, 60),
        Some(GitBadge::Added) => egui::Color32::from_rgb(80, 200, 120),
        Some(GitBadge::Deleted | GitBadge::Conflicted) => visuals.error_fg_color,
        Some(GitBadge::Untracked) | None => visuals.weak_text_color(),
    }
}

fn draw_cursor(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    display: &str,
    raw: &str,
    snapshot: &EditorSnapshot,
    font_id: &egui::FontId,
    text_offset: f32,
) -> egui::Rect {
    let display_cursor = conceal::display_cursor(raw, snapshot.cursor);
    let byte_index = valid_byte_index(display, display_cursor.col);
    let before = &display[..byte_index];
    let cursor_text = display[byte_index..]
        .chars()
        .next()
        .map(|character| character.to_string())
        .unwrap_or_else(|| " ".to_owned());

    let painter = ui.painter();
    let before_width = painter
        .layout_no_wrap(
            before.to_owned(),
            font_id.clone(),
            ui.visuals().text_color(),
        )
        .size()
        .x;
    let cursor_width = painter
        .layout_no_wrap(
            cursor_text.clone(),
            font_id.clone(),
            ui.visuals().text_color(),
        )
        .size()
        .x
        .max(1.0);
    let cursor_x = row_rect.left() + text_offset + before_width;
    let cursor_rect = egui::Rect::from_min_size(
        egui::pos2(cursor_x, row_rect.top()),
        egui::vec2(cursor_width, row_rect.height()),
    );

    match snapshot.mode {
        Mode::Insert | Mode::Cmdline => {
            painter.line_segment(
                [
                    egui::pos2(cursor_x, row_rect.top()),
                    egui::pos2(cursor_x, row_rect.bottom()),
                ],
                egui::Stroke::new(2.0, ui.visuals().strong_text_color()),
            );
        }
        Mode::Replace => {
            painter.line_segment(
                [
                    egui::pos2(cursor_x, row_rect.bottom() - 1.0),
                    egui::pos2(cursor_x + cursor_width, row_rect.bottom() - 1.0),
                ],
                egui::Stroke::new(2.0, ui.visuals().strong_text_color()),
            );
        }
        _ => {
            painter.rect_filled(cursor_rect, 0.0, ui.visuals().selection.bg_fill);
            painter.text(
                cursor_rect.min,
                egui::Align2::LEFT_TOP,
                cursor_text,
                font_id.clone(),
                ui.visuals().selection.stroke.color,
            );
        }
    }

    cursor_rect
}

fn follow_offset(
    cursor_top: f32,
    cursor_bottom: f32,
    view_top: f32,
    view_height: f32,
) -> Option<f32> {
    if cursor_top < view_top {
        Some(cursor_top)
    } else if cursor_bottom > view_top + view_height {
        Some(cursor_bottom - view_height)
    } else {
        None
    }
}

fn valid_byte_index(text: &str, requested: usize) -> usize {
    let mut index = requested.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_badge_characters_match_decoration_contract() {
        assert_eq!(badge_character(GitBadge::Modified), "M");
        assert_eq!(badge_character(GitBadge::Added), "A");
        assert_eq!(badge_character(GitBadge::Deleted), "D");
        assert_eq!(badge_character(GitBadge::Renamed), "R");
        assert_eq!(badge_character(GitBadge::Untracked), "?");
        assert_eq!(badge_character(GitBadge::Conflicted), "!");
    }

    #[test]
    fn git_badge_is_resolved_only_from_valid_id_prefix() {
        let badges = HashMap::from([(EntryId(7), GitBadge::Modified)]);

        assert_eq!(
            badge_for_line("/007 src/main.rs", &badges),
            Some(GitBadge::Modified)
        );
        assert_eq!(badge_for_line("new.txt", &badges), None);
        assert_eq!(badge_for_line("/0", &badges), None);
    }

    #[test]
    fn cursor_byte_index_is_clamped_to_utf8_boundary() {
        assert_eq!(valid_byte_index("新a", 1), 0);
        assert_eq!(valid_byte_index("新a", 3), 3);
        assert_eq!(valid_byte_index("新a", usize::MAX), 4);
    }

    #[test]
    fn cursor_follow_scrolls_to_cursor_top_when_cursor_is_above_viewport() {
        assert_eq!(follow_offset(20.0, 36.0, 40.0, 100.0), Some(20.0));
    }

    #[test]
    fn cursor_follow_scrolls_minimally_when_cursor_is_below_viewport() {
        assert_eq!(follow_offset(140.0, 156.0, 20.0, 100.0), Some(56.0));
    }

    #[test]
    fn cursor_follow_keeps_scroll_when_cursor_is_inside_viewport() {
        assert_eq!(follow_offset(40.0, 56.0, 20.0, 100.0), None);
    }

    #[test]
    fn cursor_follow_prioritizes_top_when_row_is_taller_than_viewport() {
        assert_eq!(follow_offset(20.0, 60.0, 30.0, 10.0), Some(20.0));
    }

    #[test]
    fn charwise_selection_normalizes_same_line_direction() {
        let forward = selection_span(
            &Mode::Visual,
            Cursor { line: 2, col: 1 },
            Cursor { line: 2, col: 3 },
            2,
            "abcde",
        );
        let reverse = selection_span(
            &Mode::Visual,
            Cursor { line: 2, col: 3 },
            Cursor { line: 2, col: 1 },
            2,
            "abcde",
        );

        assert_eq!(forward, Some((1, 4)));
        assert_eq!(reverse, forward);
    }

    #[test]
    fn charwise_selection_spans_forward_and_reverse_multiple_lines() {
        for (start, cursor) in [
            (Cursor { line: 1, col: 2 }, Cursor { line: 3, col: 1 }),
            (Cursor { line: 3, col: 1 }, Cursor { line: 1, col: 2 }),
        ] {
            assert_eq!(
                selection_span(&Mode::Visual, start, cursor, 1, "abcde"),
                Some((2, 5))
            );
            assert_eq!(
                selection_span(&Mode::Visual, start, cursor, 2, "middle"),
                Some((0, 6))
            );
            assert_eq!(
                selection_span(&Mode::Visual, start, cursor, 3, "xyz"),
                Some((0, 2))
            );
            assert_eq!(
                selection_span(&Mode::Visual, start, cursor, 0, "outside"),
                None
            );
        }
    }

    #[test]
    fn block_selection_normalizes_columns_and_clamps_utf8_boundaries() {
        let start = Cursor { line: 4, col: 4 };
        let cursor = Cursor { line: 1, col: 1 };

        assert_eq!(
            selection_span(&Mode::VisualBlock, start, cursor, 2, "abcdef"),
            Some((1, 5))
        );
        assert_eq!(
            selection_span(&Mode::VisualBlock, start, cursor, 2, "新ab"),
            Some((0, 5))
        );
        assert_eq!(
            selection_span(&Mode::VisualBlock, start, cursor, 0, "abcdef"),
            None
        );
    }

    #[test]
    fn linewise_selection_covers_each_selected_row() {
        let start = Cursor { line: 5, col: 8 };
        let cursor = Cursor { line: 3, col: 1 };

        assert_eq!(
            selection_span(&Mode::VisualLine, start, cursor, 3, "abc"),
            Some((0, 3))
        );
        assert_eq!(
            selection_span(&Mode::VisualLine, start, cursor, 4, ""),
            Some((0, 0))
        );
        assert_eq!(
            selection_span(&Mode::VisualLine, start, cursor, 6, "outside"),
            None
        );
    }
}
