//! ツリー本体の描画。

use std::collections::{HashMap, HashSet};

use eframe::egui;
use fyler_core::editor::{Cursor, EditorSnapshot, Mode, SearchHighlight};
use fyler_core::fileinfo::{FileInfo, human_readable_size};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::EntryId;
use fyler_core::pane::PaneId;

use crate::{conceal, icon, theme};

/// 1階層ぶんの装飾インデント。文字列のタブ幅とは独立したGUI座標。
const INDENT_UNIT_PX: f32 = 20.0;
const TREE_LEFT_PADDING: f32 = 12.0;

/// 前フレームのツリー可視範囲。
#[derive(Debug, Clone, Copy)]
pub struct TreeViewport {
    /// 可視範囲上端のスクロールオフセット。
    pub scroll_offset: f32,
    /// スクロール領域の表示高。
    pub height: f32,
    /// この可視範囲を記録した時点のカーソル行。
    pub cursor_line: usize,
}

/// 行クリックの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowClickKind {
    /// 単純click(左クリック1回)。
    Single,
    /// 左クリックのdouble-click。
    Double,
    /// Shift押下中の左クリック(anchorからlinewise選択する対象行)。
    Shift,
    /// 右クリック(secondary button)。
    Secondary,
}

/// ツリー行へのクリック結果。
#[derive(Debug, Clone, Copy)]
pub struct RowClick {
    /// クリックされた表示行(0始まり、`EditorSnapshot::lines`のindex)。
    pub line: usize,
    pub kind: RowClickKind,
    /// クリック位置(screen座標)。context menuの表示位置に使う。
    pub pos: egui::Pos2,
    /// 行がID付き(保存済み)かどうか。context menuのgatingに使う。
    pub has_id: bool,
    /// 行がディレクトリかどうか。
    pub is_dir: bool,
    /// 行が.zipアーカイブのファイルか。context menuのExtract gatingに使う。
    pub is_zip: bool,
}

/// ツリー行からOLE drag-out候補のdragが開始されたことを示す情報。
///
/// fylerのwindow**内**でのdrag(pane間含む)はOLEを使わずegui内で完結し、
/// pointerがwindow境界を離れた時点でのみapp層がOLE dragへ移行する
/// (親セッション設計確定事項)。この構造体はGUI側drag状態機械の開始点。
#[derive(Debug, Clone, Copy)]
pub struct RowDragStart {
    /// dragが開始された表示行(0始まり)。
    pub line: usize,
    /// 行がID付き(保存済み)かどうか。未保存行はdrag対象にしない。
    pub has_id: bool,
    pub is_dir: bool,
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
    /// `drag_active`(inbound drop)または`internal_drag_active`(GUI内tree drag)
    /// のいずれか時、pointer直下の行(取り込み/dropの対象候補)。
    pub drop_target_line: Option<usize>,
    /// このフレームで行のdragが開始されていればその詳細(最初の1件のみ)。
    pub drag_started: Option<RowDragStart>,
    /// このフレームで行がクリックされていればその詳細。
    pub click: Option<RowClick>,
    /// 行の外(ツリー領域内の空白)がクリックされたか(pane focus要求のみ、
    /// カーソルは変えない)。スクロールバー領域のクリックは含まない。
    pub blank_clicked: bool,
}

/// egui responseフラグから行クリックの種別を決める純ロジック(unit test対象)。
/// `secondary`(右click)を最優先、次に`double`、最後に`primary`(左click)を見る。
/// いずれも成立しなければ`None`(この行はクリックされていない)。
fn classify_row_click(
    secondary: bool,
    double: bool,
    primary: bool,
    shift: bool,
) -> Option<RowClickKind> {
    if secondary {
        Some(RowClickKind::Secondary)
    } else if double {
        Some(RowClickKind::Double)
    } else if primary {
        Some(if shift {
            RowClickKind::Shift
        } else {
            RowClickKind::Single
        })
    } else {
        None
    }
}

