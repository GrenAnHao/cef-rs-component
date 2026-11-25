//! Reusable interactive GPUI text field widget with caret, selection, hover/focus
//! animations, mouse hit-testing/drag selection, keyboard editing, and Win32 IME
//! (`EntityInputHandler`) support.

use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Background, Bounds, ClipboardItem, ContentMask,
    Context, Corners, CursorStyle, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, GlobalElementId, Hsla, InspectorElementId,
    InteractiveElement as _, IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    PaintQuad, ParentElement as _, Pixels, Point, ShapedLine, SharedString, Style, Styled as _,
    TextAlign, TextRun, UnderlineStyle, Window, div, ease_out_quint, fill, hsla, point,
    prelude::FluentBuilder as _, px, relative, rgb, rgba, size, BoxShadow,
};

use super::model::{TextBuffer, now_monotonic_ms};

#[link(name = "user32")]
unsafe extern "system" {
    fn GetFocus() -> isize;
    fn SetFocus(hwnd: isize) -> isize;
    fn IsChild(parent: isize, child: isize) -> i32;
}

/// Ensures Win32 keyboard focus belongs to the GPUI parent HWND (not a native
/// child HWND browser), so `WM_KEYDOWN`/`WM_CHAR`/`WM_IME_*` reach GPUI controls.
pub(crate) fn ensure_parent_window_win32_focus(window: &Window) {
    let Some(hwnd) = rust_cef_gpui::window_hwnd(window) else {
        return;
    };
    let raw = hwnd.get() as isize;
    if raw != 0 && unsafe { GetFocus() } != raw {
        unsafe {
            let _ = SetFocus(raw);
        }
    }
}

/// Returns true when Win32 focus currently sits inside a child HWND owned by
/// `window` (for example, a `NativeChildHwnd` CEF browser).
pub(crate) fn child_hwnd_owns_win32_focus(window: &Window) -> bool {
    let Some(hwnd) = rust_cef_gpui::window_hwnd(window) else {
        return false;
    };
    let raw = hwnd.get() as isize;
    if raw == 0 {
        return false;
    }
    let focused = unsafe { GetFocus() };
    focused != 0 && focused != raw && unsafe { IsChild(raw, focused) } != 0
}

pub(crate) struct ShapedRow {
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: ShapedLine,
}

#[derive(Default)]
pub(crate) struct TextFieldLayoutCache {
    pub bounds: Option<Bounds<Pixels>>,
    pub line_height: Pixels,
    pub rows: Vec<ShapedRow>,
    pub scroll_x: f32,
    pub scroll_y: f32,
    pub handler_painted: bool,
}

impl TextFieldLayoutCache {
    pub(crate) fn reset_scroll(&mut self) {
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
    }

    pub(crate) fn index_for_point(
        &self,
        text: &str,
        point: Point<Pixels>,
        multiline: bool,
    ) -> usize {
        if text.is_empty() || self.rows.is_empty() {
            return 0;
        }
        let Some(bounds) = self.bounds else {
            return text.len();
        };
        let row_idx = if !multiline || self.rows.len() <= 1 {
            0
        } else {
            let lh = f32::from(self.line_height).max(1.0);
            let local_y = f32::from(point.y) - f32::from(bounds.origin.y) + self.scroll_y;
            if local_y < 0.0 {
                return 0;
            }
            let idx = (local_y / lh).floor() as usize;
            if idx >= self.rows.len() {
                return text.len();
            }
            idx
        };
        let row = &self.rows[row_idx];
        let local_x = (f32::from(point.x) - f32::from(bounds.origin.x) + self.scroll_x).max(0.0);
        let rel = row.line.closest_index_for_x(px(local_x));
        (row.start_byte + rel).min(row.end_byte).min(text.len())
    }

    pub(crate) fn vertical_move_offset(
        &self,
        text: &str,
        current_offset: usize,
        delta_rows: isize,
    ) -> usize {
        if self.rows.is_empty() {
            return current_offset.min(text.len());
        }
        let clamped = current_offset.min(text.len());
        let cur_row_idx = self
            .rows
            .iter()
            .position(|r| clamped >= r.start_byte && clamped <= r.end_byte)
            .unwrap_or_else(|| self.rows.len().saturating_sub(1));
        if delta_rows < 0 && cur_row_idx == 0 {
            return 0;
        }
        if delta_rows > 0 && cur_row_idx + 1 >= self.rows.len() {
            return text.len();
        }
        let target_idx = cur_row_idx.saturating_add_signed(delta_rows).min(self.rows.len() - 1);
        let cur_row = &self.rows[cur_row_idx];
        let x = cur_row
            .line
            .x_for_index(clamped.saturating_sub(cur_row.start_byte));
        let target_row = &self.rows[target_idx];
        let rel = target_row.line.closest_index_for_x(x);
        (target_row.start_byte + rel)
            .min(target_row.end_byte)
            .min(text.len())
    }