/// 表示名が.zipアーカイブのファイルかを判定する純ロジック(unit test対象)。
/// ディレクトリsuffix付きは常にfalse。拡張子はcase-insensitiveに比較する。
/// Explorer同様、名前全体が`.zip`のファイルもzip扱いとする(ends_with判定)。
pub(crate) fn display_name_is_zip(display: &str) -> bool {
    let (name, is_dir) = fyler_core::grammar::split_dir_suffix(display);
    !is_dir
        && name
            .get(name.len().wrapping_sub(4)..)
            .is_some_and(|ext| ext.eq_ignore_ascii_case(".zip"))
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
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    git_badges: &HashMap<EntryId, GitBadge>,
    incomplete_dirs: &HashSet<EntryId>,
    collapsed_dirs: &HashSet<EntryId>,
    file_infos: &HashMap<EntryId, FileInfo>,
    previous_viewport: Option<TreeViewport>,
    pane_id: PaneId,
    is_active: bool,
    drag_active: bool,
    // GUI window内で開始したtree行drag(pane間dragを含む)が進行中か。
    // trueの間、全行が`contains_pointer()`ベースでdrop先候補を判定する。
    internal_drag_active: bool,
) -> TreeViewOutput {
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let icon_font_id = egui::FontId::new(font_id.size, icon::font_family());
    let text_color = theme::TEXT;
    let row_height = theme::TREE_ROW_HEIGHT;
    // 行間の余白をゼロにして item を詰める(pitch = 行高 24px)。
    ui.spacing_mut().item_spacing.y = 0.0;
    let row_pitch = row_height + ui.spacing().item_spacing.y;
    let selection = display_selection(snapshot);
    let requested_offset = previous_viewport
        .filter(|viewport| viewport.cursor_line != snapshot.cursor.line)
        .filter(|_| snapshot.cursor.line < snapshot.lines.len())
        .and_then(|viewport| {
            let (cursor_top, cursor_bottom) =
                row_bounds(snapshot.cursor.line, row_height, row_pitch);
            follow_offset(
                cursor_top,
                cursor_bottom,
                viewport.scroll_offset,
                viewport.height,
            )
        });

    let mut scroll_area = egui::ScrollArea::vertical()
        .id_salt(pane_id.get())
        .auto_shrink([false, false]);
    if let Some(offset) = requested_offset {
        scroll_area = scroll_area.vertical_scroll_offset(offset);
    }
    let output = scroll_area.show_rows(ui, row_height, snapshot.lines.len(), |ui, row_range| {
        let mut cursor_rect = None;
        let mut hovered_line = None;
        let mut row_click: Option<RowClick> = None;
        let mut row_drag_start: Option<RowDragStart> = None;
        let first_line = row_range.start;
        for (line_offset, line) in snapshot.lines[row_range].iter().enumerate() {
            let line_index = first_line + line_offset;
            let concealed = conceal::conceal_line(&line.text);
            let indent_px = indent_offset(concealed.depth, INDENT_UNIT_PX);
            let painter = ui.painter().clone();
            let (_, is_dir) = fyler_core::grammar::split_dir_suffix(concealed.display);
            let icon_color = if is_dir {
                theme::BLUE
            } else {
                theme::TEXT_MUTED
            };
            let icon_galley = painter.layout_no_wrap(
                format!(
                    "{} ",
                    icon::for_display_name(
                        concealed.display,
                        line_is_expanded_dir(&line.text, collapsed_dirs),
                    )
                ),
                icon_font_id.clone(),
                icon_color,
            );
            let badge = badge_for_line(&line.text, git_badges);
            let badge_galley = painter.layout_no_wrap(
                badge.map(badge_character).unwrap_or(" ").to_owned(),
                font_id.clone(),
                badge_color(ui.visuals(), badge),
            );
            let text_galley = layout_line_text(
                &painter,
                concealed.display,
                &font_id,
                text_color,
                snapshot.search.as_ref(),
            );
            let incomplete = incomplete_for_line(&line.text, incomplete_dirs);
            let incomplete_galley = painter.layout_no_wrap(
                if incomplete {
                    "[unreadable]".to_owned()
                } else {
                    String::new()
                },
                egui::FontId::monospace(11.0),
                theme::RED,
            );
            let modified_text = file_info_for_line(&line.text, file_infos)
                .and_then(|info| info.modified.clone())
                .unwrap_or_default();
            let modified_galley = painter.layout_no_wrap(
                modified_text,
                egui::FontId::monospace(11.0),
                theme::TEXT_MUTED,
            );
            let size_text = file_info_for_line(&line.text, file_infos)
                .and_then(|info| info.size)
                .map(human_readable_size)
                .unwrap_or_default();
            let size_galley =
                painter.layout_no_wrap(size_text, egui::FontId::monospace(11.0), theme::TEXT_MUTED);
            let icon_width = icon_galley.size().x;
            let text_width = text_galley.size().x;
            let text_offset = TREE_LEFT_PADDING + indent_px + icon_width;
            let width = ui.available_width().max(
                text_offset
                    + text_width
                    + modified_galley.size().x
                    + size_galley.size().x
                    + incomplete_galley.size().x
                    + badge_galley.size().x
                    + 44.0,
            );
            let (rect, response) = ui
                .allocate_exact_size(egui::vec2(width, row_height), egui::Sense::click_and_drag());
            let metadata_cluster = layout_metadata_cluster(
                rect.right(),
                badge.is_some().then(|| badge_galley.size().x),
                incomplete.then(|| incomplete_galley.size().x),
                (modified_galley.size().x > 0.0).then(|| modified_galley.size().x),
                (size_galley.size().x > 0.0).then(|| size_galley.size().x),
            );
            let has_id = matches!(
                fyler_core::grammar::split_id_prefix(&line.text),
                PrefixParse::WithId { .. }
            );
            if row_click.is_none()
                && let Some(kind) = classify_row_click(
                    response.secondary_clicked(),
                    response.double_clicked(),
                    response.clicked(),
                    ui.input(|input| input.modifiers.shift),
                )
            {
                let pos = response
                    .interact_pointer_pos()
                    .unwrap_or_else(|| rect.center());
                row_click = Some(RowClick {
                    line: line_index,
                    kind,
                    pos,
                    has_id,
                    is_dir,
                    is_zip: display_name_is_zip(concealed.display),
                });
            }
            if row_drag_start.is_none() && response.drag_started_by(egui::PointerButton::Primary) {
                row_drag_start = Some(RowDragStart {
                    line: line_index,
                    has_id,
                    is_dir,
                });
            }

            if response.hovered() {
                painter.rect_filled(rect, 0.0, theme::HOVER);
                if drag_active {
                    hovered_line = Some(line_index);
                }
            }
            // GUI内tree drag中は、drag元の行がpointer/press captureを保持するため
            // 他行の`hovered()`は立たない。`contains_pointer()`はdrag中でも
            // hit-testされるため、drop先候補の判定にはこちらを使う。
            if internal_drag_active && response.contains_pointer() {
                hovered_line = Some(line_index);
            }
            if snapshot.cursor.line == line_index {
                painter.rect_filled(rect, 0.0, theme::accent_selection_fill());
                painter.rect_filled(
                    egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
                    0.0,
                    theme::ACCENT_DIM,
                );
            }
            for depth in 0..concealed.depth {
                let x = rect.left() + TREE_LEFT_PADDING + (depth as f32 + 0.5) * INDENT_UNIT_PX;
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
                );
            }

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
                    metadata_cluster.cluster_left,
                );
            }
            let icon_y = rect.center().y - icon_galley.size().y / 2.0;
            painter.galley(
                egui::pos2(rect.left() + TREE_LEFT_PADDING + indent_px, icon_y),
                icon_galley,
                icon_color,
            );
            let text_y = rect.center().y - text_galley.size().y / 2.0;
            painter.galley(
                egui::pos2(rect.left() + text_offset, text_y),
                text_galley,
                text_color,
            );
            if let Some(x) = metadata_cluster.badge_x {
                painter.galley(
                    egui::pos2(x, rect.center().y - badge_galley.size().y / 2.0),
                    badge_galley,
                    badge_color(ui.visuals(), badge),
                );
            }
            if let Some(x) = metadata_cluster.incomplete_x {
                painter.galley(
                    egui::pos2(x, rect.center().y - incomplete_galley.size().y / 2.0),
                    incomplete_galley,
                    theme::RED,
                );
            }
            if let Some(x) = metadata_cluster.modified_x {
                painter.galley(
                    egui::pos2(x, rect.center().y - modified_galley.size().y / 2.0),
                    modified_galley,
                    theme::TEXT_MUTED,
                );
            }
            if let Some(x) = metadata_cluster.size_x {
                painter.galley(
                    egui::pos2(x, rect.center().y - size_galley.size().y / 2.0),
                    size_galley,
                    theme::TEXT_MUTED,
                );
            }

            if is_active && snapshot.cursor.line == line_index {
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
        (cursor_rect, hovered_line, row_click, row_drag_start)
    });

    let (cursor_rect, hovered_line, click, drag_started) = output.inner;
    let drop_target_line =
        hovered_line.and_then(|hovered| resolve_drop_target_line(&snapshot.lines, hovered));
    if let Some(target) = drop_target_line {
        let (top, bottom) = row_bounds(target, row_height, row_pitch);
        let screen_top = output.inner_rect.top() - output.state.offset.y + top;
        let screen_bottom = output.inner_rect.top() - output.state.offset.y + bottom;
        let highlight_rect = egui::Rect::from_min_max(
            egui::pos2(output.inner_rect.left(), screen_top),
            egui::pos2(output.inner_rect.right(), screen_bottom),
        );
        if highlight_rect.bottom() > output.inner_rect.top()
            && highlight_rect.top() < output.inner_rect.bottom()
        {
            ui.painter().with_clip_rect(output.inner_rect).rect_stroke(
                highlight_rect,
                0.0,
                egui::Stroke::new(2.0, theme::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
    }
    // 行がクリックされなかった場合のみ、ツリー可視領域内の空白clickを判定する
    // (行は常にツリー全幅を覆うため、行外clickは「行より下の余白」のみになる。
    // スクロールバーは可視領域の外側にあるため誤認しない)。
    let blank_clicked = click.is_none()
        && ui.input(|input| input.pointer.primary_clicked() || input.pointer.secondary_clicked())
        && ui
            .input(|input| input.pointer.interact_pos())
            .is_some_and(|pos| output.inner_rect.contains(pos));

    TreeViewOutput {
        cursor_rect: cursor_rect.filter(|cursor_rect| {
            cursor_rect.bottom() > output.inner_rect.top()
                && cursor_rect.top() < output.inner_rect.bottom()
        }),
        tree_rect: output.inner_rect,
        viewport: TreeViewport {
            scroll_offset: output.state.offset.y,
            height: output.inner_rect.height(),
            cursor_line: snapshot.cursor.line,
        },
        drop_target_line,
        drag_started,
        click,
        blank_clicked,
    }
}

/// 行の表示テキストのgalleyを作る。検索マッチがあればその範囲に背景色を敷く
/// (nvimの `hlsearch` はRust描画に届かないため、ここで再現する)。
///
/// マッチなし(検索なし・`:noh` 後)は素の `layout_no_wrap` を使う(高速パス)。
/// マッチはVim既定の Search 配色に倣い、黒文字 + 黄背景でテーマ非依存に見せる。
fn layout_line_text(
    painter: &egui::Painter,
    display: &str,
    font_id: &egui::FontId,
    text_color: egui::Color32,
    search: Option<&SearchHighlight>,
) -> std::sync::Arc<egui::Galley> {
    let spans = search
        .map(|search| search.match_spans(display))
        .unwrap_or_default();
    if spans.is_empty() {
        return painter.layout_no_wrap(display.to_owned(), font_id.clone(), text_color);
    }

    let normal = egui::TextFormat {
        font_id: font_id.clone(),
        color: text_color,
        ..Default::default()
    };
    let matched = egui::TextFormat {
        font_id: font_id.clone(),
        color: egui::Color32::BLACK,
        background: egui::Color32::from_rgb(240, 220, 90),
        ..Default::default()
    };

    let mut job = egui::text::LayoutJob::default();
    let mut cursor = 0;
    for (start, end) in spans {
        if start > cursor {
            job.append(&display[cursor..start], 0.0, normal.clone());
        }
        job.append(&display[start..end], 0.0, matched.clone());
        cursor = end;
    }
    if cursor < display.len() {
        job.append(&display[cursor..], 0.0, normal);
    }
    painter.layout_job(job)
}

/// 行がディレクトリで、かつ展開状態(= 折りたたみ集合に含まれない)かを返す。
///
/// 展開状態の正典は app 層の `collapsed_dirs`。子の有無に依存しないため、
/// 子を持たない空ディレクトリを展開しても正しく開アイコンになる
/// (バッファの次行だけからは空展開と折りたたみを区別できない)。
fn line_is_expanded_dir(raw: &str, collapsed_dirs: &HashSet<EntryId>) -> bool {
    let PrefixParse::WithId { id, rest } = fyler_core::grammar::split_id_prefix(raw) else {
        return false;
    };
    let (_, name) = fyler_core::grammar::split_indent(rest);
    if !fyler_core::grammar::split_dir_suffix(name).1 {
        return false;
    }
    !collapsed_dirs.contains(&id)
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
    metadata_left: f32,
) {
    let Some((span_start, span_end)) = selection_span(mode, start, cursor, line_index, display)
    else {
        return;
    };
    let fill = translucent_selection_fill();
    let painter = ui.painter();

    if matches!(mode, Mode::VisualLine) {
        // オリジナルfyler同様、エントリ自身のインデントに関係なくツリー左端から
        // 塗る。右端は行末ではなく右詰めメタデータクラスタ(size/modified/
        // incomplete/badge)の開始位置で止める("until last updated section")。
        let selection_rect = egui::Rect::from_min_max(
            egui::pos2(row_rect.left() + TREE_LEFT_PADDING, row_rect.top()),
            egui::pos2(metadata_left, row_rect.bottom()),
        );
        painter.rect_filled(selection_rect, 0.0, fill);
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

fn translucent_selection_fill() -> egui::Color32 {
    // Visual選択が確実に視認できるよう、Visualモードバッジと同系のBLUEで塗る。
    egui::Color32::from_rgba_unmultiplied(theme::BLUE.r(), theme::BLUE.g(), theme::BLUE.b(), 72)
}

fn badge_for_line(raw: &str, git_badges: &HashMap<EntryId, GitBadge>) -> Option<GitBadge> {
    let PrefixParse::WithId { id, .. } = fyler_core::grammar::split_id_prefix(raw) else {
        return None;
    };
    git_badges.get(&id).copied()
}

fn file_info_for_line<'a>(
    raw: &str,
    file_infos: &'a HashMap<EntryId, FileInfo>,
) -> Option<&'a FileInfo> {
    let PrefixParse::WithId { id, .. } = fyler_core::grammar::split_id_prefix(raw) else {
        return None;
    };
    file_infos.get(&id)
}

fn incomplete_for_line(raw: &str, incomplete_dirs: &HashSet<EntryId>) -> bool {
    let PrefixParse::WithId { id, .. } = fyler_core::grammar::split_id_prefix(raw) else {
        return false;
    };
    incomplete_dirs.contains(&id)
}

/// 右詰めメタデータクラスタ(badge/incomplete/modified/size)の描画x座標。
/// [`layout_metadata_cluster`] が算出する。
#[derive(Debug, Clone, Copy, PartialEq)]
struct MetadataClusterLayout {
    badge_x: Option<f32>,
    incomplete_x: Option<f32>,
    modified_x: Option<f32>,
    size_x: Option<f32>,
    /// クラスタ全体の左端x座標。要素が一つも無ければ`row_right`をそのまま返す
    /// (VisualLine選択が行末まで塗られる、という呼び出し側の契約に対応)。
    cluster_left: f32,
}

/// 右詰めメタデータクラスタの各要素を row_rect の右端から敷き詰める純関数
/// (egui非依存、unit test対象)。右から badge → incomplete → modified → size
/// の順に並べる。各`_width`引数は要素が存在しない行では`None`。
fn layout_metadata_cluster(
    row_right: f32,
    badge_width: Option<f32>,
    incomplete_width: Option<f32>,
    modified_width: Option<f32>,
    size_width: Option<f32>,
) -> MetadataClusterLayout {
    let mut right = row_right - 16.0;
    let mut any = false;

    let badge_x = badge_width.map(|w| {
        right -= w;
        any = true;
        right
    });
    let incomplete_x = incomplete_width.map(|w| {
        right -= w + 12.0;
        any = true;
        right
    });
    let modified_x = modified_width.map(|w| {
        right -= w + 12.0;
        any = true;
        right
    });
    let size_x = size_width.map(|w| {
        right -= w + 12.0;
        any = true;
        right
    });

    MetadataClusterLayout {
        badge_x,
        incomplete_x,
        modified_x,
        size_x,
        cluster_left: if any { right } else { row_right },
    }
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

fn badge_color(_visuals: &egui::Visuals, badge: Option<GitBadge>) -> egui::Color32 {
    match badge {
        Some(GitBadge::Modified | GitBadge::Renamed) => theme::YELLOW,
        Some(GitBadge::Added) => theme::GREEN,
        Some(GitBadge::Deleted | GitBadge::Conflicted) => theme::RED,
        Some(GitBadge::Untracked) | None => theme::TEXT_FAINT,
    }
}

fn indent_offset(depth: usize, unit_px: f32) -> f32 {
    depth as f32 * unit_px
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
    let cursor_size = painter
        .layout_no_wrap(cursor_text.clone(), font_id.clone(), theme::TEXT)
        .size();
    let cursor_width = cursor_size.x.max(1.0);
    let cursor_x = row_rect.left() + text_offset + before_width;
    let cursor_y = row_rect.center().y - cursor_size.y / 2.0;
    let cursor_rect = egui::Rect::from_min_size(
        egui::pos2(cursor_x, cursor_y),
        egui::vec2(cursor_width, cursor_size.y),
    );

    // vim準拠のカーソル形状(点滅なし)。
    match snapshot.mode {
        Mode::Insert => {
            // 縦バー(細)。文字色は変えない。
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(cursor_x, cursor_rect.top()),
                    egui::pos2(cursor_x + 2.0, cursor_rect.bottom()),
                ),
                0.0,
                theme::BLUE,
            );
        }
        Mode::Replace => {
            // 下線。
            painter.line_segment(
                [
                    egui::pos2(cursor_x, cursor_rect.bottom() - 1.0),
                    egui::pos2(cursor_x + cursor_width, cursor_rect.bottom() - 1.0),
                ],
                egui::Stroke::new(2.0, theme::TEXT),
            );
        }
        _ => {
            // ブロック(reverse video)。セルをTEXT色で塗り、文字を背景色で描き直す。
            painter.rect_filled(cursor_rect, 0.0, theme::TEXT);
            painter.text(
                cursor_rect.left_top(),
                egui::Align2::LEFT_TOP,
                &cursor_text,
                font_id.clone(),
                theme::CANVAS,
            );
        }
    }

    cursor_rect
}

fn row_bounds(line: usize, row_height: f32, row_pitch: f32) -> (f32, f32) {
    let top = line as f32 * row_pitch;
    (top, top + row_height)
}

/// pointer直下の行から、inbound dropの取り込み先候補行を解決する。
/// ディレクトリ行はその行、file/symlink行は最も近い祖先ディレクトリ行を返す
/// (祖先が無いルート直下ファイルは`None`。呼び出し側でrootとして扱う)。
fn resolve_drop_target_line(
    lines: &[fyler_core::editor::EditorLine],
    hovered: usize,
) -> Option<usize> {
    let hovered_line = lines.get(hovered)?;
    let hovered_concealed = conceal::conceal_line(&hovered_line.text);
    let (_, is_dir) = fyler_core::grammar::split_dir_suffix(hovered_concealed.display);
    if is_dir {
        return Some(hovered);
    }
    (0..hovered).rev().find(|&candidate| {
        conceal::conceal_line(&lines[candidate].text).depth < hovered_concealed.depth
    })
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

    fn lines_from(texts: &[&str]) -> Vec<fyler_core::editor::EditorLine> {
        texts
            .iter()
            .map(|text| fyler_core::editor::EditorLine::new(*text))
            .collect()
    }

    #[test]
    fn display_name_is_zip_matches_extension_case_insensitively() {
        assert!(display_name_is_zip("Archive.ZIP"));
        assert!(display_name_is_zip("bundle.zip"));
        assert!(!display_name_is_zip("photos/"));
        assert!(!display_name_is_zip("backup.zip/"));
        assert!(!display_name_is_zip("report.txt"));
        assert!(!display_name_is_zip("zip"));
    }

    #[test]
    fn resolve_drop_target_line_targets_hovered_directory_itself() {
        let lines = lines_from(&["/001 dir/", "/002 \tfile.txt"]);
        assert_eq!(resolve_drop_target_line(&lines, 0), Some(0));
    }

    #[test]
    fn resolve_drop_target_line_targets_nearest_ancestor_directory_for_a_file() {
        let lines = lines_from(&["/001 dir/", "/002 \tsub/", "/003 \t\tfile.txt"]);
        assert_eq!(resolve_drop_target_line(&lines, 2), Some(1));
    }

    #[test]
    fn resolve_drop_target_line_is_none_for_a_root_level_file() {
        let lines = lines_from(&["/001 file.txt"]);
        assert_eq!(resolve_drop_target_line(&lines, 0), None);
    }

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
    fn row_click_classification_priority_secondary_double_single() {
        // 右clickは他フラグの有無に関わらず最優先。
        assert_eq!(
            classify_row_click(true, true, true, true),
            Some(RowClickKind::Secondary)
        );
        // double-clickはSecondaryが立っていなければ優先。
        assert_eq!(
            classify_row_click(false, true, true, false),
            Some(RowClickKind::Double)
        );
        // 単純click(Shift無し)。
        assert_eq!(
            classify_row_click(false, false, true, false),
            Some(RowClickKind::Single)
        );
        // Shift押下中のclickはShift種別。
        assert_eq!(
            classify_row_click(false, false, true, true),
            Some(RowClickKind::Shift)
        );
        // どれも立っていなければクリックなし。
        assert_eq!(classify_row_click(false, false, false, false), None);
    }

    #[test]
    fn expanded_dir_is_decided_by_collapsed_set_not_child_presence() {
        let collapsed = HashSet::from([EntryId(2)]);

        // 子を持たない空dirでも、折りたたみ集合に無ければ展開(開アイコン)。
        // これが旧「次行が深いか」ヒューリスティックで壊れていたケース。
        assert!(line_is_expanded_dir("/001 empty/", &collapsed));
        // 折りたたみ集合にあるdirは折りたたみ。
        assert!(!line_is_expanded_dir("/002 collapsed/", &collapsed));
        // ファイル行はディレクトリではない。
        assert!(!line_is_expanded_dir("/003 main.rs", &collapsed));
        // ID未割当(新規作成)行は展開扱いにしない。
        assert!(!line_is_expanded_dir("newdir/", &collapsed));
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
    fn incomplete_marker_is_resolved_only_from_valid_id_prefix() {
        let incomplete = HashSet::from([EntryId(7)]);

        assert!(incomplete_for_line("/007 blocked/", &incomplete));
        assert!(!incomplete_for_line("/008 readable/", &incomplete));
        assert!(!incomplete_for_line("new/", &incomplete));
    }

    #[test]
    fn metadata_cluster_layout_places_elements_right_to_left_badge_incomplete_modified_size() {
        let layout = layout_metadata_cluster(400.0, Some(10.0), Some(20.0), Some(30.0), Some(15.0));

        // badgeは行右端16px内側から幅ぶんそのまま。
        assert_eq!(layout.badge_x, Some(400.0 - 16.0 - 10.0));
        // 以降は各要素の左に12pxの余白を挟んで並ぶ。
        assert_eq!(layout.incomplete_x, Some(374.0 - 20.0 - 12.0));
        assert_eq!(layout.modified_x, Some(342.0 - 30.0 - 12.0));
        assert_eq!(layout.size_x, Some(300.0 - 15.0 - 12.0));
        // クラスタ全体の左端はsizeのさらに左端に一致する。
        assert_eq!(layout.cluster_left, layout.size_x.unwrap());
    }

    #[test]
    fn metadata_cluster_layout_skips_absent_elements() {
        let layout = layout_metadata_cluster(400.0, None, None, Some(30.0), None);

        assert_eq!(layout.badge_x, None);
        assert_eq!(layout.incomplete_x, None);
        assert_eq!(layout.modified_x, Some(400.0 - 16.0 - 30.0 - 12.0));
        assert_eq!(layout.size_x, None);
        assert_eq!(layout.cluster_left, layout.modified_x.unwrap());
    }

    #[test]
    fn metadata_cluster_layout_falls_back_to_row_right_when_nothing_is_shown() {
        let layout = layout_metadata_cluster(400.0, None, None, None, None);

        assert_eq!(layout.cluster_left, 400.0);
    }

    #[test]
    fn indent_offset_scales_depth_by_measured_unit_width() {
        assert_eq!(indent_offset(0, 8.0), 0.0);
        assert_eq!(indent_offset(1, 8.0), 8.0);
        assert_eq!(indent_offset(3, 8.0), 24.0);
    }

    #[test]
    fn cursor_byte_index_is_clamped_to_utf8_boundary() {
        assert_eq!(valid_byte_index("新a", 1), 0);
        assert_eq!(valid_byte_index("新a", 3), 3);
        assert_eq!(valid_byte_index("新a", usize::MAX), 4);
    }

    #[test]
    fn cursor_row_bounds_include_show_rows_item_spacing() {
        assert_eq!(row_bounds(20, 24.0, 28.0), (560.0, 584.0));
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