    pub(crate) fn bounds_for_utf8_range(&self, range: Range<usize>) -> Option<Bounds<Pixels>> {
        let bounds = self.bounds?;
        let lh = f32::from(self.line_height).max(12.0);
        if self.rows.is_empty() {
            return Some(Bounds::new(
                point(bounds.left() - px(self.scroll_x), bounds.top() - px(self.scroll_y)),
                size(px(2.0), px(lh)),
            ));
        }
        let start = range.start.min(range.end);
        let end = range.start.max(range.end);
        let row_idx = self
            .rows
            .iter()
            .position(|r| start >= r.start_byte && start <= r.end_byte)
            .unwrap_or_else(|| self.rows.len() - 1);
        let row = &self.rows[row_idx];
        let x0 = f32::from(row.line.x_for_index(start.saturating_sub(row.start_byte)));
        let end_in_row = end.min(row.end_byte).saturating_sub(row.start_byte);
        let x1 = f32::from(row.line.x_for_index(end_in_row));
        let y0 = f32::from(bounds.top()) + (row_idx as f32) * lh - self.scroll_y;
        let left = f32::from(bounds.left()) + x0 - self.scroll_x;
        let width = (x1 - x0).max(2.0);
        Some(Bounds::new(point(px(left), px(y0)), size(px(width), px(lh))))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TextFieldTheme {
    pub bg_idle: u32,
    pub bg_focused: u32,
    pub bg_hover: u32,
    pub border_idle: u32,
    pub border_hover: u32,
    pub border_focus: u32,
    pub focus_ring: Hsla,
    pub text_color: Hsla,
    pub placeholder_color: Hsla,
    pub selection_bg: u32,
    pub caret_color: Hsla,
}

impl TextFieldTheme {
    pub(crate) fn example_light() -> Self {
        Self {
            bg_idle: 0xffffff,
            bg_focused: 0xffffff,
            bg_hover: 0xffffff,
            border_idle: 0xbdbdbd,
            border_hover: 0x424242,
            border_focus: 0x5e35b1,
            focus_ring: hsla(262.0 / 360.0, 0.54, 0.45, 0.24),
            text_color: rgb(0x212121).into(),
            placeholder_color: hsla(0.0, 0.0, 0.45, 1.0),
            selection_bg: 0x5e35b138,
            caret_color: rgb(0x5e35b1).into(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn shell_dark() -> Self {
        Self {
            bg_idle: 0x111827,
            bg_focused: 0x0f172a,
            bg_hover: 0x172033,
            border_idle: 0x334155,
            border_hover: 0x64748b,
            border_focus: 0x3b82f6,
            focus_ring: hsla(217.0 / 360.0, 0.91, 0.60, 0.30),
            text_color: rgb(0xf3f4f6).into(),
            placeholder_color: hsla(215.0 / 360.0, 0.16, 0.56, 1.0),
            selection_bg: 0x3b82f64d,
            caret_color: rgb(0x60a5fa).into(),
        }
    }
}

pub(crate) fn caret_blink_opacity(last_interaction_ms: u64) -> f32 {
    let elapsed = now_monotonic_ms().saturating_sub(last_interaction_ms);
    if elapsed < 500 || ((elapsed - 500) % 1060) < 530 {
        1.0
    } else {
        0.0
    }
}

fn rounded_quad(bounds: Bounds<Pixels>, color: impl Into<Background>, radius: Pixels) -> PaintQuad {
    let mut quad = fill(bounds, color);
    quad.corner_radii = Corners {
        top_left: radius,
        top_right: radius,
        bottom_right: radius,
        bottom_left: radius,
    };
    quad
}

fn split_text_rows(text: &str, multiline: bool) -> Vec<(usize, usize)> {
    if !multiline {
        return vec![(0, text.len())];
    }
    if text.is_empty() {
        return vec![(0, 0)];
    }
    let mut rows = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            rows.push((start, idx));
            start = idx + 1;
        }
    }
    rows.push((start, text.len()));
    rows
}

fn shape_row_line(
    row_text: &str,
    row_start: usize,
    marked_range: Option<&Range<usize>>,
    font_size: Pixels,
    style: &gpui::TextStyle,
    text_color: Hsla,
    window: &mut Window,
) -> ShapedLine {
    let base_run = TextRun {
        len: row_text.len(),
        font: style.font(),
        color: text_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let row_end = row_start + row_text.len();
    let runs = if let Some(marked) = marked_range {
        let ov_start = marked.start.max(row_start).min(row_end);
        let ov_end = marked.end.max(row_start).min(row_end);
        if ov_end > ov_start {
            let rel_s = ov_start - row_start;
            let rel_e = ov_end - row_start;
            vec![
                TextRun {
                    len: rel_s,
                    ..base_run.clone()
                },
                TextRun {
                    len: rel_e - rel_s,
                    underline: Some(UnderlineStyle {
                        color: Some(text_color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..base_run.clone()
                },
                TextRun {
                    len: row_text.len() - rel_e,
                    ..base_run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![base_run]
        }
    } else {
        vec![base_run]
    };
    window
        .text_system()
        .shape_line(row_text.to_owned().into(), font_size, &runs, None)
}

pub(crate) struct TextFieldCanvas<V: EntityInputHandler> {
    host: Entity<V>,
    focus_handle: FocusHandle,
    cache: Rc<RefCell<TextFieldLayoutCache>>,
    text: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    cursor_offset: usize,
    marked_range: Option<Range<usize>>,
    multiline: bool,
    focused: bool,
    pub(crate) caret_opacity: f32,
    theme: TextFieldTheme,
}

pub(crate) struct TextFieldPrepaint {
    line_height: Pixels,
    rows: Vec<ShapedRow>,
    placeholder_line: Option<ShapedLine>,
    selection_quads: Vec<PaintQuad>,
    caret_quad: Option<PaintQuad>,
    scroll_x: f32,
    scroll_y: f32,
}

impl<V: EntityInputHandler> IntoElement for TextFieldCanvas<V> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<V: EntityInputHandler> Element for TextFieldCanvas<V> {
    type RequestLayoutState = ();
    type PrepaintState = TextFieldPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = if self.multiline {
            relative(1.0).into()
        } else {
            window.line_height().into()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let lh = f32::from(line_height).max(12.0);

        let row_spans = split_text_rows(&self.text, self.multiline);
        let mut rows = Vec::with_capacity(row_spans.len());
        let mut max_line_w = 0.0_f32;
        for (start_byte, end_byte) in row_spans {
            let slice = &self.text[start_byte..end_byte];
            let shaped = shape_row_line(
                slice,
                start_byte,
                self.marked_range.as_ref(),
                font_size,
                &style,
                self.theme.text_color,
                window,
            );
            max_line_w = max_line_w.max(f32::from(shaped.width));
            rows.push(ShapedRow {
                start_byte,
                end_byte,
                line: shaped,
            });
        }

        let placeholder_line = if self.text.is_empty() && !self.placeholder.is_empty() {
            let run = TextRun {
                len: self.placeholder.len(),
                font: style.font(),
                color: self.theme.placeholder_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            Some(
                window
                    .text_system()
                    .shape_line(self.placeholder.clone(), font_size, &[run], None),
            )
        } else {
            None
        };

        let clamped_caret = self.cursor_offset.min(self.text.len());
        let caret_row_idx = rows
            .iter()
            .position(|r| clamped_caret >= r.start_byte && clamped_caret <= r.end_byte)
            .unwrap_or_else(|| rows.len().saturating_sub(1));
        let caret_row = &rows[caret_row_idx];
        let caret_x = f32::from(
            caret_row
                .line
                .x_for_index(clamped_caret.saturating_sub(caret_row.start_byte)),
        );

        let avail_w = f32::from(bounds.size.width).max(1.0);
        let avail_h = f32::from(bounds.size.height).max(lh);

        let (mut scroll_x, mut scroll_y) = {
            let prev = self.cache.borrow();
            (prev.scroll_x, prev.scroll_y)
        };

        if !self.focused {
            scroll_x = 0.0;
            if !self.multiline {
                scroll_y = 0.0;
            }
        } else {
            let max_scroll_x = (max_line_w + 6.0 - avail_w).max(0.0);
            let pad = 4.0_f32.min(avail_w * 0.25);
            if caret_x - scroll_x > avail_w - pad {
                scroll_x = caret_x - (avail_w - pad);
            } else if caret_x - scroll_x < pad {
                scroll_x = (caret_x - pad).max(0.0);
            }
            scroll_x = scroll_x.clamp(0.0, max_scroll_x);

            if self.multiline {
                let total_h = (rows.len() as f32) * lh;
                let max_scroll_y = (total_h - avail_h).max(0.0);
                let caret_top = (caret_row_idx as f32) * lh;
                let caret_bottom = caret_top + lh;
                if caret_bottom - scroll_y > avail_h {
                    scroll_y = caret_bottom - avail_h;
                } else if caret_top - scroll_y < 0.0 {
                    scroll_y = caret_top;
                }
                scroll_y = scroll_y.clamp(0.0, max_scroll_y);
            } else {
                scroll_y = 0.0;
            }
        }

        let mut selection_quads = Vec::new();
        let sel_start = self.selected_range.start.min(self.selected_range.end).min(self.text.len());
        let sel_end = self.selected_range.start.max(self.selected_range.end).min(self.text.len());
        if self.focused && sel_end > sel_start {
            for (row_idx, row) in rows.iter().enumerate() {
                let ov_s = sel_start.max(row.start_byte);
                let ov_e = sel_end.min(row.end_byte);
                let extends_newline = self.multiline && sel_end > row.end_byte && sel_start <= row.end_byte;
                if ov_e > ov_s || extends_newline {
                    let x0 = f32::from(bounds.left())
                        + f32::from(row.line.x_for_index(ov_s - row.start_byte))
                        - scroll_x;
                    let mut x1 = f32::from(bounds.left())
                        + f32::from(row.line.x_for_index(ov_e.max(ov_s) - row.start_byte))
                        - scroll_x;
                    if extends_newline && ov_e == row.end_byte {
                        x1 += 6.0;
                    }
                    let y0 = f32::from(bounds.top()) + (row_idx as f32) * lh - scroll_y;
                    let w = (x1 - x0).max(2.0);
                    selection_quads.push(rounded_quad(
                        Bounds::new(point(px(x0), px(y0 + 1.0)), size(px(w), px((lh - 2.0).max(8.0)))),
                        rgba(self.theme.selection_bg),
                        px(2.0),
                    ));
                }
            }
        }

        let caret_quad = if self.focused && self.caret_opacity > 0.02 {
            let max_x = (f32::from(bounds.right()) - 2.0).max(f32::from(bounds.left()));
            let cx_px = (f32::from(bounds.left()) + caret_x - scroll_x)
                .clamp(f32::from(bounds.left()), max_x);
            let cy_px = f32::from(bounds.top()) + (caret_row_idx as f32) * lh - scroll_y + 1.5;
            let ch_px = (lh - 3.0).max(10.0);
            Some(rounded_quad(
                Bounds::new(point(px(cx_px), px(cy_px)), size(px(2.0), px(ch_px))),
                self.theme.caret_color.opacity(self.caret_opacity),
                px(1.0),
            ))
        } else {
            None
        };

        TextFieldPrepaint {
            line_height,
            rows,
            placeholder_line,
            selection_quads,
            caret_quad,
            scroll_x,
            scroll_y,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.focused {
            window.handle_input(
                &self.focus_handle,
                ElementInputHandler::new(bounds, self.host.clone()),
                cx,
            );
        }

        let lh = f32::from(prepaint.line_height).max(12.0);
        let scroll_x = prepaint.scroll_x;
        let scroll_y = prepaint.scroll_y;
        let rows = std::mem::take(&mut prepaint.rows);
        let placeholder_line = prepaint.placeholder_line.take();
        let selection_quads = std::mem::take(&mut prepaint.selection_quads);
        let caret_quad = prepaint.caret_quad.take();

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in selection_quads {
                window.paint_quad(quad);
            }
            if let Some(ph) = placeholder_line {
                let _ = ph.paint(
                    bounds.origin,
                    prepaint.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            } else {
                for (row_idx, row) in rows.iter().enumerate() {
                    let origin = point(
                        bounds.left() - px(scroll_x),
                        bounds.top() + px((row_idx as f32) * lh - scroll_y),
                    );
                    let _ = row.line.paint(
                        origin,
                        prepaint.line_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
            if let Some(caret) = caret_quad {
                window.paint_quad(caret);
            }
        });

        let mut cache = self.cache.borrow_mut();
        cache.bounds = Some(bounds);
        cache.line_height = prepaint.line_height;
        cache.rows = rows;
        cache.scroll_x = scroll_x;
        cache.scroll_y = scroll_y;
        cache.handler_painted = self.focused;
    }
}

pub(crate) struct TextFieldRenderSpec<'a, V: EntityInputHandler> {
    pub id: &'static str,
    pub host: Entity<V>,
    pub focus_handle: FocusHandle,
    pub buffer: &'a TextBuffer,
    pub cache: Rc<RefCell<TextFieldLayoutCache>>,
    pub placeholder: &'a str,
    pub focused: bool,
    pub multiline: bool,
    pub font_size_px: f32,
    pub pad_x_px: f32,
    pub pad_y_px: f32,
    pub height_px: Option<f32>,
    pub rounded_px: f32,
    pub flex_1: bool,
    pub theme: TextFieldTheme,
}

pub(crate) fn render_text_field_box<V: EntityInputHandler>(
    spec: TextFieldRenderSpec<'_, V>,
    on_mouse_down: impl Fn(&mut V, &MouseDownEvent, &mut Window, &mut Context<V>) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let focus_handle = spec.focus_handle.clone();
    let canvas = TextFieldCanvas {
        host: spec.host,
        focus_handle: spec.focus_handle,
        cache: spec.cache,
        text: spec.buffer.text.clone(),
        placeholder: spec.placeholder.to_owned().into(),
        selected_range: spec.buffer.selected_range.clone(),
        cursor_offset: spec.buffer.cursor_offset(),
        marked_range: spec.buffer.marked_range.clone(),
        multiline: spec.multiline,
        focused: spec.focused,
        caret_opacity: caret_blink_opacity(spec.buffer.last_interaction_ms),
        theme: spec.theme,
    };

    let interaction_ms = spec.buffer.last_interaction_ms;
    let id_str = spec.id;
    let canvas_element: AnyElement = if spec.focused {
        canvas
            .with_animation(
                ElementId::Name(format!("{id_str}-caret").into()),
                Animation::new(Duration::from_millis(1060))
                    .repeat()
                    .with_max_fps(6.0),
                move |mut el, _| {
                    el.caret_opacity = caret_blink_opacity(interaction_ms);
                    el
                },
            )
            .into_any_element()
    } else {
        canvas.into_any_element()
    };

    let box_el = div()
        .id(id_str)
        .when(spec.focused, |d| d.track_focus(&focus_handle))
        .cursor(CursorStyle::IBeam)
        .overflow_hidden()
        .rounded(px(spec.rounded_px))
        .text_size(px(spec.font_size_px))
        .text_color(spec.theme.text_color)
        .bg(rgb(if spec.focused {
            spec.theme.bg_focused
        } else {
            spec.theme.bg_idle
        }))
        .when(spec.flex_1, |d| d.flex_1())
        .when(!spec.multiline, |d| d.flex().items_center())
        .when_some(spec.height_px, |d, h| d.h(px(h)))
        .when(spec.focused, |d| {
            d.border_2()
                .border_color(rgb(spec.theme.border_focus))
                .px(px((spec.pad_x_px - 1.0).max(0.0)))
                .py(px((spec.pad_y_px - 1.0).max(0.0)))
        })
        .when(!spec.focused, |d| {
            let hover_bg = spec.theme.bg_hover;
            let hover_border = spec.theme.border_hover;
            d.border_1()
                .border_color(rgb(spec.theme.border_idle))
                .px(px(spec.pad_x_px))
                .py(px(spec.pad_y_px))
                .hover(move |s| {
                    s.border_color(rgb(hover_border))
                        .bg(rgb(hover_bg))
                        .shadow(vec![
                            BoxShadow::new(px(0.0), px(1.0), hsla(0.0, 0.0, 0.0, 0.07))
                                .blur_radius(px(3.0)),
                        ])
                })
        })
        .on_mouse_down(MouseButton::Left, cx.listener(on_mouse_down))
        .child(canvas_element);

    if spec.focused {
        let ring_color = spec.theme.focus_ring;
        box_el
            .with_animation(
                ElementId::Name(format!("{id_str}-focus-ring").into()),
                Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
                move |el, t| {
                    el.shadow(vec![
                        BoxShadow::new(px(0.0), px(0.0), ring_color.opacity(t))
                            .blur_radius(px(5.0 * t))
                            .spread_radius(px(2.0 * t)),
                    ])
                },
            )
            .into_any_element()
    } else {
        box_el.into_any_element()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyEditOutcome {
    NotHandled,
    HandledChanged,
    HandledNoChange,
}

pub(crate) fn apply_editor_key_down(
    buf: &mut TextBuffer,
    cache: &TextFieldLayoutCache,
    event: &KeyDownEvent,
    multiline: bool,
    cx: &mut App,
) -> KeyEditOutcome {
    let mods = &event.keystroke.modifiers;
    let key = event.keystroke.key.as_str();

    if mods.control && !mods.alt && !mods.platform {
        match key.to_ascii_lowercase().as_str() {
            "a" => {
                buf.focus_select_all();
                return KeyEditOutcome::HandledNoChange;
            }
            "c" => {
                if !buf.selected_range.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        buf.selected_text().to_owned(),
                    ));
                }
                return KeyEditOutcome::HandledNoChange;
            }
            "x" => {
                if !buf.selected_range.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        buf.selected_text().to_owned(),
                    ));
                    buf.insert_str_filtered("", multiline);
                    return KeyEditOutcome::HandledChanged;
                }
                return KeyEditOutcome::HandledNoChange;
            }
            "v" => {
                if let Some(paste) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    buf.insert_str_filtered(&paste, multiline);
                    return KeyEditOutcome::HandledChanged;
                }
                return KeyEditOutcome::HandledNoChange;
            }
            "z" => {
                return if buf.undo() {
                    KeyEditOutcome::HandledChanged
                } else {
                    KeyEditOutcome::HandledNoChange
                };
            }
            _ => {}
        }
    }

    match key {
        "left" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            let target = if mods.control {
                buf.prev_word_boundary(buf.cursor_offset())
            } else if !mods.shift && !buf.selected_range.is_empty() {
                buf.selected_range.start
            } else {
                buf.prev_char_boundary(buf.cursor_offset())
            };
            if mods.shift {
                buf.select_to(target);
            } else {
                buf.move_to(target);
            }
            KeyEditOutcome::HandledNoChange
        }
        "right" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            let target = if mods.control {
                buf.next_word_boundary(buf.cursor_offset())
            } else if !mods.shift && !buf.selected_range.is_empty() {
                buf.selected_range.end
            } else {
                buf.next_char_boundary(buf.cursor_offset())
            };
            if mods.shift {
                buf.select_to(target);
            } else {
                buf.move_to(target);
            }
            KeyEditOutcome::HandledNoChange
        }
        "home" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            let target = if multiline && !mods.control {
                buf.line_start_boundary(buf.cursor_offset())
            } else {
                0
            };
            if mods.shift {
                buf.select_to(target);
            } else {
                buf.move_to(target);
            }
            KeyEditOutcome::HandledNoChange
        }
        "end" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            let target = if multiline && !mods.control {
                buf.line_end_boundary(buf.cursor_offset())
            } else {
                buf.text.len()
            };
            if mods.shift {
                buf.select_to(target);
            } else {
                buf.move_to(target);
            }
            KeyEditOutcome::HandledNoChange
        }
        "up" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            let target = if multiline {
                cache.vertical_move_offset(&buf.text, buf.cursor_offset(), -1)
            } else {
                0
            };
            if mods.shift {
                buf.select_to(target);
            } else {
                buf.move_to(target);
            }
            KeyEditOutcome::HandledNoChange
        }
        "down" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            let target = if multiline {
                cache.vertical_move_offset(&buf.text, buf.cursor_offset(), 1)
            } else {
                buf.text.len()
            };
            if mods.shift {
                buf.select_to(target);
            } else {
                buf.move_to(target);
            }
            KeyEditOutcome::HandledNoChange
        }
        "backspace" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            if mods.control {
                buf.delete_word_backward();
            } else {
                buf.backspace();
            }
            KeyEditOutcome::HandledChanged
        }
        "delete" => {
            if mods.alt || mods.platform {
                return KeyEditOutcome::NotHandled;
            }
            if mods.control {
                buf.delete_word_forward();
            } else {
                buf.delete_forward();
            }
            KeyEditOutcome::HandledChanged
        }
        _ => {
            if !cache.handler_painted && !mods.control && !mods.alt && !mods.platform {
                let text = event
                    .keystroke
                    .key_char
                    .as_deref()
                    .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
                    .or_else(|| (key.chars().count() == 1).then_some(key));
                if let Some(ch) = text {
                    buf.insert_str_filtered(ch, multiline);
                    return KeyEditOutcome::HandledChanged;
                }
            }
            KeyEditOutcome::NotHandled
        }
    }
}
